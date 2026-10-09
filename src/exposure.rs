//! Display-only exposure: brightens/darkens decoded previews on the CPU.
//! Works in linear light with a soft highlight shoulder so boosted images
//! don't clip harshly. Never touches the files on disk.

use image::RgbImage;

/// Median scene luminance gets pushed towards this (a bit under linear middle grey,
/// so night shots stay contrasty instead of turning into dusk).
const AUTO_TARGET: f32 = 0.12;
pub const AUTO_RANGE: (f32, f32) = (-2.0, 3.0);

fn to_lin(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Tone curve for `ev` stops of display exposure, as a lookup on 8-bit sRGB values.
pub fn lut(ev: f32) -> [u8; 256] {
    if ev.abs() < 0.01 {
        return std::array::from_fn(|i| i as u8);
    }
    let gain = ev.exp2();
    // Shoulder starts lower the harder we push, so highlights roll off instead of clipping.
    let knee = if ev > 0.0 { (1.0 - 0.1 * ev).clamp(0.6, 1.0) } else { 1.0 };
    std::array::from_fn(|i| {
        let x = to_lin(i as f32 / 255.0) * gain;
        let y = if x <= knee || knee >= 1.0 {
            x.min(1.0)
        } else {
            let r = 1.0 - knee;
            knee + r * (1.0 - (-(x - knee) / r).exp())
        };
        (to_srgb(y) * 255.0 + 0.5) as u8
    })
}

/// EV that brings the image's median luminance to AUTO_TARGET. Samples at most
/// ~250k pixels; a median doesn't need all 26 million.
pub fn auto_ev(img: &RgbImage) -> f32 {
    let lin: [f32; 256] = std::array::from_fn(|i| to_lin(i as f32 / 255.0));
    let mut hist = [0u32; 4096];
    let px = img.as_raw().chunks_exact(3);
    let n = px.len();
    let step = (n / 250_000).max(1);
    for p in px.step_by(step) {
        let y = 0.2126 * lin[p[0] as usize] + 0.7152 * lin[p[1] as usize] + 0.0722 * lin[p[2] as usize];
        hist[((y * 4095.0) as usize).min(4095)] += 1;
    }
    let half = n.div_ceil(step) as u32 / 2;
    let mut acc = 0;
    let bin = hist.iter().position(|&n| {
        acc += n;
        acc > half
    });
    let median = (bin.unwrap_or(0) as f32 + 0.5) / 4096.0;
    (AUTO_TARGET / median).log2().clamp(AUTO_RANGE.0, AUTO_RANGE.1)
}
