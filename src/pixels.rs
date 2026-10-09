//! Pixel plumbing: orientation + conversion to egui's format in one pass, block
//! downscaling, and a scoped-thread helper so 26 MP images use every core.

use eframe::egui::{Color32, ColorImage};
use image::RgbImage;
use std::mem::MaybeUninit;

/// Below this many elements, threading costs more than it saves (thumbnails).
const PAR_MIN: usize = 1 << 20;

/// Split `data` (rows of `row_len`) into one contiguous band per core and run
/// `f(first_row, band)` on each in parallel.
pub fn par_rows<T: Send>(data: &mut [T], row_len: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let threads = if data.len() < PAR_MIN { 1 } else { std::thread::available_parallelism().map_or(4, |n| n.get()) };
    let rows = data.len() / row_len.max(1);
    let per = rows.div_ceil(threads).max(1);
    if threads == 1 {
        return f(0, data);
    }
    std::thread::scope(|s| {
        for (i, band) in data.chunks_mut(per * row_len).enumerate() {
            let f = &f;
            s.spawn(move || f(i * per, band));
        }
    });
}

/// Build a `rows x row_len` buffer in parallel without first zero-filling it (a
/// single-threaded 100 MB memset is a noticeable chunk of a 26 MP pipeline).
/// `f(first_row, band)` must write every element of its band.
pub fn par_build<T: Send>(rows: usize, row_len: usize, f: impl Fn(usize, &mut [MaybeUninit<T>]) + Sync) -> Vec<T> {
    let n = rows * row_len;
    let mut v = Vec::with_capacity(n);
    par_rows(&mut v.spare_capacity_mut()[..n], row_len, f);
    // SAFETY: par_rows covered all n elements and `f` initialises every one it's given.
    unsafe { v.set_len(n) };
    v
}

/// Rotate per EXIF orientation, apply a tone `lut` and convert RGB -> Color32, all
/// in one pass straight into the output (each extra 26 MP buffer costs ~17 ms
/// just for the kernel to hand over fresh pages).
pub fn render(rgb: &RgbImage, orientation: u16, lut: &[u8; 256]) -> ColorImage {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let (ow, oh) = if matches!(orientation, 6 | 8) { (h, w) } else { (w, h) };
    let s = rgb.as_raw();
    let px = |sx: usize, sy: usize| {
        let i = (sy * w + sx) * 3;
        Color32::from_rgb(lut[s[i] as usize], lut[s[i + 1] as usize], lut[s[i + 2] as usize])
    };
    let out = par_build(oh, ow, |y0, band| {
        let rows = band.len() / ow;
        match orientation {
            3 => {
                for (r, row) in band.chunks_exact_mut(ow).enumerate() {
                    for (x, o) in row.iter_mut().enumerate() {
                        o.write(px(w - 1 - x, h - 1 - (y0 + r)));
                    }
                }
            }
            6 | 8 => {
                // Output rows come from source columns; walk 64-wide column blocks
                // so source reads stay within a few cache-friendly rows.
                for xb in (0..ow).step_by(64) {
                    for r in 0..rows {
                        let y = y0 + r;
                        for x in xb..(xb + 64).min(ow) {
                            band[r * ow + x].write(if orientation == 6 { px(y, h - 1 - x) } else { px(w - 1 - y, x) });
                        }
                    }
                }
            }
            _ => {
                let src = &s[y0 * w * 3..][..rows * w * 3];
                for (o, p) in band.iter_mut().zip(src.chunks_exact(3)) {
                    o.write(Color32::from_rgb(lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]));
                }
            }
        }
    });
    ColorImage::new([ow, oh], out)
}

/// Average `f`x`f` blocks (e.g. f = 4 gives a quarter-resolution thumbnail).
pub fn box_shrink(src: &RgbImage, f: u32) -> RgbImage {
    let (w, h) = ((src.width() / f) as usize, (src.height() / f) as usize);
    let (f, sw, s) = (f as usize, src.width() as usize * 3, src.as_raw());
    let n = (f * f) as u32;
    let mut out = vec![0u8; w * h * 3];
    let mut acc = vec![0u32; w * 3];
    for (y, orow) in out.chunks_exact_mut(w * 3).enumerate() {
        acc.fill(0);
        for dy in 0..f {
            let row = &s[(y * f + dy) * sw..][..w * f * 3];
            for (a, block) in acc.chunks_exact_mut(3).zip(row.chunks_exact(f * 3)) {
                for p in block.chunks_exact(3) {
                    a[0] += p[0] as u32;
                    a[1] += p[1] as u32;
                    a[2] += p[2] as u32;
                }
            }
        }
        for (o, a) in orow.iter_mut().zip(&acc) {
            *o = ((a + n / 2) / n) as u8;
        }
    }
    RgbImage::from_raw(w as u32, h as u32, out).unwrap()
}
