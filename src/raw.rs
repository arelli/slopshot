//! Minimal TIFF/ARW reader: pulls the embedded JPEG preview, capture date and
//! orientation out of a Sony ARW without reading (or demosaicing) the whole file.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Which embedded JPEG to load. Sony bodies store a 160px thumbnail, a ~1.6k
/// preview and (newer ones, e.g. a6700) a full-size camera JPEG.
#[derive(Clone, Copy, PartialEq)]
pub enum Pick {
    /// The screen-sized preview: smallest JPEG at least 1200px wide.
    Preview,
    /// A full-resolution JPEG (at least 3000px wide) or nothing.
    FullSize,
}

pub struct RawMeta {
    pub preview: Option<Vec<u8>>,
    /// "YYYY-MM-DD HH:MM:SS"
    pub date: Option<String>,
    pub orientation: u16,
    pub exif: Exif,
}

#[derive(Clone, Default)]
pub struct Exif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub focal: Option<f32>,
    pub focal35: Option<u32>,
    pub fnumber: Option<f32>,
    /// Exposure time as a fraction of a second.
    pub exposure: Option<(u32, u32)>,
    pub iso: Option<u32>,
    pub bias: Option<f32>,
    /// Largest image in the file (the sensor data), before orientation.
    pub width: u32,
    pub height: u32,
    /// The ARW embeds a full-resolution camera JPEG (a6700 generation and newer).
    pub full_jpeg: bool,
    /// In-camera star rating (Sony MakerNote 0x2002).
    pub rating: Option<u8>,
    pub focus: Option<Focus>,
}

/// Where the camera focused, in sensor coordinates (Sony MakerNote 0x2027/0x2037).
#[derive(Clone, Copy)]
pub struct Focus {
    pub sensor: (f32, f32),
    pub at: (f32, f32),
    /// Size of the focus frame (eye/face box) when the body records it.
    pub frame: Option<(f32, f32)>,
}

impl Focus {
    /// Centre and frame size as fractions of the displayed (rotated) image.
    pub fn uv(&self, orientation: u16) -> ((f32, f32), (f32, f32)) {
        let (w, h) = self.sensor;
        let (u, v) = (self.at.0 / w, self.at.1 / h);
        let (fw, fh) = self.frame.map_or((0.0, 0.0), |(fw, fh)| (fw / w, fh / h));
        match orientation {
            3 => ((1.0 - u, 1.0 - v), (fw, fh)),
            6 => ((1.0 - v, u), (fh, fw)),
            8 => ((v, 1.0 - u), (fh, fw)),
            _ => ((u, v), (fw, fh)),
        }
    }
}

