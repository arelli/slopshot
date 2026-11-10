//! Color grading for clips. One function, `Grade::eval`, defines the look: the
//! preview runs it through per-channel lookup tables on every frame, and export
//! bakes the very same function into a 3D LUT (`.cube`) for ffmpeg's `lut3d`, so
//! what you see while editing is what lands in the file.
//!
//! The pipeline, per channel, on display-referred Rec.709 values in 0..1:
//! levels (black/white point) → midtones (the middle levels handle) → exposure and
//! white balance in (approximately) linear light → contrast. Saturation comes last
//! and is the only step that mixes channels.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Grade {
    /// Stops, -3..3.
    pub exposure: f32,
    /// -1..1; positive is an S-curve, negative flattens toward grey.
    pub contrast: f32,
    /// 0..2, 1 = unchanged, 0 = black and white.
    pub saturation: f32,
    /// -1..1, cool to warm.
    pub temperature: f32,
    /// -1..1, green to magenta.
    pub tint: f32,
    /// Levels: input values at or below `black` become black, at or above `white` white.
    pub black: f32,
    pub white: f32,
    /// Levels midtone gamma (Photoshop's middle number), 1 = unchanged, >1 brightens.
    pub mid: f32,
}

impl Default for Grade {
    fn default() -> Self {
        Self::NONE
    }
}

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];
const GAMMA: f32 = 2.2;
pub const MID_RANGE: (f32, f32) = (0.25, 4.0);

impl Grade {
    pub const NONE: Grade = Grade {
        exposure: 0.0,
        contrast: 0.0,
        saturation: 1.0,
        temperature: 0.0,
        tint: 0.0,
        black: 0.0,
        white: 1.0,
        mid: 1.0,
    };

    pub fn is_identity(&self) -> bool {
        *self == Self::NONE
    }

    /// Per-channel gains for the white balance, normalised so grey stays as bright.
    fn gains(&self) -> [f32; 3] {
        let (t, g) = (self.temperature * 0.25, self.tint * 0.25);
        let raw = [1.0 + t, 1.0 - g, 1.0 - t];
        let y: f32 = raw.iter().zip(LUMA).map(|(r, l)| r * l).sum();
        raw.map(|r| r / y)
    }

    /// The per-channel part of the grade for one channel value.
    fn curve(&self, gain: f32, x: f32) -> f32 {
        let x = ((x - self.black) / (self.white - self.black).max(1e-3)).clamp(0.0, 1.0);
        let x = x.powf(1.0 / self.mid.clamp(MID_RANGE.0, MID_RANGE.1));
        let lin = x.powf(GAMMA) * self.exposure.exp2() * gain;
        contrast(lin.max(0.0).powf(1.0 / GAMMA).min(1.0), self.contrast)
    }

    /// The whole grade for one RGB value (0..1).
    pub fn eval(&self, rgb: [f32; 3]) -> [f32; 3] {
        let g = self.gains();
        saturate([self.curve(g[0], rgb[0]), self.curve(g[1], rgb[1]), self.curve(g[2], rgb[2])], self.saturation)
    }

    /// Lookup tables for 8-bit frames.
    pub fn tables(&self) -> Tables {
        let g = self.gains();
        let curve = |c: usize| std::array::from_fn(|i| self.curve(g[c], i as f32 / 255.0));
        Tables { curves: [curve(0), curve(1), curve(2)], saturation: self.saturation, identity: self.is_identity() }
    }

    /// The grade as an `n`³ Adobe/Resolve `.cube` 3D LUT (red varies fastest).
    pub fn cube(&self, n: usize) -> String {
        let mut s = format!("TITLE \"slopshot grade\"\nLUT_3D_SIZE {n}\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n");
        let step = 1.0 / (n - 1) as f32;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let [r, g, b] = self.eval([r as f32 * step, g as f32 * step, b as f32 * step]);
                    s += &format!("{r:.6} {g:.6} {b:.6}\n");
                }
            }
        }
        s
    }

    /// Levels stretched to the frame's darkest and brightest 0.1% (what Photoshop's
    /// Auto does), leaving the rest of the grade alone.
    pub fn auto_levels(&self, hist: &Histogram) -> Grade {
        let (lo, hi) = hist.luma_percentiles(0.001, 0.999);
        Grade { black: (lo as f32 / 255.0).min(0.3), white: (hi as f32 / 255.0).max(0.7), ..*self }
    }
}

