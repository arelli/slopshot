//! Clip playback for the large view.
//!
//! An ffmpeg process decodes from the current position into scaled RGB frames on a
//! pipe. The pipe is the pacing: the reader thread can only run a few frames ahead,
//! so a paused clip keeps its decoder parked at the next frame (play and frame-step
//! continue instantly), and seeking starts a fresh process at the new time.
//!
//! Seeking costs a process start plus decoding from the previous keyframe, about
//! half a second for 4K. To keep scrubbing responsive a second, cheap pass decodes
//! only the keyframes at thumbnail size; while the exact frame is on its way, the
//! nearest keyframe stands in.

use super::VideoMeta;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest side of the frames decoded for preview.
pub const PREVIEW_MAX: u32 = 1280;
/// Longest side of the scrub cache's keyframes.
const KEYFRAME_MAX: u32 = 384;

pub struct Frame {
    /// Clip time, seconds.
    pub pts: f64,
    pub rgb: Arc<Vec<u8>>,
    pub size: [usize; 2],
}

/// One ffmpeg decoding from `start`.
struct Decoder {
    child: Child,
    rx: Receiver<Frame>,
    start: f64,
    hw: bool,
    started_at: Instant,
    /// A frame has come out (seek finished).
    delivered: bool,
}

impl Decoder {
    fn spawn(path: &Path, meta: &VideoMeta, start: f64, size: (u32, u32)) -> Option<Decoder> {
        let hw = super::hwaccel();
        let mut child = super::ffmpeg()
            .args(hw)
            .args(["-ss", &format!("{start:.4}"), "-i"])
            .arg(path)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", &format!("scale={}:{}:flags=bilinear", size.0, size.1)])
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let (tx, rx) = sync_channel(3);
        let (frame_t, px) = (meta.frame_time(), [size.0 as usize, size.1 as usize]);
        std::thread::spawn(move || {
            for n in 0.. {
                let mut buf = vec![0u8; px[0] * px[1] * 3];
                if !super::read_frame(&mut out, &mut buf) {
                    return;
                }
                let frame = Frame { pts: start + n as f64 * frame_t, rgb: Arc::new(buf), size: px };
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        Some(Decoder { child, rx, start, hw: !hw.is_empty(), started_at: Instant::now(), delivered: false })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Keyframes = Arc<Mutex<Vec<(f64, Arc<Vec<u8>>)>>>;

pub struct Player {
    pub path: PathBuf,
    pub meta: VideoMeta,
    size: (u32, u32),
    kf_size: (u32, u32),
    /// Position while paused, and where the clock started from while it runs.
    pos: f64,
    /// Play was pressed. The clock itself only runs once frames are coming, so
    /// a seek while playing doesn't race ahead of its first frame.
    want_play: bool,
    clock: Option<Instant>,
    /// Bumped whenever the clock starts or stops, so the sound can follow.
    pub epoch: u64,
    dec: Option<Decoder>,
    /// Next decoded frame, not yet due.
    ahead: Option<Frame>,
    /// Where to seek once the decoder in flight has delivered (coalesces scrubbing).
    pending: Option<f64>,
    /// The frame on screen; `fresh` once it's new since the last `take_new`.
    pub frame: Option<Frame>,
    fresh: bool,
    keyframes: Keyframes,
    kf_child: Arc<Mutex<Option<Child>>>,
    /// The decoder hit the end of the clip.
    ended: bool,
}

impl Player {
    pub fn new(path: PathBuf, meta: VideoMeta, at: f64) -> Player {
        let size = super::fit_even(meta.width, meta.height, PREVIEW_MAX);
        let kf_size = super::fit_even(meta.width, meta.height, KEYFRAME_MAX);
        let mut p = Player {
            path,
            meta,
            size,
            kf_size,
            pos: 0.0,
            want_play: false,
            clock: None,
            epoch: 0,
            dec: None,
            ahead: None,
            pending: None,
            frame: None,
            fresh: false,
            keyframes: Arc::default(),
            kf_child: Arc::default(),
            ended: false,
        };
        p.start_keyframes();
        p.restart(at);
        p
    }

    pub fn duration(&self) -> f64 {
        self.meta.duration
    }

    pub fn playing(&self) -> bool {
        self.want_play
    }

    /// Current playback position.
    pub fn position(&self) -> f64 {
        match self.clock {
            Some(c) => (self.pos + c.elapsed().as_secs_f64()).min(self.duration()),
            None => self.pos,
        }
    }

    /// The running clock, for the sound to follow: (start instant, clip time then).
    pub fn clock(&self) -> Option<(Instant, f64)> {
        self.clock.map(|c| (c, self.pos))
    }

    fn start_clock(&mut self) {
        self.clock = Some(Instant::now());
        self.epoch += 1;
    }

    fn stop_clock(&mut self) {
        if self.clock.is_some() {
            self.pos = self.position();
            self.clock = None;
            self.epoch += 1;
        }
    }

    fn restart(&mut self, t: f64) {
        let ft = self.meta.frame_time();
        let t = t.clamp(0.0, (self.duration() - ft).max(0.0));
        // Snap to the frame grid so frame numbers and timestamps line up.
        let t = (t / ft).round() * ft;
        self.stop_clock();
        self.dec = None;
        self.ahead = None;
        self.pending = None;
        self.ended = false;
        self.pos = t;
        self.dec = Decoder::spawn(&self.path, &self.meta, t, self.size);
    }

    /// Jump to `t`. While a seek is still decoding, only the latest target is kept.
    pub fn seek(&mut self, t: f64) {
        self.stop_clock();
        match &self.dec {
            Some(d) if !d.delivered && d.started_at.elapsed() < Duration::from_secs(2) => {
                self.pending = Some(t);
                self.pos = t;
            }
            _ => self.restart(t),
        }
    }

    pub fn play(&mut self) {
        if self.want_play {
            return;
        }
        self.want_play = true;
        if self.pos >= self.duration() - self.meta.frame_time() * 1.5 {
            self.restart(0.0);
        } else if self.dec.is_none() || self.ended {
            self.restart(self.pos);
        }
        if self.dec.as_ref().is_some_and(|d| d.delivered) && self.pending.is_none() {
            self.start_clock(); // parked right after the frame on screen
        }
    }

    pub fn pause(&mut self) {
        self.want_play = false;
        self.stop_clock();
        // The frame on screen is the position; the decoder stays parked after it.
        if let (Some(f), None) = (&self.frame, self.pending) {
            self.pos = f.pts;
        }
    }

    pub fn toggle(&mut self) {
        if self.want_play { self.pause() } else { self.play() }
    }

    /// Frames forward (from the parked decoder) or back (by seeking).
    pub fn step(&mut self, frames: i32) {
        self.pause();
        if frames < 0 {
            self.seek(self.pos + frames as f64 * self.meta.frame_time());
            return;
        }
        for _ in 0..frames {
            match self.next_frame(Duration::from_secs(3)) {
                Some(f) => {
                    self.pos = f.pts;
                    self.show(f);
                }
                None => break,
            }
        }
    }

    fn next_frame(&mut self, wait: Duration) -> Option<Frame> {
        if let Some(f) = self.ahead.take() {
            return Some(f);
        }
        let d = self.dec.as_mut()?;
        match d.rx.recv_timeout(wait) {
            Ok(f) => {
                d.delivered = true;
                Some(f)
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                None
            }
            Err(RecvTimeoutError::Timeout) => None,
        }
    }

    fn show(&mut self, f: Frame) {
        self.frame = Some(f);
        self.fresh = true;
    }

    /// The new frame to display, if one arrived since the last call.
    pub fn take_new(&mut self) -> Option<&Frame> {
        std::mem::take(&mut self.fresh).then_some(self.frame.as_ref()).flatten()
    }

    /// A seek is still on its way.
    pub fn seeking(&self) -> bool {
        self.pending.is_some() || self.dec.as_ref().is_some_and(|d| !d.delivered)
    }

    /// Nearest cached keyframe at or before `t`, for showing while scrubbing.
    pub fn keyframe_near(&self, t: f64) -> Option<Frame> {
        let kf = self.keyframes.lock().unwrap();
        let i = kf.partition_point(|(pts, _)| *pts <= t + 1e-3).checked_sub(1)?;
        let (pts, rgb) = &kf[i];
        Some(Frame { pts: *pts, rgb: rgb.clone(), size: [self.kf_size.0 as usize, self.kf_size.1 as usize] })
    }

    /// Advance playback. Returns how soon the UI should check again, `None` when idle.
    pub fn tick(&mut self) -> Option<Duration> {
        // A finished seek: show its first frame, then go on to a newer target.
        let duration = self.duration();
        if let Some(d) = &mut self.dec
            && !d.delivered
        {
            match d.rx.try_recv() {
                Ok(f) => {
                    d.delivered = true;
                    self.pos = f.pts;
                    self.show(f);
                    if let Some(t) = self.pending.take() {
                        self.restart(t);
                        return Some(Duration::from_millis(15));
                    }
                    if self.want_play {
                        self.start_clock();
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    // No frame at all: a broken hardware decoder is the usual cause.
                    if d.hw && d.start < duration - 1.0 {
                        super::hw_decode_failed();
                        let t = d.start;
                        self.restart(t);
                        return Some(Duration::from_millis(15));
                    }
                    self.ended = true;
                    self.want_play = false;
                    self.pending = None;
                    return None;
                }
                Err(TryRecvError::Empty) => return Some(Duration::from_millis(15)),
            }
        }
        let clock = self.clock?;
        let now = self.pos + clock.elapsed().as_secs_f64();
        let mut due = None;
        loop {
            let f = match self.ahead.take() {
                Some(f) => f,
                None => match self.dec.as_mut().map(|d| d.rx.try_recv()) {
                    Some(Ok(f)) => f,
                    Some(Err(TryRecvError::Empty)) => break,
                    Some(Err(TryRecvError::Disconnected)) | None => {
                        self.ended = true;
                        break;
                    }
                },
            };
            if f.pts <= now + 0.002 {
                due = Some(f); // late frames are skipped in favour of the newest due one
            } else {
                self.ahead = Some(f);
                break;
            }
        }
        if let Some(f) = due {
            self.show(f);
        }
        if self.ended && self.ahead.is_none() {
            self.pause();
            return None;
        }
        let wait = self.ahead.as_ref().map_or(0.005, |f| (f.pts - now).max(0.0));
        Some(Duration::from_secs_f64(wait.min(0.05)))
    }

    /// Decode every keyframe at thumbnail size in the background (`-skip_frame nokey`
    /// skips everything else, so this is quick even for 4K). Frame times come from
    /// `showinfo` on stderr.
    fn start_keyframes(&mut self) {
        let (w, h) = self.kf_size;
        let child = super::ffmpeg()
            .args(["-loglevel", "info", "-threads", "2", "-skip_frame", "nokey", "-i"])
            .arg(&self.path)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough"])
            .args(["-vf", &format!("scale={w}:{h}:flags=bilinear,showinfo"), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else { return };
        let (Some(mut out), Some(err)) = (child.stdout.take(), child.stderr.take()) else { return };
        *self.kf_child.lock().unwrap() = Some(child);
        let times = Arc::new(Mutex::new(Vec::new()));
        let t2 = times.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if let Some(t) = line.split("pts_time:").nth(1).and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok()) {
                    t2.lock().unwrap().push(t);
                }
            }
        });
        let (kf, kf_child) = (self.keyframes.clone(), self.kf_child.clone());
        std::thread::spawn(move || {
            for n in 0.. {
                let mut buf = vec![0u8; (w * h * 3) as usize];
                if !super::read_frame(&mut out, &mut buf) {
                    break;
                }
                // showinfo logs a frame before it's written, but give stderr a moment.
                let mut t = None;
                for _ in 0..50 {
                    t = times.lock().unwrap().get(n).copied();
                    if t.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                let Some(t) = t else { break };
                let mut kf = kf.lock().unwrap();
                let at = kf.partition_point(|(pts, _)| *pts < t);
                kf.insert(at, (t, Arc::new(buf)));
            }
            if let Some(mut c) = kf_child.lock().unwrap().take() {
                let _ = c.wait();
            }
        });
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(mut c) = self.kf_child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
