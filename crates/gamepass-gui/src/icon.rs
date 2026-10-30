//! The app mark, a download arrow into a tray on Xbox green, as pixels. The
//! window icon uses it, and `build.rs` uses it for the exe's embedded icon.

const GREEN: [f32; 3] = [16.0, 124.0, 16.0];
const WHITE: [f32; 3] = [255.0, 255.0, 255.0];

/// Straight (not premultiplied) RGBA, `side` × `side`, top row first.
pub fn rgba(side: u32) -> Vec<u8> {
    const SAMPLES: u32 = 8;
    let rounded = |u: f32, v: f32| {
        let r = 0.24;
        let (dx, dy) = ((u - 0.5).abs() - (0.5 - r), (v - 0.5).abs() - (0.5 - r));
        dx <= 0.0 || dy <= 0.0 || dx * dx + dy * dy <= r * r
    };
    let white = |u: f32, v: f32| {
        let shaft = (u - 0.5).abs() <= 0.055 && (0.2..=0.5).contains(&v);
        let head = (0.45..=0.68).contains(&v) && (u - 0.5).abs() <= 0.2 * (0.68 - v) / 0.23;
        let tray = (v - 0.8).abs() <= 0.045 && (0.24..=0.76).contains(&u);
        shaft || head || tray
    };
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let (mut sum, mut cover) = ([0.0f32; 3], 0.0f32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let u = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / side as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / side as f32;
                    if !rounded(u, v) {
                        continue;
                    }
                    let c = if white(u, v) { WHITE } else { GREEN };
                    for i in 0..3 {
                        sum[i] += c[i];
                    }
                    cover += 1.0;
                }
            }
            let channel = |s: f32| if cover > 0.0 { (s / cover).round() as u8 } else { 0 };
            let alpha = (cover / (SAMPLES * SAMPLES) as f32 * 255.0).round() as u8;
            out.extend([channel(sum[0]), channel(sum[1]), channel(sum[2]), alpha]);
        }
    }
    out
}
