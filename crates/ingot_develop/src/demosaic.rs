// Ported to Rust from darktable (https://github.com/darktable-org/darktable),
// src/iop/demosaicing/rcd.c, Copyright (C) the darktable developers, GPL-3.0-or-later.
// Modified by the Ingot contributors (2026).
//! Ratio Corrected Demosaicing, processed in overlapping tiles.
//!
//! The outermost pixels, which RCD can't reach, come from a box filter. darktable
//! uses PPG there.

use std::sync::Mutex;

use rayon::prelude::*;

const TILE: usize = 256;
const BORDER: usize = 10;
const MARGIN: usize = 9;
const TILE_VALID: usize = TILE - 2 * BORDER;
const W1: usize = TILE;
const W2: usize = 2 * TILE;
const W3: usize = 3 * TILE;
const W4: usize = 4 * TILE;
const EPS: f32 = 1e-5;
const EPS_SQ: f32 = 1e-10;

/// Which color each of the 2x2 Bayer cells holds: 0 red, 1 green, 2 blue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BayerPattern(pub [[u8; 2]; 2]);

impl BayerPattern {
    #[inline]
    fn color(&self, row: usize, col: usize) -> usize {
        self.0[row & 1][col & 1] as usize
    }
}

#[inline]
fn sqr(value: f32) -> f32 {
    value * value
}

#[inline]
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

struct TileBuffers {
    cfa: Vec<f32>,
    rgb: [Vec<f32>; 3],
    vh_dir: Vec<f32>,
    pq_dir: Vec<f32>,
    p_hpf: Vec<f32>,
    q_hpf: Vec<f32>,
    buffer_v: [Vec<f32>; 3],
    buffer_h: Vec<f32>,
}

impl TileBuffers {
    fn new() -> Self {
        let full = || vec![0.0f32; TILE * TILE];
        let half = || vec![0.0f32; TILE * TILE / 2 + 1];
        TileBuffers {
            cfa: full(),
            rgb: [full(), full(), full()],
            vh_dir: full(),
            pq_dir: half(),
            p_hpf: half(),
            q_hpf: half(),
            buffer_v: [vec![0.0; TILE], vec![0.0; TILE], vec![0.0; TILE]],
            buffer_h: vec![0.0; TILE],
        }
    }
}

fn demosaic_box(
    out: &mut [f32],
    input: &[f32],
    width: usize,
    height: usize,
    pattern: BayerPattern,
) {
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(row, line)| {
            for col in 0..width {
                let mut sum = [0.0f32; 3];
                let mut count = [0.0f32; 3];
                for y in row.saturating_sub(1)..(row + 2).min(height) {
                    for x in col.saturating_sub(1)..(col + 2).min(width) {
                        let color = pattern.color(y, x);
                        sum[color] += input[width * y + x].max(0.0);
                        count[color] += 1.0;
                    }
                }
                for c in 0..3 {
                    line[col * 3 + c] = sum[c] / count[c].max(1.0);
                }
            }
        });
}

#[inline]
fn line_filter(cfa: &[f32], i: usize, step1: usize, step2: usize, step3: usize) -> f32 {
    sqr(
        (cfa[i - step3] - cfa[i - step1] - cfa[i + step1] + cfa[i + step3])
            - 3.0 * (cfa[i - step2] + cfa[i + step2])
            + 6.0 * cfa[i],
    )
}

