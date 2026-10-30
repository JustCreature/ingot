// Ported to Rust from darktable (https://github.com/darktable-org/darktable),
// src/iop/sharpen.c, Copyright (C) the darktable developers, GPL-3.0-or-later.
// Modified by the Ingot contributors (2026): works on sRGB-encoded luma and applies
// the detail to all three channels, rather than on the L of Lab.
//! Unsharp mask: blur the lightness, and add back the difference above a threshold.

use rayon::prelude::*;

/// darktable's default radius and threshold, scaled to a 0-1 encoded image.
const RADIUS: f32 = 2.0;
const THRESHOLD: f32 = 0.005;

fn kernel(radius: usize, sigma: f32) -> Vec<f32> {
    let sigma2 = sigma * sigma;
    let mut weights: Vec<f32> = (0..=2 * radius)
        .map(|i| {
            let d = i as f32 - radius as f32;
            (-(d * d) / (2.0 * sigma2)).exp()
        })
        .collect();
    let total: f32 = weights.iter().sum();
    weights.iter_mut().for_each(|w| *w /= total);
    weights
}

/// Sharpens interleaved RGB in place. `scale` is the image's size over the full
/// image's, so a preview shows about the same effect as the export.
pub fn unsharp_mask(rgb: &mut [f32], width: usize, amount: f32, scale: f32) {
    let height = rgb.len() / 3 / width.max(1);
    let scaled = RADIUS * scale;
    let radius = scaled.ceil() as usize;
    if radius == 0 || width < 2 * radius + 1 || height < 2 * radius + 1 {
        return;
    }
    let weights = kernel(radius, (scaled / 2.5).max(0.3));

    let luma: Vec<f32> = rgb
        .par_chunks_exact(3)
        .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
        .collect();

    let mut across = vec![0.0f32; luma.len()];
    across
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, line)| {
            let source = &luma[y * width..(y + 1) * width];
            for (x, out) in line.iter_mut().enumerate() {
                *out = weights
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let at = (x + i).saturating_sub(radius).min(width - 1);
                        source[at] * w
                    })
                    .sum();
            }
        });

    rgb.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, line)| {
            for x in 0..width {
                let blurred: f32 = weights
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let at = (y + i).saturating_sub(radius).min(height - 1);
                        across[at * width + x] * w
                    })
                    .sum();
                let diff = luma[y * width + x] - blurred;
                let excess = diff.abs() - THRESHOLD;
                if excess > 0.0 {
                    let detail = excess.copysign(diff) * amount;
                    for c in 0..3 {
                        line[x * 3 + c] += detail;
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_edge(width: usize, height: usize) -> Vec<f32> {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for _ in 0..height {
            for x in 0..width {
                let v = if x < width / 2 { 0.3 } else { 0.7 };
                rgb.extend([v, v, v]);
            }
        }
        rgb
    }

    #[test]
    fn flat_areas_are_untouched() {
        let mut rgb = vec![0.4f32; 60 * 60 * 3];
        let before = rgb.clone();
        unsharp_mask(&mut rgb, 60, 1.0, 1.0);
        assert_eq!(rgb, before);
    }

    #[test]
    fn edges_gain_contrast_on_both_sides() {
        let mut rgb = step_edge(60, 60);
        unsharp_mask(&mut rgb, 60, 1.0, 1.0);
        let at = |x: usize| rgb[(30 * 60 + x) * 3];
        assert!(at(29) < 0.3, "{}", at(29));
        assert!(at(30) > 0.7, "{}", at(30));
        assert!((at(5) - 0.3).abs() < 1e-5);
    }

    #[test]
    fn more_amount_means_more_sharpening() {
        let overshoot = |amount| {
            let mut rgb = step_edge(60, 60);
            unsharp_mask(&mut rgb, 60, amount, 1.0);
            rgb[(30 * 60 + 30) * 3]
        };
        assert!(overshoot(1.5) > overshoot(0.5));
    }

    #[test]
    fn tiny_images_pass_through() {
        let mut rgb = vec![0.5f32; 3 * 3 * 3];
        rgb[4] = 0.9;
        let before = rgb.clone();
        unsharp_mask(&mut rgb, 3, 1.0, 1.0);
        assert_eq!(rgb, before);
    }
}
