//! Clip metadata from `ffprobe -print_format json`.

use serde_json::Value;
use std::path::Path;
use std::process::Stdio;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoMeta {
    pub duration: f64,
    /// Frame size as displayed, after any rotation the file asks for.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// "H.264", "HEVC", …
    pub codec: String,
    pub bit_depth: u8,
    /// PQ or HLG. Color editing assumes SDR Rec.709, so it's off for these.
    pub hdr: bool,
    /// ffmpeg's names for the color tags, passed through untouched for HDR clips.
    pub color: Option<(String, String, String)>,
    pub bitrate: Option<u64>,
    /// "AAC · 48 kHz · stereo"
    pub audio: Option<String>,
    /// When recording started (container `creation_time`).
    pub created: Option<chrono::DateTime<chrono::Utc>>,
    /// ISO 6709 location some cameras and drones store (`+35.6529+023.7623/`).
    pub location: Option<String>,
}

impl VideoMeta {
    /// Grid card line: `4K · 29.97p · 0:32 · H.264`.
    pub fn summary(&self) -> String {
        format!("{} · {} · {} · {}", self.resolution_name(), self.fps_label(), super::short_duration(self.duration), self.codec)
    }
    pub fn resolution_name(&self) -> String {
        match self.width.min(self.height) {
            2160.. => "4K".into(),
            1440..2160 => "1440p".into(),
            1080..1440 => "1080p".into(),
            720..1080 => "720p".into(),
            h => format!("{h}p"),
        }
    }
    pub fn fps_label(&self) -> String {
        let r = (self.fps * 100.0).round() / 100.0;
        if (r - r.round()).abs() < 0.005 { format!("{}p", r.round()) } else { format!("{r}p") }
    }
    pub fn frame_time(&self) -> f64 {
        1.0 / if self.fps > 0.0 { self.fps } else { 30.0 }
    }
}

pub fn probe(path: &Path) -> Option<VideoMeta> {
    let out = super::command("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse(std::str::from_utf8(&out.stdout).ok()?)
}

fn ratio(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then_some(n / d)
}

fn codec_name(c: &str) -> String {
    match c {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "av1" => "AV1".into(),
        "prores" => "ProRes".into(),
        "mpeg2video" => "MPEG-2".into(),
        "aac" => "AAC".into(),
        c if c.starts_with("pcm_") => "PCM".into(),
        c => c.to_uppercase(),
    }
}

pub fn parse(json: &str) -> Option<VideoMeta> {
    let v: Value = serde_json::from_str(json).ok()?;
    let streams = v["streams"].as_array()?;
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64() != Some(1))?;
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok());

    let (mut w, mut h) = (video["width"].as_u64()? as u32, video["height"].as_u64()? as u32);
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| num(&d["rotation"])))
        .or_else(|| num(&video["tags"]["rotate"]))
        .unwrap_or(0.0);
    if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
        (w, h) = (h, w);
    }
    let fps = text(&video["avg_frame_rate"])
        .and_then(|r| ratio(&r))
        .or_else(|| ratio(video["r_frame_rate"].as_str()?))
        .unwrap_or(30.0);
    let pix = video["pix_fmt"].as_str().unwrap_or("");
    let bit_depth = num(&video["bits_per_raw_sample"])
        .map(|b| b as u8)
        .filter(|&b| b > 0)
        .unwrap_or(if pix.contains("10") || pix == "p010le" { 10 } else if pix.contains("12") { 12 } else { 8 });
    let trc = video["color_transfer"].as_str().unwrap_or("");
    let hdr = matches!(trc, "smpte2084" | "arib-std-b67");
    let color = match (text(&video["color_primaries"]), text(&video["color_transfer"]), text(&video["color_space"])) {
        (Some(p), Some(t), Some(s)) => Some((p, t, s)),
        _ => None,
    };
    let duration = num(&v["format"]["duration"]).or_else(|| num(&video["duration"])).unwrap_or(0.0);

    let audio = streams.iter().find(|s| s["codec_type"] == "audio").map(|a| {
        let mut parts = vec![codec_name(a["codec_name"].as_str().unwrap_or("?"))];
        if let Some(r) = num(&a["sample_rate"]) {
            parts.push(format!("{} kHz", r / 1000.0));
        }
        parts.push(match a["channels"].as_u64() {
            Some(1) => "mono".into(),
            Some(2) => "stereo".into(),
            Some(n) => format!("{n} ch"),
            None => "?".into(),
        });
        parts.join(" · ")
    });
    let tags = &v["format"]["tags"];
    let created = text(&tags["creation_time"])
        .or_else(|| text(&video["tags"]["creation_time"]))
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        // Cameras with no clock set write 1904-01-01 (the QuickTime epoch).
        .filter(|t| chrono::Datelike::year(t) > 1970);
    let location = text(&tags["location"]).or_else(|| text(&tags["com.apple.quicktime.location.ISO6709"]));

    Some(VideoMeta {
        duration,
        width: w,
        height: h,
        fps,
        codec: codec_name(video["codec_name"].as_str().unwrap_or("?")),
        bit_depth,
        hdr,
        color,
        bitrate: num(&v["format"]["bit_rate"]).map(|b| b as u64),
        audio,
        created,
        location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed ffprobe output for a DJI drone clip (4K, no audio, GPS tag).
    const DJI: &str = r#"{"streams":[
        {"index":0,"codec_name":"h264","codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv420p",
         "color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709",
         "r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001","bits_per_raw_sample":"8","disposition":{"attached_pic":0}},
        {"index":1,"codec_name":"none","codec_type":"data"},
        {"index":2,"codec_name":"mov_text","codec_type":"subtitle"}],
      "format":{"duration":"32.000000","bit_rate":"98706706",
        "tags":{"creation_time":"2026-05-13T10:17:52.000000Z","location":"+35.6529+23.7623"}}}"#;

    #[test]
    fn parses_a_drone_clip() {
        let m = parse(DJI).unwrap();
        assert_eq!((m.width, m.height, m.duration), (3840, 2160, 32.0));
        assert!((m.fps - 29.97).abs() < 0.01);
        assert_eq!(m.summary(), "4K · 29.97p · 0:32 · H.264");
        assert_eq!(m.audio, None);
        assert_eq!(m.location.as_deref(), Some("+35.6529+23.7623"));
        assert_eq!(m.created.unwrap().to_rfc3339(), "2026-05-13T10:17:52+00:00");
        assert!(!m.hdr);
    }

    #[test]
    fn portrait_phone_clip_rotation_and_hdr() {
        let json = r#"{"streams":[
            {"codec_name":"hevc","codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p10le",
             "color_transfer":"arib-std-b67","avg_frame_rate":"50/1",
             "side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]},
            {"codec_name":"pcm_s16be","codec_type":"audio","sample_rate":"48000","channels":2}],
          "format":{"duration":"5.5","tags":{"creation_time":"1904-01-01T00:00:00.000000Z"}}}"#;
        let m = parse(json).unwrap();
        assert_eq!((m.width, m.height), (1080, 1920));
        assert_eq!((m.bit_depth, m.hdr), (10, true));
        assert_eq!(m.fps_label(), "50p");
        assert_eq!(m.audio.as_deref(), Some("PCM · 48 kHz · stereo"));
        assert_eq!(m.created, None);
    }

    #[test]
    fn no_video_stream_is_not_a_clip() {
        assert_eq!(parse(r#"{"streams":[{"codec_type":"audio"}],"format":{}}"#), None);
        assert_eq!(parse("not json"), None);
    }
}
