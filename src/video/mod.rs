//! Video clips: culled in the same grid as the photos, played, trimmed, color
//! graded and compressed on export. Decoding and encoding go through the `ffmpeg`
//! and `ffprobe` executables (on PATH, or `SLOPSHOT_FFMPEG` pointing at ffmpeg),
//! so there's no codec library to build and the GPU encoders ffmpeg ships with
//! come for free.

pub mod audio;
pub mod export;
pub mod grade;
pub mod player;
pub mod probe;
pub mod ui;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

pub use grade::Grade;
pub use probe::VideoMeta;

/// What slopshot opens as video. Camera clips: Sony XAVC/Canon/DJI `.MP4`, `.MOV`,
/// AVCHD `.MTS`.
const EXTENSIONS: [&str; 5] = ["mp4", "mov", "m4v", "mts", "m2ts"];

pub fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// A clip's trim and color, stored in the sidecar next to its rating.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Edit {
    /// Start of the kept part, in seconds from the start of the clip.
    pub trim_in: f64,
    /// End of the kept part; `None` keeps everything to the end.
    pub trim_out: Option<f64>,
    #[serde(skip_serializing_if = "Grade::is_identity")]
    pub grade: Grade,
}

impl Edit {
    pub const NONE: Edit = Edit { trim_in: 0.0, trim_out: None, grade: Grade::NONE };

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }
    pub fn trimmed(&self) -> bool {
        self.trim_in > 0.0 || self.trim_out.is_some()
    }
    /// The kept range in a clip of length `duration`.
    pub fn range(&self, duration: f64) -> (f64, f64) {
        let end = self.trim_out.unwrap_or(duration).min(duration);
        (self.trim_in.min(end), end)
    }
}

impl Default for Edit {
    fn default() -> Self {
        Self::NONE
    }
}

fn tool(name: &str) -> PathBuf {
    match std::env::var_os("SLOPSHOT_FFMPEG").map(PathBuf::from) {
        Some(ff) if name == "ffmpeg" => ff,
        Some(ff) => ff.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        None => PathBuf::from(name),
    }
}

/// `ffmpeg`/`ffprobe` with stdin closed and, on Windows, no console window
/// flashing up for every thumbnail.
pub fn command(name: &str) -> Command {
    let mut c = Command::new(tool(name));
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

pub fn ffmpeg() -> Command {
    let mut c = command("ffmpeg");
    c.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    c
}

/// Set once a hardware decode failed, so later clips go straight to software.
static HW_DECODE_BROKEN: AtomicBool = AtomicBool::new(false);

/// Input options for GPU decoding (frames come back to system memory, so the
/// filters after it don't care). D3D11VA beat both `auto` (which picks the older
/// DXVA2) and software on a 4K drone clip; elsewhere ffmpeg picks.
pub fn hwaccel() -> &'static [&'static str] {
    if HW_DECODE_BROKEN.load(Ordering::Relaxed) {
        &[]
    } else if cfg!(windows) {
        &["-hwaccel", "d3d11va"]
    } else if cfg!(target_os = "macos") {
        &["-hwaccel", "videotoolbox"]
    } else {
        &["-hwaccel", "auto"]
    }
}

pub fn hw_decode_failed() {
    HW_DECODE_BROKEN.store(true, Ordering::Relaxed);
}

/// Whether ffmpeg can be run at all.
pub fn ffmpeg_available() -> bool {
    command("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Even dimensions no bigger than `max` (long side), keeping the aspect ratio.
/// Encoders and YUV 4:2:0 want even sizes.
pub fn fit_even(w: u32, h: u32, max: u32) -> (u32, u32) {
    let s = (max as f64 / w.max(h).max(1) as f64).min(1.0);
    let even = |v: f64| (((v + 1e-6) / 2.0).floor() as u32 * 2).max(2);
    (even(w as f64 * s), even(h as f64 * s))
}

/// Read exactly `buf.len()` bytes, or report a clean end of stream.
pub fn read_frame(r: &mut impl std::io::Read, buf: &mut [u8]) -> bool {
    r.read_exact(buf).is_ok()
}

/// A middle-of-the-clip frame for the grid (a few seconds in skips the black
/// first frames many cameras record), plus the clip's metadata.
pub fn thumbnail(path: &Path) -> (Option<image::RgbImage>, Option<VideoMeta>) {
    let Some(meta) = probe::probe(path) else { return (None, None) };
    let (w, h) = fit_even(meta.width, meta.height, 480);
    let at = (meta.duration * 0.1).min(2.0);
    let out = ffmpeg()
        .args(["-threads", "2", "-ss", &format!("{at:.3}"), "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stderr(Stdio::null())
        .output();
    let img = out
        .ok()
        .and_then(|o| image::RgbImage::from_raw(w, h, o.stdout.get(..(w * h * 3) as usize)?.to_vec()));
    (img, Some(meta))
}

/// `m:ss.cc`, or `h:mm:ss` for anything an hour or longer.
pub fn timecode(t: f64) -> String {
    let t = t.max(0.0);
    let (h, m, s) = ((t / 3600.0) as u64, ((t % 3600.0) / 60.0) as u64, t % 60.0);
    if h > 0 { format!("{h}:{m:02}:{:02}", s as u64) } else { format!("{m}:{s:05.2}") }
}

/// `0:32`, for card labels.
pub fn short_duration(t: f64) -> String {
    let t = t.round() as u64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_extensions() {
        for f in ["C0001.MP4", "clip.mov", "00001.MTS", "a.m4v"] {
            assert!(is_video(Path::new(f)), "{f}");
        }
        for f in ["DSC1.ARW", "x.cr3", "mp4", "a.mp4.txt"] {
            assert!(!is_video(Path::new(f)), "{f}");
        }
    }

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit_even(3840, 2160, 480), (480, 270));
        assert_eq!(fit_even(1080, 1920, 480), (270, 480));
        assert_eq!(fit_even(640, 360, 1280), (640, 360)); // never upscales
        assert_eq!(fit_even(1440, 1080, 1281), (1280, 960)); // rounds down to even
    }

    #[test]
    fn timecodes() {
        assert_eq!(timecode(0.0), "0:00.00");
        assert_eq!(timecode(75.456), "1:15.46");
        assert_eq!(timecode(3725.0), "1:02:05");
        assert_eq!(short_duration(32.0), "0:32");
        assert_eq!(short_duration(3725.4), "1:02:05");
    }

    #[test]
    fn edit_range_clamps_to_the_clip() {
        let e = Edit { trim_in: 2.0, trim_out: Some(50.0), ..Edit::NONE };
        assert_eq!(e.range(30.0), (2.0, 30.0));
        assert_eq!(Edit::NONE.range(30.0), (0.0, 30.0));
        assert!(e.trimmed() && !Edit::NONE.trimmed());
    }
}
