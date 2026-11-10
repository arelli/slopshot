//! Exporting clips: trim, apply the grade and compress in one ffmpeg run, keeping
//! the capture date and GPS tags so photo libraries (Immich & co.) put the clip
//! in the right place on their timeline and map.

use super::{Edit, VideoMeta};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoder {
    HevcQsv,
    H264Qsv,
    HevcNvenc,
    H264Nvenc,
    X265,
    X264,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    High,
    Balanced,
    Small,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaxRes {
    Original,
    P2160,
    P1440,
    P1080,
    P720,
}

impl MaxRes {
    pub const ALL: [MaxRes; 5] = [MaxRes::Original, MaxRes::P2160, MaxRes::P1440, MaxRes::P1080, MaxRes::P720];
    /// Cap on the short side (1080 for 1920×1080 and for 1080×1920 alike).
    fn short_side(self) -> Option<u32> {
        match self {
            MaxRes::Original => None,
            MaxRes::P2160 => Some(2160),
            MaxRes::P1440 => Some(1440),
            MaxRes::P1080 => Some(1080),
            MaxRes::P720 => Some(720),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MaxRes::Original => "Original size",
            MaxRes::P2160 => "4K (2160p)",
            MaxRes::P1440 => "1440p",
            MaxRes::P1080 => "1080p",
            MaxRes::P720 => "720p",
        }
    }
}

impl Encoder {
    /// Preference order: GPU HEVC first (fast and small), CPU encoders last.
    pub const ALL: [Encoder; 6] = [Encoder::HevcQsv, Encoder::HevcNvenc, Encoder::X265, Encoder::H264Qsv, Encoder::H264Nvenc, Encoder::X264];

    pub fn codec(self) -> &'static str {
        match self {
            Encoder::HevcQsv => "hevc_qsv",
            Encoder::H264Qsv => "h264_qsv",
            Encoder::HevcNvenc => "hevc_nvenc",
            Encoder::H264Nvenc => "h264_nvenc",
            Encoder::X265 => "libx265",
            Encoder::X264 => "libx264",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Encoder::HevcQsv => "HEVC · Intel GPU",
            Encoder::H264Qsv => "H.264 · Intel GPU",
            Encoder::HevcNvenc => "HEVC · NVIDIA GPU",
            Encoder::H264Nvenc => "H.264 · NVIDIA GPU",
            Encoder::X265 => "HEVC · CPU (x265, slow)",
            Encoder::X264 => "H.264 · CPU (x264)",
        }
    }
    fn hevc(self) -> bool {
        matches!(self, Encoder::HevcQsv | Encoder::HevcNvenc | Encoder::X265)
    }
    fn gpu(self) -> bool {
        !matches!(self, Encoder::X265 | Encoder::X264)
    }
    /// Pixel format fed to the encoder. 10-bit sources stay 10-bit with HEVC.
    fn pix_fmt(self, ten_bit: bool) -> &'static str {
        match (self.gpu(), ten_bit && self.hevc()) {
            (true, true) => "p010le",
            (true, false) => "nv12",
            (false, true) => "yuv420p10le",
            (false, false) => "yuv420p",
        }
    }
    /// Rate control: constant quality, with values picked so Balanced lands near
    /// a HandBrake "Fast 1080p" file for H.264 and around half that for HEVC.
    fn quality_args(self, q: Quality) -> Vec<String> {
        let pick = |v: [u8; 3]| match q {
            Quality::High => v[0],
            Quality::Balanced => v[1],
            Quality::Small => v[2],
        };
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Encoder::HevcQsv => [s(&["-preset", "medium", "-global_quality"]), vec![pick([25, 28, 31]).to_string()]].concat(),
            Encoder::H264Qsv => [s(&["-preset", "medium", "-global_quality"]), vec![pick([22, 25, 28]).to_string()]].concat(),
            Encoder::HevcNvenc => [s(&["-preset", "p5", "-rc", "vbr", "-b:v", "0", "-cq"]), vec![pick([24, 27, 30]).to_string()]].concat(),
            Encoder::H264Nvenc => [s(&["-preset", "p5", "-rc", "vbr", "-b:v", "0", "-cq"]), vec![pick([21, 24, 27]).to_string()]].concat(),
            Encoder::X265 => [s(&["-preset", "fast", "-crf"]), vec![pick([22, 25, 28]).to_string()]].concat(),
            Encoder::X264 => [s(&["-preset", "faster", "-crf"]), vec![pick([20, 23, 26]).to_string()]].concat(),
        }
    }
}