fn contrast(x: f32, k: f32) -> f32 {
    if k >= 0.0 {
        let s = x * x * (3.0 - 2.0 * x);
        x + k * (s - x)
    } else {
        0.5 + (x - 0.5) * (1.0 + k * 0.6)
    }
}

fn saturate(c: [f32; 3], s: f32) -> [f32; 3] {
    let y: f32 = c.iter().zip(LUMA).map(|(v, l)| v * l).sum();
    c.map(|v| (y + s * (v - y)).clamp(0.0, 1.0))
}

/// `Grade` compiled for 8-bit RGB frames.
pub struct Tables {
    curves: [[f32; 256]; 3],
    saturation: f32,
    identity: bool,
}

impl Tables {
    /// Grade packed RGB24 `src` into `dst` (same length).
    pub fn apply(&self, src: &[u8], dst: &mut [u8]) {
        if self.identity {
            return dst.copy_from_slice(src);
        }
        let [cr, cg, cb] = &self.curves;
        let s = self.saturation;
        crate::pixels::par_rows(dst, 3 * 1024, |first, band| {
            let src = &src[first * 3 * 1024..][..band.len()];
            for (o, p) in band.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
                let c = [cr[p[0] as usize], cg[p[1] as usize], cb[p[2] as usize]];
                let c = if s == 1.0 { c.map(|v| v.clamp(0.0, 1.0)) } else { saturate(c, s) };
                o[0] = (c[0] * 255.0 + 0.5) as u8;
                o[1] = (c[1] * 255.0 + 0.5) as u8;
                o[2] = (c[2] * 255.0 + 0.5) as u8;
            }
        });
    }
}

/// Per-channel 256-bin histogram plus luma.
#[derive(Clone)]
pub struct Histogram {
    pub rgb: [[u32; 256]; 3],
    pub luma: [u32; 256],
}

impl Histogram {
    /// From packed RGB24, sampling every `step`th pixel (a frame has far more than
    /// a 256-bin histogram needs).
    pub fn of(rgb: &[u8], step: usize) -> Self {
        let mut h = Histogram { rgb: [[0; 256]; 3], luma: [0; 256] };
        for p in rgb.chunks_exact(3).step_by(step.max(1)) {
            for (bins, &v) in h.rgb.iter_mut().zip(p) {
                bins[v as usize] += 1;
            }
            let y = (54 * p[0] as u32 + 183 * p[1] as u32 + 19 * p[2] as u32) >> 8;
            h.luma[y as usize] += 1;
        }
        h
    }