impl Exif {
    pub fn camera(&self) -> Option<String> {
        match (&self.make, &self.model) {
            (Some(make), Some(model)) if !model.to_lowercase().starts_with(&make.to_lowercase()) => {
                Some(format!("{make} {model}"))
            }
            (_, Some(model)) => Some(model.clone()),
            (make, None) => make.clone(),
        }
    }
    pub fn shutter(&self) -> Option<String> {
        let (n, d) = self.exposure.filter(|&(n, d)| n > 0 && d > 0)?;
        let secs = n as f32 / d as f32;
        Some(if secs >= 0.3 {
            format!("{}s", trim(secs, 1))
        } else {
            format!("1/{}s", (1.0 / secs).round())
        })
    }
    pub fn aperture(&self) -> Option<String> {
        self.fnumber.map(|f| format!("f/{}", trim(f, 1)))
    }
    pub fn focal_length(&self) -> Option<String> {
        self.focal.map(|f| format!("{}mm", trim(f, 1)))
    }
    /// e.g. "35mm  f/1.8  1/250s  ISO 400"
    pub fn summary(&self) -> String {
        let iso = self.iso.map(|i| format!("ISO {i}"));
        [self.focal_length(), self.aperture(), self.shutter(), iso]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// Format with at most `places` decimals, dropping trailing zeros ("2.0" -> "2").
pub fn trim(v: f32, places: usize) -> String {
    let s = format!("{v:.places$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// All of the TIFF/EXIF/MakerNote structure sits at the start of an ARW, so one read
/// of this much serves every metadata lookup (instead of ~40 small reads, which
/// adds up on USB drives). The preview JPEG is then a second read.
const HEAD: usize = 64 << 10;

struct Tiff {
    f: File,
    head: Vec<u8>,
    le: bool,
}

impl Tiff {
    fn bytes(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        if let Some(b) = self.head.get(off as usize..off as usize + len) {
            return Some(b.to_vec());
        }
        self.f.seek(SeekFrom::Start(off)).ok()?;
        let mut b = vec![0; len];
        self.f.read_exact(&mut b).ok()?;
        Some(b)
    }
    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }
    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }
    /// Width x height from a JPEG's SOF header, walking marker segments from `off`.
    fn jpeg_dims(&mut self, off: u64) -> Option<(u32, u32)> {
        if self.bytes(off, 2)? != [0xFF, 0xD8] {
            return None;
        }
        let mut pos = off + 2;
        for _ in 0..64 {
            let m = self.bytes(pos, 4)?;
            if m[0] != 0xFF {
                return None;
            }
            let len = u16::from_be_bytes([m[2], m[3]]) as u64;
            if matches!(m[1], 0xC0..=0xC3) {
                let b = self.bytes(pos + 5, 4)?;
                let (h, w) = (u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]]));
                return Some((w as u32, h as u32));
            }
            pos += 2 + len;
        }
        None
    }
    fn rational(&mut self, off: u64, signed: bool) -> Option<f32> {
        let b = self.bytes(off, 8)?;
        let (n, d) = (self.u32(&b[0..4]), self.u32(&b[4..8]));
        if d == 0 {
            return None;
        }
        Some(if signed { n as i32 as f32 / d as i32 as f32 } else { n as f32 / d as f32 })
    }
    /// ASCII value: inline in the entry when it fits in 4 bytes, otherwise at the offset.
    fn text(&mut self, e: &[u8], count: u32) -> Option<String> {
        let b = if count <= 4 { e[8..8 + count as usize].to_vec() } else { self.bytes(self.u32(&e[8..12]) as u64, count.min(256) as usize)? };
        let s = String::from_utf8_lossy(&b).trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }
    fn date(&mut self, off: u64, count: u32) -> Option<String> {
        let b = self.bytes(off, count.min(64) as usize)?;
        let s = String::from_utf8_lossy(&b).trim_end_matches('\0').trim().to_string();
        // "YYYY:MM:DD HH:MM:SS" -> "YYYY-MM-DD HH:MM:SS"
        if s.len() >= 19 && s.as_bytes()[4] == b':' {
            Some(format!("{}-{}-{}", &s[0..4], &s[5..7], &s[8..19]))
        } else {
            None
        }
    }
}