/// The encoders this ffmpeg build and machine can actually run, in preference
/// order. Each gets a 3-frame test encode; a GPU encoder that's compiled in but has
/// no hardware behind it fails here rather than halfway through an export.
pub fn detect() -> Vec<Encoder> {
    Encoder::ALL
        .into_iter()
        .filter(|e| {
            super::ffmpeg()
                .args(["-f", "lavfi", "-i", "color=black:s=320x240:r=30", "-frames:v", "3", "-pix_fmt", e.pix_fmt(false), "-c:v", e.codec()])
                .args(["-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    pub encoder: Encoder,
    pub quality: Quality,
    pub max_res: MaxRes,
    /// Off: copy the streams as they are (trim snaps to keyframes, no color).
    pub reencode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { encoder: Encoder::HevcQsv, quality: Quality::Balanced, max_res: MaxRes::P1080, reencode: true }
    }
}

pub struct Job {
    pub src: PathBuf,
    pub meta: VideoMeta,
    pub edit: Edit,
}

/// Output frame size: capped on the short side, never upscaled.
fn out_size(meta: &VideoMeta, max: MaxRes) -> Option<(u32, u32)> {
    let cap = max.short_side()?;
    let short = meta.width.min(meta.height);
    (short > cap).then(|| super::fit_even(meta.width, meta.height, (meta.width.max(meta.height) as u64 * cap as u64 / short as u64) as u32))
}

/// Where the export of `rel` goes: same relative path, `.mp4` when re-encoding.
pub fn target(dest: &Path, rel: &str, s: &Settings) -> PathBuf {
    let t = dest.join(rel);
    if s.reencode { t.with_extension("mp4") } else { t }
}

/// When the kept part started recording.
fn start_time(meta: &VideoMeta, edit: &Edit) -> Option<chrono::DateTime<chrono::Utc>> {
    meta.created.map(|c| c + chrono::Duration::milliseconds((edit.trim_in * 1000.0) as i64))
}

/// ffmpeg arguments for one clip, writing to `out`. A grade is read from
/// `grade.cube` in ffmpeg's working directory (a relative name dodges the
/// filtergraph escaping a Windows path would need).
pub fn args(job: &Job, s: &Settings, out: &Path) -> Vec<String> {
    let (start, end) = job.edit.range(job.meta.duration);
    let mut a: Vec<String> = vec!["-y".into()];
    let graded = s.reencode && !job.edit.grade.is_identity() && !job.meta.hdr;
    if s.reencode {
        a.extend(super::hwaccel().iter().map(|s| s.to_string()));
    }
    if start > 0.0 {
        a.extend(["-ss".into(), format!("{start:.3}")]);
    }
    a.extend(["-i".into(), job.src.to_string_lossy().into_owned()]);
    if job.edit.trimmed() {
        a.extend(["-t".into(), format!("{:.3}", end - start)]);
    }
    a.extend(["-map", "0:v:0", "-map", "0:a?", "-map_metadata", "0"].map(String::from));
    if s.reencode {
        let ten = job.meta.bit_depth > 8;
        let pix = s.encoder.pix_fmt(ten);
        let size = out_size(&job.meta, s.max_res).map_or(String::new(), |(w, h)| format!("{w}:{h}:"));
        let scale = |extra: &str| if size.is_empty() && extra.is_empty() { String::new() } else { format!("scale={size}{extra}flags=bicubic,") };
        let vf = if graded {
            let rgb = if ten { "gbrp10le" } else { "gbrp" };
            format!("{}format={rgb},lut3d=file=grade.cube:interp=tetrahedral,scale=out_color_matrix=bt709:out_range=tv,format={pix}", scale(""))
        } else if job.meta.hdr {
            format!("{}format={pix}", scale(""))
        } else {
            format!("{}format={pix}", scale("out_color_matrix=bt709:out_range=tv:"))
        };
        a.extend(["-vf".into(), vf, "-c:v".into(), s.encoder.codec().into()]);
        a.extend(s.encoder.quality_args(s.quality));
        if s.encoder.hevc() {
            a.extend(["-tag:v", "hvc1"].map(String::from)); // what Apple players and browsers look for
        }
        match (&job.meta.color, job.meta.hdr) {
            (Some((p, t, c)), true) => a.extend(["-color_primaries".into(), p.clone(), "-color_trc".into(), t.clone(), "-colorspace".into(), c.clone()]),
            _ => a.extend(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"].map(String::from)),
        }
        a.extend(["-c:a", "aac", "-b:a", "192k"].map(String::from));
    } else {
        a.extend(["-c", "copy", "-avoid_negative_ts", "make_zero"].map(String::from));
    }
    if let Some(t) = start_time(&job.meta, &job.edit) {
        a.extend(["-metadata".into(), format!("creation_time={}", t.format("%Y-%m-%dT%H:%M:%S%.3fZ"))]);
    }
    a.extend(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"].map(String::from));
    a.push(out.to_string_lossy().into_owned());
    a
}

pub enum Outcome {
    Done,
    Skipped,
    Cancelled,
}

/// Export one clip to `out`, reporting progress in thousandths. Writes to a
/// temporary name first, so a cancelled or failed export leaves nothing behind
/// that a later run would skip as "already present".
pub fn run(job: &Job, s: &Settings, out: &Path, progress: &AtomicU32, cancel: &AtomicBool) -> Result<Outcome, String> {
    if out.exists() {
        return Ok(Outcome::Skipped);
    }
    let dir = out.parent().ok_or("bad destination")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = out.with_extension(format!("slopshot-part.{}", out.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
    let work = std::env::temp_dir().join(format!("slopshot-export-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;

    let result = if !s.reencode && !job.edit.trimmed() {
        // Nothing to cut: a plain copy.
        std::fs::copy(&job.src, &tmp).map(|_| Outcome::Done).map_err(|e| e.to_string())
    } else {
        if s.reencode && !job.edit.grade.is_identity() {
            std::fs::write(work.join("grade.cube"), job.edit.grade.cube(33)).map_err(|e| e.to_string())?;
        }
        encode(job, s, &tmp, &work, progress, cancel)
    };
    match result {
        Ok(Outcome::Done) => {
            std::fs::rename(&tmp, out).map_err(|e| e.to_string())?;
            // Libraries fall back to the file date; give it the recording's.
            let when = start_time(&job.meta, &job.edit)
                .map(std::time::SystemTime::from)
                .or_else(|| std::fs::metadata(&job.src).and_then(|m| m.modified()).ok());
            if let (Some(t), Ok(f)) = (when, std::fs::File::options().write(true).open(out)) {
                let _ = f.set_modified(t);
            }
            Ok(Outcome::Done)
        }
        other => {
            let _ = std::fs::remove_file(&tmp);
            other
        }
    }
}

fn encode(job: &Job, s: &Settings, tmp: &Path, work: &Path, progress: &AtomicU32, cancel: &AtomicBool) -> Result<Outcome, String> {
    let (start, end) = job.edit.range(job.meta.duration);
    let length = (end - start).max(0.001);
    let mut child = super::ffmpeg()
        .args(args(job, s, tmp))
        .current_dir(work)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run ffmpeg: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut lines: Vec<String> = BufReader::new(stderr).lines().map_while(Result::ok).collect();
        lines.retain(|l| !l.trim().is_empty());
        lines.into_iter().last().unwrap_or_default()
    });
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Cancelled);
        }
        if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
            progress.store(((us / 1e6 / length).clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let err = errors.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Ok(Outcome::Cancelled);
    }
    if status.success() { Ok(Outcome::Done) } else { Err(if err.is_empty() { format!("ffmpeg failed ({status})") } else { err }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::Grade;

    fn drone() -> VideoMeta {
        VideoMeta {
            duration: 32.0,
            width: 3840,
            height: 2160,
            fps: 29.97,
            codec: "H.264".into(),
            bit_depth: 8,
            created: chrono::DateTime::parse_from_rfc3339("2026-05-13T10:17:52Z").ok().map(|t| t.to_utc()),
            ..Default::default()
        }
    }

    fn arg_after<'a>(a: &'a [String], flag: &str) -> Option<&'a str> {
        a.iter().position(|x| x == flag).map(|i| a[i + 1].as_str())
    }

    #[test]
    fn plain_compress_scales_to_1080p_and_keeps_the_date() {
        let job = Job { src: "C:/clips/DJI_0716.MP4".into(), meta: drone(), edit: Edit::NONE };
        let a = args(&job, &Settings::default(), Path::new("out.mp4"));
        assert_eq!(arg_after(&a, "-vf"), Some("scale=1920:1080:out_color_matrix=bt709:out_range=tv:flags=bicubic,format=nv12"));
        assert_eq!(arg_after(&a, "-c:v"), Some("hevc_qsv"));
        assert_eq!(arg_after(&a, "-global_quality"), Some("28"));
        assert_eq!(arg_after(&a, "-metadata"), Some("creation_time=2026-05-13T10:17:52.000Z"));
        assert_eq!(arg_after(&a, "-map_metadata"), Some("0"));
        assert!(!a.contains(&"-ss".to_string()) && !a.contains(&"-t".to_string()));
        assert_eq!(a.last().unwrap(), "out.mp4");
    }

    #[test]
    fn trimmed_graded_clip_seeks_and_uses_the_lut() {
        let edit = Edit { trim_in: 4.5, trim_out: Some(10.0), grade: Grade { exposure: 0.5, ..Grade::NONE } };
        let job = Job { src: "a.mp4".into(), meta: drone(), edit };
        let s = Settings { encoder: Encoder::X264, max_res: MaxRes::Original, ..Settings::default() };
        let a = args(&job, &s, Path::new("o.mp4"));
        assert_eq!(arg_after(&a, "-ss"), Some("4.500"));
        assert_eq!(arg_after(&a, "-t"), Some("5.500"));
        let vf = arg_after(&a, "-vf").unwrap();
        assert!(vf.starts_with("format=gbrp,lut3d=file=grade.cube"), "{vf}");
        assert!(vf.ends_with("format=yuv420p"), "{vf}");
        assert_eq!(arg_after(&a, "-crf"), Some("23"));
        // The trimmed clip starts recording 4.5 s later.
        assert_eq!(arg_after(&a, "-metadata"), Some("creation_time=2026-05-13T10:17:56.500Z"));
        // -ss goes before -i (fast seek), -t after.
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert!(pos("-ss") < pos("-i") && pos("-i") < pos("-t"));
    }

    #[test]
    fn stream_copy_ignores_grade_and_size() {
        let edit = Edit { trim_in: 1.0, trim_out: None, grade: Grade { saturation: 0.0, ..Grade::NONE } };
        let job = Job { src: "a.MOV".into(), meta: drone(), edit };
        let s = Settings { reencode: false, ..Settings::default() };
        let a = args(&job, &s, Path::new("o.MOV"));
        assert_eq!(arg_after(&a, "-c"), Some("copy"));
        assert!(arg_after(&a, "-vf").is_none() && arg_after(&a, "-c:v").is_none());
        assert_eq!(target(Path::new("/d"), "sub/a.MOV", &s), Path::new("/d/sub/a.MOV"));
        assert_eq!(target(Path::new("/d"), "sub/a.MOV", &Settings::default()), Path::new("/d/sub/a.mp4"));
    }

    #[test]
    fn sizes_cap_the_short_side_and_never_upscale() {
        let mut m = drone();
        assert_eq!(out_size(&m, MaxRes::P1080), Some((1920, 1080)));
        assert_eq!(out_size(&m, MaxRes::P2160), None);
        (m.width, m.height) = (1080, 1920); // portrait
        assert_eq!(out_size(&m, MaxRes::P720), Some((720, 1280)));
        assert_eq!(out_size(&m, MaxRes::P1080), None);
        assert_eq!(out_size(&m, MaxRes::Original), None);
    }

    /// End to end through the real ffmpeg (skipped without one): make a 4 s clip
    /// with sound, a capture date and GPS, then export it trimmed and graded with
    /// every encoder this machine has, and as a stream copy.
    #[test]
    fn exports_a_real_clip() {
        if !crate::video::ffmpeg_available() {
            eprintln!("ffmpeg not found; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("slopshot-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.mp4");
        let made = crate::video::ffmpeg()
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=25:d=4", "-f", "lavfi", "-i", "sine=f=440:d=4"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac", "-shortest"])
            .args(["-metadata", "creation_time=2026-05-13T10:17:52Z", "-metadata", "location=+35.6529+023.7623/"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let meta = crate::video::probe::probe(&src).unwrap();
        assert_eq!((meta.width, meta.height), (640, 360));
        assert!(meta.audio.is_some());
        let (thumb, _) = crate::video::thumbnail(&src);
        assert_eq!(thumb.map(|t| t.dimensions()), Some((480, 270)));

        let edit = Edit { trim_in: 1.0, trim_out: Some(3.0), grade: Grade { saturation: 0.0, ..Grade::NONE } };
        let job = Job { src: src.clone(), meta, edit };
        let (progress, cancel) = (AtomicU32::new(0), AtomicBool::new(false));
        let mut settings: Vec<Settings> = detect()
            .into_iter()
            .map(|encoder| Settings { encoder, max_res: MaxRes::Original, ..Settings::default() })
            .collect();
        assert!(!settings.is_empty(), "no encoder works");
        settings.push(Settings { reencode: false, ..Settings::default() });
        for (k, s) in settings.iter().enumerate() {
            let out = dir.join(format!("out{k}.mp4"));
            let r = run(&job, s, &out, &progress, &cancel);
            assert!(matches!(r, Ok(Outcome::Done)), "{s:?}: {:?}", r.err());
            let m = crate::video::probe::probe(&out).unwrap();
            // Re-encodes are frame accurate; a stream copy starts at a keyframe.
            let ok = if s.reencode { (m.duration - 2.0).abs() < 0.15 } else { (1.9..=3.1).contains(&m.duration) };
            assert!(ok, "{s:?}: {} s", m.duration);
            assert_eq!(m.created.unwrap().to_rfc3339(), "2026-05-13T10:17:53+00:00", "{s:?}");
            assert!(m.location.as_deref().is_some_and(|l| l.starts_with("+35.6529")), "{s:?}: {:?}", m.location);
            assert!(m.audio.is_some(), "{s:?}");
            let mtime = std::fs::metadata(&out).unwrap().modified().unwrap();
            assert_eq!(chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339(), "2026-05-13T10:17:53+00:00");
            if s.reencode {
                assert_eq!(progress.load(Ordering::Relaxed), 1000, "{s:?}");
                // Saturation 0: the exported frame is grey.
                let (frame, _) = crate::video::thumbnail(&out);
                let frame = frame.unwrap();
                let worst = frame.pixels().map(|p| p[0].abs_diff(p[1]).max(p[1].abs_diff(p[2]))).max().unwrap();
                assert!(worst <= 6, "{s:?}: color left after desaturating ({worst})");
            }
            // Never overwritten.
            assert!(matches!(run(&job, s, &out, &progress, &cancel), Ok(Outcome::Skipped)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ten_bit_hevc_stays_ten_bit() {
        assert_eq!(Encoder::HevcQsv.pix_fmt(true), "p010le");
        assert_eq!(Encoder::H264Qsv.pix_fmt(true), "nv12");
        assert_eq!(Encoder::X265.pix_fmt(true), "yuv420p10le");
    }
}