/// Runs steps 1 to 4 on one tile. Row and column offsets keep the CFA phase of the
/// full image.
fn process_tile(
    b: &mut TileBuffers,
    input: &[f32],
    width: usize,
    pattern: BayerPattern,
    origin: (usize, usize),
    rows: usize,
    cols: usize,
) {
    let (row_start, col_start) = origin;
    let fc = |row: usize, col: usize| pattern.color(row + row_start, col + col_start);

    if rows < TILE || cols < TILE {
        // Partial tiles read borders that are never written; keep them consistent.
        b.vh_dir.fill(0.0);
        b.rgb.iter_mut().for_each(|channel| channel.fill(0.0));
    }

    // Step 0: fill data and make sure it isn't negative.
    for row in 0..rows {
        let (c0, c1) = (fc(row, 0), fc(row, 1));
        let from = (row + row_start) * width + col_start;
        for col in 0..cols {
            let i = row * TILE + col;
            let value = input[from + col].max(0.0);
            b.cfa[i] = value;
            b.rgb[c0][i] = value;
            b.rgb[c1][i] = value;
        }
    }

    // Step 1: vertical and horizontal interpolation directions.
    let [v0, v1, v2] = &mut b.buffer_v;
    let (mut v0, mut v1, mut v2) = (v0, v1, v2);
    for row in 3..rows.saturating_sub(3).min(5) {
        let line = if row == 3 { &mut *v0 } else { &mut *v1 };
        for col in 4..cols - 4 {
            line[col - 4] = line_filter(&b.cfa, row * TILE + col, W1, W2, W3);
        }
    }
    for row in 4..rows - 4 {
        for col in 3..cols - 3 {
            b.buffer_h[col - 3] = line_filter(&b.cfa, row * TILE + col, 1, 2, 3);
        }
        for col in 4..cols - 4 {
            v2[col - 4] = line_filter(&b.cfa, (row + 1) * TILE + col, W1, W2, W3);
        }
        for col in 4..cols - 4 {
            let v_stat = (v0[col - 4] + v1[col - 4] + v2[col - 4]).max(EPS_SQ);
            let h_stat =
                (b.buffer_h[col - 4] + b.buffer_h[col - 3] + b.buffer_h[col - 2]).max(EPS_SQ);
            b.vh_dir[row * TILE + col] = v_stat / (v_stat + h_stat);
        }
        std::mem::swap(&mut v0, &mut v1);
        std::mem::swap(&mut v1, &mut v2);
    }

    // Step 2: low pass filter from the raw data. Shares its buffer with `pq_dir`,
    // which isn't used until step 4.
    let lpf = &mut b.pq_dir;
    for row in 2..rows - 2 {
        let mut col = 2 + (fc(row, 0) & 1);
        while col < cols - 2 {
            let i = row * TILE + col;
            let cfa = &b.cfa;
            lpf[i / 2] = cfa[i]
                + 0.5 * (cfa[i - W1] + cfa[i + W1] + cfa[i - 1] + cfa[i + 1])
                + 0.25 * (cfa[i - W1 - 1] + cfa[i - W1 + 1] + cfa[i + W1 - 1] + cfa[i + W1 + 1]);
            col += 2;
        }
    }

    // Step 3: green at blue and red positions.
    for row in 4..rows - 4 {
        let mut col = 4 + (fc(row, 0) & 1);
        while col < cols - 4 {
            let i = row * TILE + col;
            let lp = i / 2;
            let cfa = &b.cfa;
            let center = cfa[i];

            let n_grad = EPS
                + (cfa[i - W1] - cfa[i + W1]).abs()
                + (center - cfa[i - W2]).abs()
                + (cfa[i - W1] - cfa[i - W3]).abs()
                + (cfa[i - W2] - cfa[i - W4]).abs();
            let s_grad = EPS
                + (cfa[i + W1] - cfa[i - W1]).abs()
                + (center - cfa[i + W2]).abs()
                + (cfa[i + W1] - cfa[i + W3]).abs()
                + (cfa[i + W2] - cfa[i + W4]).abs();
            let w_grad = EPS
                + (cfa[i - 1] - cfa[i + 1]).abs()
                + (center - cfa[i - 2]).abs()
                + (cfa[i - 1] - cfa[i - 3]).abs()
                + (cfa[i - 2] - cfa[i - 4]).abs();
            let e_grad = EPS
                + (cfa[i + 1] - cfa[i - 1]).abs()
                + (center - cfa[i + 2]).abs()
                + (cfa[i + 1] - cfa[i + 3]).abs()
                + (cfa[i + 2] - cfa[i + 4]).abs();

            let lpf_i = lpf[lp];
            let n_est = cfa[i - W1] * (lpf_i + lpf_i) / (EPS + lpf_i + lpf[lp - W1]);
            let s_est = cfa[i + W1] * (lpf_i + lpf_i) / (EPS + lpf_i + lpf[lp + W1]);
            let w_est = cfa[i - 1] * (lpf_i + lpf_i) / (EPS + lpf_i + lpf[lp - 1]);
            let e_est = cfa[i + 1] * (lpf_i + lpf_i) / (EPS + lpf_i + lpf[lp + 1]);

            let v_est = (s_grad * n_est + n_grad * s_est) / (n_grad + s_grad);
            let h_est = (w_grad * e_est + e_grad * w_est) / (e_grad + w_grad);

            let vh = &b.vh_dir;
            let central = vh[i];
            let around = 0.25 * (vh[i - W1 - 1] + vh[i - W1 + 1] + vh[i + W1 - 1] + vh[i + W1 + 1]);
            let disc = if (0.5 - central).abs() < (0.5 - around).abs() {
                around
            } else {
                central
            };
            b.rgb[1][i] = mix(v_est, h_est, disc.clamp(0.0, 1.0));
            col += 2;
        }
    }

    // Step 4.0: diagonal high pass filters.
    for row in 3..rows - 3 {
        let mut col = 3;
        while col < cols - 3 {
            let i = row * TILE + col;
            let cfa = &b.cfa;
            b.p_hpf[i / 2] = sqr((cfa[i - W3 - 3] - cfa[i - W1 - 1] - cfa[i + W1 + 1]
                + cfa[i + W3 + 3])
                - 3.0 * (cfa[i - W2 - 2] + cfa[i + W2 + 2])
                + 6.0 * cfa[i]);
            b.q_hpf[i / 2] = sqr((cfa[i - W3 + 3] - cfa[i - W1 + 1] - cfa[i + W1 - 1]
                + cfa[i + W3 - 3])
                - 3.0 * (cfa[i - W2 + 2] + cfa[i + W2 - 2])
                + 6.0 * cfa[i]);
            col += 2;
        }
    }

    // Step 4.1: diagonal direction strength.
    for row in 4..rows - 4 {
        let mut col = 4 + (fc(row, 0) & 1);
        while col < cols - 4 {
            let i = row * TILE + col;
            let (i2, i3, i4) = (i / 2, (i - W1 - 1) / 2, (i + W1 - 1) / 2);
            let p_stat = (b.p_hpf[i3] + b.p_hpf[i2] + b.p_hpf[i4 + 1]).max(EPS_SQ);
            let q_stat = (b.q_hpf[i3 + 1] + b.q_hpf[i2] + b.q_hpf[i4]).max(EPS_SQ);
            b.pq_dir[i2] = p_stat / (p_stat + q_stat);
            col += 2;
        }
    }

    // Step 4.2: red and blue at blue and red positions.
    for row in 4..rows - 4 {
        let mut col = 4 + (fc(row, 0) & 1);
        while col < cols - 4 {
            let i = row * TILE + col;
            let c = 2 - fc(row, col);
            let (pq1, pq2, pq3) = (i / 2, (i - W1 - 1) / 2, (i + W1 - 1) / 2);
            let central = b.pq_dir[pq1];
            let around =
                0.25 * (b.pq_dir[pq2] + b.pq_dir[pq2 + 1] + b.pq_dir[pq3] + b.pq_dir[pq3 + 1]);
            let disc = if (0.5 - central).abs() < (0.5 - around).abs() {
                around
            } else {
                central
            };

            let green = &b.rgb[1];
            let other = &b.rgb[c];
            let nw_grad = EPS
                + (other[i - W1 - 1] - other[i + W1 + 1]).abs()
                + (other[i - W1 - 1] - other[i - W3 - 3]).abs()
                + (green[i] - green[i - W2 - 2]).abs();
            let ne_grad = EPS
                + (other[i - W1 + 1] - other[i + W1 - 1]).abs()
                + (other[i - W1 + 1] - other[i - W3 + 3]).abs()
                + (green[i] - green[i - W2 + 2]).abs();
            let sw_grad = EPS
                + (other[i - W1 + 1] - other[i + W1 - 1]).abs()
                + (other[i + W1 - 1] - other[i + W3 - 3]).abs()
                + (green[i] - green[i + W2 - 2]).abs();
            let se_grad = EPS
                + (other[i - W1 - 1] - other[i + W1 + 1]).abs()
                + (other[i + W1 + 1] - other[i + W3 + 3]).abs()
                + (green[i] - green[i + W2 + 2]).abs();

            let nw_est = other[i - W1 - 1] - green[i - W1 - 1];
            let ne_est = other[i - W1 + 1] - green[i - W1 + 1];
            let sw_est = other[i + W1 - 1] - green[i + W1 - 1];
            let se_est = other[i + W1 + 1] - green[i + W1 + 1];

            let p_est = (nw_grad * se_est + se_grad * nw_est) / (nw_grad + se_grad);
            let q_est = (ne_grad * sw_est + sw_grad * ne_est) / (ne_grad + sw_grad);

            let value = green[i] + mix(p_est, q_est, disc.clamp(0.0, 1.0));
            b.rgb[c][i] = value;
            col += 2;
        }
    }

    // Step 4.3: red and blue at green positions.
    for row in 4..rows - 4 {
        let mut col = 4 + (fc(row, 1) & 1);
        while col < cols - 4 {
            let i = row * TILE + col;
            let vh = &b.vh_dir;
            let central = vh[i];
            let around = 0.25 * (vh[i - W1 - 1] + vh[i - W1 + 1] + vh[i + W1 - 1] + vh[i + W1 + 1]);
            let disc = if (0.5 - central).abs() < (0.5 - around).abs() {
                around
            } else {
                central
            };
            let (green_i, n1, s1, w1, e1, g_n, g_s, g_w, g_e) = {
                let g = &b.rgb[1];
                (
                    g[i],
                    EPS + (g[i] - g[i - W2]).abs(),
                    EPS + (g[i] - g[i + W2]).abs(),
                    EPS + (g[i] - g[i - 2]).abs(),
                    EPS + (g[i] - g[i + 2]).abs(),
                    g[i - W1],
                    g[i + W1],
                    g[i - 1],
                    g[i + 1],
                )
            };
            for c in [0, 2] {
                let ch = &b.rgb[c];
                let sn_abs = (ch[i - W1] - ch[i + W1]).abs();
                let ew_abs = (ch[i - 1] - ch[i + 1]).abs();

                let n_grad = n1 + sn_abs + (ch[i - W1] - ch[i - W3]).abs();
                let s_grad = s1 + sn_abs + (ch[i + W1] - ch[i + W3]).abs();
                let w_grad = w1 + ew_abs + (ch[i - 1] - ch[i - 3]).abs();
                let e_grad = e1 + ew_abs + (ch[i + 1] - ch[i + 3]).abs();

                let n_est = ch[i - W1] - g_n;
                let s_est = ch[i + W1] - g_s;
                let w_est = ch[i - 1] - g_w;
                let e_est = ch[i + 1] - g_e;

                let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
                let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);

                let value = green_i + mix(v_est, h_est, disc.clamp(0.0, 1.0));
                b.rgb[c][i] = value;
            }
            col += 2;
        }
    }
}