pub fn read(path: &Path, pick: Pick) -> Option<RawMeta> {
    let mut f = File::open(path).ok()?;
    let mut head = Vec::with_capacity(HEAD);
    (&mut f).take(HEAD as u64).read_to_end(&mut head).ok()?;
    let mut t = Tiff { f, head, le: true };
    let hdr = t.bytes(0, 8)?;
    t.le = match &hdr[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if t.u16(&hdr[2..4]) != 42 {
        return None;
    }

    let mut queue = vec![t.u32(&hdr[4..8]) as u64];
    let mut seen = HashSet::new();
    let mut previews: Vec<(u64, u64)> = Vec::new();
    let (mut date_orig, mut date) = (None, None);
    let mut orientation = 1;
    let mut first = true;
    let mut exif = Exif::default();
    let (mut iso, mut rec_exposure_index) = (None, None);
    let mut makernote = None;

    while let Some(off) = queue.pop() {
        if off == 0 || seen.len() > 64 || !seen.insert(off) {
            continue;
        }
        let Some(nb) = t.bytes(off, 2) else { continue };
        let n = t.u16(&nb) as usize;
        if n == 0 || n > 1000 {
            continue;
        }
        let Some(block) = t.bytes(off + 2, n * 12 + 4) else { continue };
        let (mut jpg_off, mut jpg_len) = (0u64, 0u64);
        let (mut w, mut h) = (0, 0);

        for e in block[..n * 12].chunks_exact(12) {
            let tag = t.u16(&e[0..2]);
            let typ = t.u16(&e[2..4]);
            let count = t.u32(&e[4..8]);
            let val = if typ == 3 { t.u16(&e[8..10]) as u32 } else { t.u32(&e[8..12]) };
            match tag {
                0x0201 => jpg_off = val as u64,
                0x0202 => jpg_len = val as u64,
                0x0112 if first => orientation = val as u16,
                0x0132 if date.is_none() => date = t.date(val as u64, count),
                0x9003 if date_orig.is_none() => date_orig = t.date(val as u64, count),
                0x010F if first => exif.make = t.text(e, count),
                0x0110 if first => exif.model = t.text(e, count),
                0xA434 => exif.lens = t.text(e, count),
                0x829A => {
                    if let Some(b) = t.bytes(val as u64, 8) {
                        exif.exposure = Some((t.u32(&b[0..4]), t.u32(&b[4..8])));
                    }
                }
                0x829D => exif.fnumber = t.rational(val as u64, false),
                0x920A => exif.focal = t.rational(val as u64, false),
                0x9204 => exif.bias = t.rational(val as u64, true),
                0xA405 if val > 0 => exif.focal35 = Some(val),
                0x8827 => iso = Some(val),
                0x8832 => rec_exposure_index = Some(val),
                0x0100 => w = val,
                0x0101 => h = val,
                0x8769 => queue.push(val as u64), // Exif IFD
                0x927C => makernote = Some(val as u64),
                0x014A => {
                    // SubIFDs
                    if count == 1 {
                        queue.push(val as u64);
                    } else if count <= 16 {
                        if let Some(arr) = t.bytes(val as u64, count as usize * 4) {
                            queue.extend(arr.chunks_exact(4).map(|c| t.u32(c) as u64));
                        }
                    }
                }
                _ => {}
            }
        }
        if w as u64 * h as u64 > exif.width as u64 * exif.height as u64 {
            (exif.width, exif.height) = (w, h);
        }
        if jpg_off > 0 && jpg_len > 0 {
            previews.push((jpg_off, jpg_len));
        }
        queue.push(t.u32(&block[n * 12..]) as u64); // next IFD in chain
        first = false;
    }

    if let Some(off) = makernote {
        sony_makernote(&mut t, off, &mut exif);
    }

    let mut jpegs: Vec<(u64, u64, u32)> = previews
        .into_iter()
        .filter(|&(_, len)| len < 64 << 20)
        .filter_map(|(off, len)| Some((off, len, t.jpeg_dims(off)?.0)))
        .collect();
    jpegs.sort_by_key(|&(_, _, w)| w);
    exif.full_jpeg = jpegs.last().is_some_and(|j| j.2 >= 3000);
    let chosen = match pick {
        Pick::Preview => jpegs.iter().find(|j| j.2 >= 1200).or(jpegs.last()),
        Pick::FullSize => jpegs.last().filter(|j| j.2 >= 3000),
    };
    let preview = chosen.and_then(|&(off, len, _)| t.bytes(off, len as usize));

    // ISO is a SHORT, so high ISOs are stored as 65535 with the real value elsewhere.
    exif.iso = match iso {
        Some(65535) | None => rec_exposure_index.or(iso),
        i => i,
    };
    Some(RawMeta { preview, date: date_orig.or(date), orientation, exif })
}

/// Sony's MakerNote is a plain IFD (optionally behind a "SONY DSC "/"SONY CAM "
/// header) whose offsets are relative to the TIFF start. Its tag numbers overlap
/// the standard ones, so it's parsed on its own rather than through the main walk.
fn sony_makernote(t: &mut Tiff, off: u64, exif: &mut Exif) -> Option<()> {
    let hdr = t.bytes(off, 12)?;
    let off = if hdr.starts_with(b"SONY") { off + 12 } else { off };
    let nb = t.bytes(off, 2)?;
    let n = t.u16(&nb) as usize;
    if n == 0 || n > 1000 {
        return None;
    }
    let block = t.bytes(off + 2, n * 12)?;
    let (mut loc, mut frame) = (None, None);
    for e in block.chunks_exact(12) {
        let tag = t.u16(&e[0..2]);
        let ptr = t.u32(&e[8..12]) as u64;
        match tag {
            0x2002 => exif.rating = Some(t.u32(&e[8..12]).min(5) as u8),
            // int16u[4]: sensor width, height, focus x, y
            0x2027 => loc = t.bytes(ptr, 8).map(|b| [0, 2, 4, 6].map(|i| t.u16(&b[i..]) as f32)),
            // int16u[3]: frame width, height, valid flag
            0x2037 => frame = t.bytes(ptr, 6).map(|b| [0, 2].map(|i| t.u16(&b[i..]) as f32)),
            _ => {}
        }
    }
    if let Some([w, h, x, y]) = loc.filter(|l| l[0] > 0.0 && l[1] > 0.0 && (l[2] > 0.0 || l[3] > 0.0)) {
        exif.focus = Some(Focus {
            sensor: (w, h),
            at: (x, y),
            frame: frame.filter(|f| f[0] > 0.0 && f[1] > 0.0).map(|f| (f[0], f[1])),
        });
    }
    Some(())
}