    /// Luma values below which `lo` and `hi` of the samples fall.
    pub fn luma_percentiles(&self, lo: f64, hi: f64) -> (usize, usize) {
        let total: u64 = self.luma.iter().map(|&n| n as u64).sum();
        let at = |q: f64| {
            let target = (total as f64 * q) as u64;
            let mut acc = 0;
            self.luma.iter().position(|&n| {
                acc += n as u64;
                acc > target
            })
        };
        (at(lo).unwrap_or(0), at(hi).unwrap_or(255))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= eps)
    }

    #[test]
    fn default_grade_changes_nothing() {
        let g = Grade::NONE;
        for v in [0.0, 0.1, 0.5, 0.73, 1.0] {
            assert!(close(g.eval([v, v * 0.5, 1.0 - v]), [v, v * 0.5, 1.0 - v], 1e-4));
        }
        let src: Vec<u8> = (0..=255).flat_map(|v| [v, 255 - v, v / 2]).collect();
        let mut dst = vec![0; src.len()];
        g.tables().apply(&src, &mut dst);
        assert_eq!(src, dst);
    }

    #[test]
    fn white_balance_keeps_grey_brightness_and_shifts_hue() {
        let warm = Grade { temperature: 1.0, ..Grade::NONE };
        let [r, g, b] = warm.eval([0.4, 0.4, 0.4]);
        assert!(r > g && g > b, "{r} {g} {b}");
        let y = r * LUMA[0] + g * LUMA[1] + b * LUMA[2];
        assert!((y - 0.4).abs() < 0.03, "{y}");
    }

    #[test]
    fn exposure_levels_contrast_saturation_behave() {
        let up = Grade { exposure: 1.0, ..Grade::NONE }.eval([0.3; 3])[0];
        assert!(up > 0.38 && up < 0.45, "{up}"); // one stop at gamma 2.2: ×1.37
        let lv = Grade { black: 0.2, white: 0.8, ..Grade::NONE };
        assert!(lv.eval([0.2; 3])[0] < 1e-6);
        assert!(lv.eval([0.8; 3])[0] > 1.0 - 1e-6);
        assert!((lv.eval([0.5; 3])[0] - 0.5).abs() < 1e-5);
        let bright_mids = Grade { mid: 2.0, ..Grade::NONE }.eval([0.25; 3])[0];
        assert!((bright_mids - 0.5).abs() < 1e-5);
        let c = Grade { contrast: 1.0, ..Grade::NONE };
        assert!(c.eval([0.25; 3])[0] < 0.25 && c.eval([0.75; 3])[0] > 0.75);
        assert!((c.eval([0.5; 3])[0] - 0.5).abs() < 1e-5);
        let bw = Grade { saturation: 0.0, ..Grade::NONE }.eval([0.9, 0.2, 0.1]);
        assert!(close(bw, [bw[0]; 3], 1e-6));
    }

    /// The preview (tables) and the export (cube) are the same function.
    #[test]
    fn tables_and_cube_agree_with_eval() {
        let g = Grade { exposure: 0.7, contrast: 0.4, saturation: 1.3, temperature: -0.5, tint: 0.2, black: 0.05, white: 0.95, mid: 1.2 };
        let src = [12u8, 128, 240, 200, 30, 90];
        let mut dst = [0u8; 6];
        g.tables().apply(&src, &mut dst);
        for (s, d) in src.chunks(3).zip(dst.chunks(3)) {
            let want = g.eval([s[0] as f32 / 255.0, s[1] as f32 / 255.0, s[2] as f32 / 255.0]);
            for c in 0..3 {
                assert!((d[c] as f32 - want[c] * 255.0).abs() <= 0.51, "{d:?} vs {want:?}");
            }
        }
        let cube = g.cube(3);
        let lines: Vec<&str> = cube.lines().collect();
        assert_eq!(lines[1], "LUT_3D_SIZE 3");
        assert_eq!(lines.len(), 4 + 27);
        // Entry 1 is r = 0.5, g = b = 0 (red varies fastest).
        let e: Vec<f32> = lines[5].split(' ').map(|v| v.parse().unwrap()).collect();
        assert!(close([e[0], e[1], e[2]], g.eval([0.5, 0.0, 0.0]), 1e-5));
    }

    #[test]
    fn histogram_and_auto_levels() {
        // A dull frame: everything between 50 and 200.
        let frame: Vec<u8> = (0..10_000).flat_map(|i| [50 + (i % 151) as u8; 3]).collect();
        let h = Histogram::of(&frame, 1);
        assert_eq!(h.rgb[0].iter().sum::<u32>(), 10_000);
        assert_eq!(h.luma[..49].iter().sum::<u32>(), 0);
        let g = Grade::NONE.auto_levels(&h);
        assert!((g.black - 50.0 / 255.0).abs() < 0.01 && (g.white - 200.0 / 255.0).abs() < 0.01, "{g:?}");
    }
}
