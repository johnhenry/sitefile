//! Lanczos-3 resampling in fixed-point integer arithmetic.
//!
//! - The filter is Lanczos with a = 3 (`sinc(x) * sinc(x / 3)` for |x| < 3),
//!   widened by the scale factor when shrinking, as Pillow and libvips do.
//! - Pixels are resampled in sRGB space (no gamma conversion) with
//!   premultiplied alpha, so transparent pixels don't bleed their color.
//! - Weights are computed in f64 using only `+ - * /` and `floor` (sin is a
//!   Taylor series, not the platform's libm), then quantized to 14-bit
//!   integers that sum to exactly 1 << 14. The convolution itself is integer
//!   arithmetic, so the result is the same on every platform, not just in
//!   the WebAssembly module.
//! - The two passes are separable: rows are resampled horizontally on demand
//!   and only the rows the vertical kernel still needs are kept, so memory is
//!   bounded by the kernel window, not by the image.

use std::collections::VecDeque;

const PREC: u32 = 14;
const ONE: i64 = 1 << PREC;
const PI: f64 = std::f64::consts::PI;
/// The largest premultiplied channel value: 255 * 255.
const FULL: i64 = 255 * 255;

/// `floor(x + 0.5)`: rounds half up, the same on every platform.
fn round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// sin(pi * x) from basic arithmetic only.
pub fn sin_pi(x: f64) -> f64 {
    let n = round(x);
    let t = PI * (x - n); // in [-pi/2, pi/2]
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    for k in 1..=9u32 {
        term *= -t2 / ((2 * k) as f64 * (2 * k + 1) as f64);
        sum += term;
    }
    if (n as i64).rem_euclid(2) == 1 {
        -sum
    } else {
        sum
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        sin_pi(x) / (PI * x)
    }
}

pub fn lanczos3(x: f64) -> f64 {
    if x <= -3.0 || x >= 3.0 {
        0.0
    } else {
        sinc(x) * sinc(x / 3.0)
    }
}

/// For each output index: the first source index and its integer weights.
pub struct Taps {
    pub start: Vec<usize>,
    pub weights: Vec<Vec<i32>>,
}

pub fn taps(src: u32, dst: u32) -> Taps {
    let scale = src as f64 / dst as f64;
    let fscale = if scale > 1.0 { scale } else { 1.0 };
    let support = 3.0 * fscale;
    let mut start = Vec::with_capacity(dst as usize);
    let mut weights = Vec::with_capacity(dst as usize);
    for i in 0..dst {
        let center = (i as f64 + 0.5) * scale;
        let lo = ((center - support).floor() as i64).max(0);
        let hi = ((center + support).ceil() as i64).min(src as i64).max(lo + 1);
        let raw: Vec<f64> = (lo..hi).map(|j| lanczos3((j as f64 + 0.5 - center) / fscale)).collect();
        let total: f64 = raw.iter().sum();
        let mut q: Vec<i32> = raw.iter().map(|w| round(w / total * ONE as f64) as i32).collect();
        // Make the weights sum to exactly ONE (put the rounding error on the largest).
        let diff = ONE as i32 - q.iter().sum::<i32>();
        let mut big = 0;
        for (k, w) in q.iter().enumerate() {
            if *w > q[big] {
                big = k;
            }
        }
        q[big] += diff;
        start.push(lo as usize);
        weights.push(q);
    }
    Taps { start, weights }
}

/// Resize RGBA8 pixels from `sw`×`sh` to `dw`×`dh`.
pub fn resize(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let h = taps(sw, dw);
    let v = taps(sh, dh);
    let dwu = dw as usize;
    let swu = sw as usize;

    // One source row, resampled horizontally: premultiplied, 4 channels per pixel.
    let hrow = |y: usize| -> Vec<i32> {
        let row = &src[y * swu * 4..(y + 1) * swu * 4];
        let mut out = vec![0i32; dwu * 4];
        for x in 0..dwu {
            let s = h.start[x];
            let mut acc = [0i64; 4];
            for (k, &w) in h.weights[x].iter().enumerate() {
                let p = &row[(s + k) * 4..(s + k) * 4 + 4];
                let a = p[3] as i64;
                let w = w as i64;
                acc[0] += w * p[0] as i64 * a;
                acc[1] += w * p[1] as i64 * a;
                acc[2] += w * p[2] as i64 * a;
                acc[3] += w * a * 255;
            }
            for c in 0..4 {
                out[x * 4 + c] = ((acc[c] + ONE / 2) >> PREC) as i32;
            }
        }
        out
    };

    let mut out = vec![0u8; dwu * dh as usize * 4];
    let mut window: VecDeque<Vec<i32>> = VecDeque::new();
    let mut first = 0usize;
    for y in 0..dh as usize {
        let lo = v.start[y];
        let n = v.weights[y].len();
        if lo < first {
            window.clear();
            first = lo;
        }
        while first < lo && !window.is_empty() {
            window.pop_front();
            first += 1;
        }
        if window.is_empty() {
            first = lo;
        }
        while first + window.len() < lo + n {
            let next = first + window.len();
            window.push_back(hrow(next));
        }
        let dst = &mut out[y * dwu * 4..(y + 1) * dwu * 4];
        for x in 0..dwu {
            let mut acc = [0i64; 4];
            for (k, &w) in v.weights[y].iter().enumerate() {
                let r = &window[lo - first + k];
                for c in 0..4 {
                    acc[c] += w as i64 * r[x * 4 + c] as i64;
                }
            }
            let mut px = [0i64; 4];
            for c in 0..4 {
                px[c] = ((acc[c] + ONE / 2) >> PREC).clamp(0, FULL);
            }
            let alpha = px[3];
            let a8 = (alpha + 127) / 255;
            let d = &mut dst[x * 4..x * 4 + 4];
            if a8 == 0 {
                d.copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            for c in 0..3 {
                d[c] = ((px[c] * 255 + alpha / 2) / alpha).clamp(0, 255) as u8;
            }
            d[3] = a8 as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_matches_the_platform_closely() {
        for i in -300..=300 {
            let x = i as f64 / 100.0;
            assert!((sin_pi(x) - (PI * x).sin()).abs() < 1e-12, "{x}");
        }
    }

    #[test]
    fn weights_sum_to_one() {
        for (s, d) in [(100, 10), (10, 7), (1, 1), (4000, 320), (7, 3), (3, 2)] {
            let t = taps(s, d);
            for w in &t.weights {
                assert_eq!(w.iter().sum::<i32>(), ONE as i32);
            }
        }
    }

    #[test]
    fn a_flat_color_stays_flat() {
        let src: Vec<u8> = (0..64 * 48).flat_map(|_| [200u8, 100, 50, 255]).collect();
        let out = resize(&src, 64, 48, 17, 13);
        assert_eq!(out.len(), 17 * 13 * 4);
        assert!(out.chunks(4).all(|p| p == [200, 100, 50, 255]));
    }

    #[test]
    fn transparent_pixels_do_not_bleed() {
        // Left half opaque red, right half fully transparent green.
        let src: Vec<u8> = (0..32 * 8).flat_map(|i| if i % 32 < 16 { [255u8, 0, 0, 255] } else { [0, 255, 0, 0] }).collect();
        let out = resize(&src, 32, 8, 8, 2);
        for p in out.chunks(4) {
            assert!(p[3] == 0 || p[1] == 0, "{p:?}");
        }
    }
}