/// Demosaics a Bayer image into interleaved RGB. `input` holds `width * height`
/// white-balanced values.
pub fn demosaic_rcd(input: &[f32], width: usize, height: usize, pattern: BayerPattern) -> Vec<f32> {
    assert_eq!(input.len(), width * height, "input does not match its size");
    let mut out = vec![0.0f32; width * height * 3];
    demosaic_box(&mut out, input, width, height, pattern);
    if width < TILE.min(2 * BORDER + 8) || height < TILE.min(2 * BORDER + 8) {
        return out;
    }

    let num_vertical = 1 + (height - 2 * BORDER - 1) / TILE_VALID;
    let num_horizontal = 1 + (width - 2 * BORDER - 1) / TILE_VALID;
    let tiles: Vec<(usize, usize)> = (0..num_vertical)
        .flat_map(|v| (0..num_horizontal).map(move |h| (v, h)))
        .collect();

    // Each tile computes without the lock and only holds it to copy its result.
    let shared = Mutex::new(&mut out);
    tiles.par_iter().for_each_init(
        || (TileBuffers::new(), Vec::new()),
        |(buffers, finished), &(tile_v, tile_h)| {
            let row_start = tile_v * TILE_VALID;
            let col_start = tile_h * TILE_VALID;
            let row_end = (row_start + TILE).min(height);
            let col_end = (col_start + TILE).min(width);
            let (rows, cols) = (row_end - row_start, col_end - col_start);
            if rows <= 2 * BORDER || cols <= 2 * BORDER {
                return;
            }
            process_tile(
                buffers,
                input,
                width,
                pattern,
                (row_start, col_start),
                rows,
                cols,
            );

            let first_row = row_start + if tile_v == 0 { MARGIN } else { BORDER };
            let last_row = row_end
                - if tile_v == num_vertical - 1 {
                    MARGIN
                } else {
                    BORDER
                };
            let first_col = col_start + if tile_h == 0 { MARGIN } else { BORDER };
            let last_col = col_end
                - if tile_h == num_horizontal - 1 {
                    MARGIN
                } else {
                    BORDER
                };

            finished.clear();
            for row in first_row..last_row {
                for col in first_col..last_col {
                    let i = (row - row_start) * TILE + col - col_start;
                    finished.push(buffers.rgb[0][i].max(0.0));
                    finished.push(buffers.rgb[1][i].max(0.0));
                    finished.push(buffers.rgb[2][i].max(0.0));
                }
            }

            let line = (last_col - first_col) * 3;
            let mut out = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (index, values) in finished.chunks_exact(line).enumerate() {
                let start = ((first_row + index) * width + first_col) * 3;
                out[start..start + line].copy_from_slice(values);
            }
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATTERNS: [BayerPattern; 4] = [
        BayerPattern([[0, 1], [1, 2]]),
        BayerPattern([[2, 1], [1, 0]]),
        BayerPattern([[1, 0], [2, 1]]),
        BayerPattern([[1, 2], [0, 1]]),
    ];

    fn mosaic(
        width: usize,
        height: usize,
        pattern: BayerPattern,
        scene: impl Fn(usize, usize) -> [f32; 3],
    ) -> Vec<f32> {
        let mut data = Vec::with_capacity(width * height);
        for row in 0..height {
            for col in 0..width {
                data.push(scene(row, col)[pattern.color(row, col)]);
            }
        }
        data
    }

    fn worst_error(
        out: &[f32],
        width: usize,
        height: usize,
        margin: usize,
        scene: impl Fn(usize, usize) -> [f32; 3],
    ) -> f32 {
        let mut worst = 0.0f32;
        for row in margin..height - margin {
            for col in margin..width - margin {
                let want = scene(row, col);
                for c in 0..3 {
                    worst = worst.max((out[(row * width + col) * 3 + c] - want[c]).abs());
                }
            }
        }
        worst
    }

    fn mse(
        out: &[f32],
        width: usize,
        height: usize,
        scene: impl Fn(usize, usize) -> [f32; 3],
    ) -> f32 {
        let mut sum = 0.0f32;
        let mut count = 0.0f32;
        for row in 12..height - 12 {
            for col in 12..width - 12 {
                let want = scene(row, col);
                for c in 0..3 {
                    sum += (out[(row * width + col) * 3 + c] - want[c]).powi(2);
                    count += 1.0;
                }
            }
        }
        sum / count
    }

    #[test]
    fn flat_color_is_recovered_on_every_pattern() {
        for pattern in PATTERNS {
            let scene = |_: usize, _: usize| [0.4, 0.25, 0.1];
            let input = mosaic(300, 280, pattern, scene);
            let out = demosaic_rcd(&input, 300, 280, pattern);
            assert!(worst_error(&out, 300, 280, 0, scene) < 1e-3, "{pattern:?}");
        }
    }

    #[test]
    fn smooth_gradient_shows_no_seams_across_tile_boundaries() {
        // Wider and taller than three tiles, so several boundaries are crossed.
        let (width, height) = (800, 700);
        let scene = |row: usize, col: usize| {
            let v = 0.05 + 0.8 * (row + col) as f32 / (width + height) as f32;
            [v, v * 0.9, v * 0.8]
        };
        let input = mosaic(width, height, PATTERNS[0], scene);
        let out = demosaic_rcd(&input, width, height, PATTERNS[0]);
        assert!(worst_error(&out, width, height, 12, scene) < 2e-3);
    }

    #[test]
    fn result_does_not_depend_on_the_thread_count() {
        let (width, height) = (600, 500);
        let scene = |row: usize, col: usize| {
            let v = ((row / 7 + col / 5) % 2) as f32;
            [0.2 + 0.5 * v, 0.1 + 0.6 * v, 0.3 + 0.2 * v]
        };
        let input = mosaic(width, height, PATTERNS[2], scene);
        let run = |threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| demosaic_rcd(&input, width, height, PATTERNS[2]))
        };
        assert_eq!(run(1), run(4));
    }

    #[test]
    fn sharp_edges_beat_the_box_filter() {
        let (width, height) = (300, 300);
        // A diagonal edge between two neutral tones, the case that causes zipper
        // artifacts in naive demosaicing.
        let scene = |row: usize, col: usize| {
            let v = if row + col < 300 { 0.1 } else { 0.8 };
            [v, v, v]
        };
        for pattern in PATTERNS {
            let input = mosaic(width, height, pattern, scene);
            let rcd = demosaic_rcd(&input, width, height, pattern);
            let mut boxed = vec![0.0; width * height * 3];
            demosaic_box(&mut boxed, &input, width, height, pattern);
            let (rcd_mse, box_mse) = (
                mse(&rcd, width, height, scene),
                mse(&boxed, width, height, scene),
            );
            assert!(
                rcd_mse < box_mse * 0.5,
                "{pattern:?}: rcd {rcd_mse}, box {box_mse}"
            );
        }
    }

    #[test]
    fn negative_input_never_produces_negative_output() {
        let mut input = vec![0.3f32; 300 * 300];
        for value in input.iter_mut().step_by(7) {
            *value = -0.2;
        }
        let out = demosaic_rcd(&input, 300, 300, PATTERNS[0]);
        assert!(out.iter().all(|v| *v >= 0.0 && v.is_finite()));
    }

    #[test]
    fn tiny_images_use_the_fallback_without_panicking() {
        for (width, height) in [(1, 1), (5, 3), (27, 27), (28, 28), (29, 40), (40, 21)] {
            let input = vec![0.5f32; width * height];
            let out = demosaic_rcd(&input, width, height, PATTERNS[0]);
            assert_eq!(out.len(), width * height * 3);
        }
    }
}
