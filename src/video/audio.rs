//! Sound for the clip player. One output stream lives as long as the app; a feed
//! thread per play decodes the clip's audio with ffmpeg into a shared buffer.
//!
//! The video player's wall clock is the master: the output callback works out
//! where playback should be and skips samples it's behind on or plays silence
//! while it's ahead, so sound stays in sync however late ffmpeg starts.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How far audio may drift from the clock before it's corrected.
const SLACK: f64 = 0.04;
/// Decoded audio kept ahead of playback.
const AHEAD: f64 = 2.0;

#[derive(Default)]
struct State {
    /// Bumped on every play/stop; feeds from an older generation quit.
    generation: u64,
    /// Playback clock: when it started and the clip time it started from.
    clock: Option<(Instant, f64)>,
    /// Interleaved stereo.
    samples: VecDeque<f32>,
    /// Clip time of `samples.front()`.
    next_t: f64,
}

pub struct Engine {
    _stream: cpal::Stream,
    state: Arc<Mutex<State>>,
    pub muted: Arc<AtomicBool>,
    rate: u32,
}

impl Engine {
    /// The default output device, or `None` without one (playback is then silent).
    pub fn new() -> Option<Engine> {
        let device = cpal::default_host().default_output_device()?;
        let config = device.default_output_config().ok()?;
        let state = Arc::new(Mutex::new(State::default()));
        let muted = Arc::new(AtomicBool::new(false));
        let rate = config.sample_rate();
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, config.into(), &state, &muted),
            cpal::SampleFormat::I16 => build::<i16>(&device, config.into(), &state, &muted),
            cpal::SampleFormat::U16 => build::<u16>(&device, config.into(), &state, &muted),
            cpal::SampleFormat::I32 => build::<i32>(&device, config.into(), &state, &muted),
            _ => None,
        }?;
        stream.play().ok()?;
        Some(Engine { _stream: stream, state, muted, rate })
    }

    /// Start sound for `path` from clip time `from`, on the player's clock.
    pub fn play(&self, path: PathBuf, from: f64, clock: Instant) {
        let generation = {
            let mut st = self.state.lock().unwrap();
            st.generation += 1;
            st.samples.clear();
            st.next_t = from;
            st.clock = Some((clock, from));
            st.generation
        };
        let (state, rate) = (self.state.clone(), self.rate);
        std::thread::spawn(move || feed(state, generation, path, from, rate));
    }

    pub fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        st.generation += 1;
        st.clock = None;
        st.samples.clear();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, state: &Arc<Mutex<State>>, muted: &Arc<AtomicBool>) -> Option<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let (state, muted) = (state.clone(), muted.clone());
    let channels = config.channels as usize;
    let rate = config.sample_rate as f64;
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                let frames = out.len() / channels;
                let mut written = 0;
                let mut st = state.lock().unwrap();
                if let Some((start, from)) = st.clock {
                    let now = from + start.elapsed().as_secs_f64();
                    let behind = now - st.next_t;
                    if behind > SLACK {
                        let skip = ((behind * rate) as usize).min(st.samples.len() / 2);
                        st.samples.drain(..skip * 2);
                        st.next_t += skip as f64 / rate;
                    }
                    if st.next_t <= now + SLACK {
                        let mute = muted.load(Ordering::Relaxed);
                        while written < frames && st.samples.len() >= 2 {
                            let (l, r) = (st.samples.pop_front().unwrap(), st.samples.pop_front().unwrap());
                            let (l, r) = if mute { (0.0, 0.0) } else { (l, r) };
                            let frame = &mut out[written * channels..][..channels];
                            match channels {
                                1 => frame[0] = T::from_sample((l + r) * 0.5),
                                _ => {
                                    frame[0] = T::from_sample(l);
                                    frame[1] = T::from_sample(r);
                                    frame[2..].fill(T::EQUILIBRIUM);
                                }
                            }
                            written += 1;
                        }
                        st.next_t += written as f64 / rate;
                    }
                }
                drop(st);
                out[written * channels..].fill(T::EQUILIBRIUM);
            },
            |e| eprintln!("audio: {e}"),
            None,
        )
        .ok()
}

/// Decode the clip's first audio track from `from` into the shared buffer until a
/// newer play or a stop makes this generation stale.
fn feed(state: Arc<Mutex<State>>, generation: u64, path: PathBuf, from: f64, rate: u32) {
    let child = super::ffmpeg()
        .args(["-ss", &format!("{from:.3}"), "-i"])
        .arg(&path)
        .args(["-map", "0:a:0", "-vn", "-f", "f32le", "-ac", "2", "-ar", &rate.to_string(), "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let mut out = child.stdout.take().unwrap();
    let mut buf = vec![0u8; 16 * 1024];
    let mut carry = Vec::new();
    'read: while let Ok(n) = out.read(&mut buf) {
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() / 4 * 4;
        let samples: Vec<f32> = carry[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        carry.drain(..whole);
        loop {
            let mut st = state.lock().unwrap();
            if st.generation != generation {
                break 'read;
            }
            if (st.samples.len() as f64) < AHEAD * rate as f64 * 2.0 {
                st.samples.extend(&samples);
                break;
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
