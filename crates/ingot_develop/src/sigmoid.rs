// Ported to Rust from darktable (https://github.com/darktable-org/darktable),
// src/iop/sigmoid.c, Copyright (C) the darktable developers, GPL-3.0-or-later.
// Modified by the Ingot contributors (2026).
//! Sigmoid tone mapping: a generalized log-logistic curve fitted so middle grey
//! stays put and the contrast setting alone decides the slope there.

const MIDDLE_GREY: f32 = 0.1845;

/// A fitted curve. `new` takes darktable's parameter ranges, with targets as ratios
/// rather than percent.
#[derive(Clone, Copy, Debug)]
pub struct Sigmoid {
    white_target: f32,
    paper_exposure: f32,
    film_fog: f32,
    film_power: f32,
    paper_power: f32,
    hue_preservation: f32,
}

#[inline]
fn loglogistic(value: f32, magnitude: f32, paper_exp: f32, fog: f32, film: f32, paper: f32) -> f32 {
    let film_response = (fog + value.max(0.0)).powf(film);
    let response = magnitude * (film_response / (paper_exp + film_response)).powf(paper);
    if response.is_nan() {
        magnitude
    } else {
        response
    }
}

impl Default for Sigmoid {
    /// darktable's defaults: contrast 1.5, skew 0, white 100%, black 0.0152%, hue
    /// preservation 100%.
    fn default() -> Self {
        Sigmoid::new(1.5, 0.0, 1.0, 0.000152, 1.0)
    }
}

impl Sigmoid {
    pub fn new(
        contrast: f32,
        skew: f32,
        white_target: f32,
        black_target: f32,
        hue_preservation: f32,
    ) -> Self {
        let delta = 1e-3f32;
        let ref_paper_exposure = MIDDLE_GREY.powf(contrast) * (1.0 / MIDDLE_GREY - 1.0);
        let ref_slope = (loglogistic(
            MIDDLE_GREY + delta,
            1.0,
            ref_paper_exposure,
            0.0,
            contrast,
            1.0,
        ) - loglogistic(
            MIDDLE_GREY - delta,
            1.0,
            ref_paper_exposure,
            0.0,
            contrast,
            1.0,
        )) / 2.0
            / delta;

        let paper_power = 5.0f32.powf(-skew);

        let temp_relation = (white_target / MIDDLE_GREY).powf(1.0 / paper_power) - 1.0;
        let temp_paper_exposure = MIDDLE_GREY * temp_relation;
        let temp_slope = (loglogistic(
            MIDDLE_GREY + delta,
            white_target,
            temp_paper_exposure,
            0.0,
            1.0,
            paper_power,
        ) - loglogistic(
            MIDDLE_GREY - delta,
            white_target,
            temp_paper_exposure,
            0.0,
            1.0,
            paper_power,
        )) / 2.0
            / delta;

        let film_power = ref_slope / temp_slope;

        let white_grey = (white_target / MIDDLE_GREY).powf(1.0 / paper_power) - 1.0;
        let white_black = (black_target / white_target).powf(-1.0 / paper_power) - 1.0;
        let film_fog = MIDDLE_GREY * white_grey.powf(1.0 / film_power)
            / (white_black.powf(1.0 / film_power) - white_grey.powf(1.0 / film_power));
        let paper_exposure = (film_fog + MIDDLE_GREY).powf(film_power) * white_grey;

        Sigmoid {
            white_target,
            paper_exposure,
            film_fog,
            film_power,
            paper_power,
            hue_preservation: hue_preservation.clamp(0.0, 1.0),
        }
    }

    #[inline]
    fn curve(&self, value: f32) -> f32 {
        loglogistic(
            value,
            self.white_target,
            self.paper_exposure,
            self.film_fog,
            self.film_power,
            self.paper_power,
        )
    }

    /// Per-channel tone mapping with hue correction.
    #[inline]
    pub fn apply(&self, pixel: [f32; 3]) -> [f32; 3] {
        let positive = desaturate_negative(pixel);
        let mapped = [
            self.curve(positive[0]),
            self.curve(positive[1]),
            self.curve(positive[2]),
        ];
        preserve_hue_and_energy(
            positive,
            mapped,
            channel_order(positive),
            self.hue_preservation,
        )
    }
}

#[inline]
fn desaturate_negative(pixel: [f32; 3]) -> [f32; 3] {
    let average = ((pixel[0] + pixel[1] + pixel[2]) / 3.0).max(0.0);
    let min = pixel[0].min(pixel[1]).min(pixel[2]);
    let factor = if min < 0.0 {
        -average / (min - average)
    } else {
        1.0
    };
    [
        average + factor * (pixel[0] - average),
        average + factor * (pixel[1] - average),
        average + factor * (pixel[2] - average),
    ]
}

/// Indexes of the (min, mid, max) channels.
#[inline]
fn channel_order(p: [f32; 3]) -> (usize, usize, usize) {
    if p[0] >= p[1] {
        if p[1] > p[2] {
            (2, 1, 0)
        } else if p[2] > p[0] {
            (1, 0, 2)
        } else if p[2] > p[1] {
            (1, 2, 0)
        } else {
            (2, 1, 0)
        }
    } else if p[0] >= p[2] {
        (2, 0, 1)
    } else if p[2] > p[1] {
        (0, 1, 2)
    } else {
        (0, 2, 1)
    }
}

#[inline]
fn preserve_hue_and_energy(
    input: [f32; 3],
    per_channel: [f32; 3],
    (min, mid, max): (usize, usize, usize),
    hue_preservation: f32,
) -> [f32; 3] {
    let chroma = input[max] - input[min];
    let midscale = if chroma != 0.0 {
        (input[mid] - input[min]) / chroma
    } else {
        0.0
    };
    let full = per_channel[min] + (per_channel[max] - per_channel[min]) * midscale;
    let naive_mid = (1.0 - hue_preservation) * per_channel[mid] + hue_preservation * full;

    let per_channel_energy = per_channel[0] + per_channel[1] + per_channel[2];
    let naive_energy = per_channel[min] + naive_mid + per_channel[max];
    let min_plus_mid = input[min] + input[mid];
    let blend = if min_plus_mid != 0.0 {
        2.0 * input[min] / min_plus_mid
    } else {
        0.0
    };
    let energy = blend * per_channel_energy + (1.0 - blend) * naive_energy;

    let mut out = [0.0; 3];
    if naive_mid <= per_channel[mid] {
        let corrected = ((1.0 - hue_preservation) * per_channel[mid]
            + hue_preservation
                * (midscale * per_channel[max] + (1.0 - midscale) * (energy - per_channel[max])))
            / (1.0 + hue_preservation * (1.0 - midscale));
        out[min] = energy - per_channel[max] - corrected;
        out[mid] = corrected;
        out[max] = per_channel[max];
    } else {
        let corrected = ((1.0 - hue_preservation) * per_channel[mid]
            + hue_preservation
                * (per_channel[min] * (1.0 - midscale) + midscale * (energy - per_channel[min])))
            / (1.0 + hue_preservation * midscale);
        out[min] = per_channel[min];
        out[mid] = corrected;
        out[max] = energy - per_channel[min] - corrected;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn curve_hits_its_three_anchor_points() {
        for (contrast, skew) in [(1.5, 0.0), (1.0, 0.5), (3.0, -0.5), (0.8, 0.2)] {
            let curve = Sigmoid::new(contrast, skew, 1.0, 0.000152, 1.0);
            assert!(
                close(curve.curve(MIDDLE_GREY), MIDDLE_GREY, 1e-3),
                "{contrast} {skew}"
            );
            assert!(close(curve.curve(0.0), 0.000152, 1e-4), "{contrast} {skew}");
            assert!(close(curve.curve(1e6), 1.0, 1e-2), "{contrast} {skew}");
        }
    }

    #[test]
    fn curve_only_ever_rises() {
        let curve = Sigmoid::default();
        let mut last = -1.0;
        for step in 0..2000 {
            let value = curve.curve(step as f32 * 0.01);
            assert!(value >= last, "at {step}");
            last = value;
        }
    }

    #[test]
    fn more_contrast_means_a_steeper_midtone() {
        let slope = |contrast| {
            let curve = Sigmoid::new(contrast, 0.0, 1.0, 0.000152, 1.0);
            curve.curve(MIDDLE_GREY * 1.1) - curve.curve(MIDDLE_GREY * 0.9)
        };
        assert!(slope(3.0) > slope(1.5));
        assert!(slope(1.5) > slope(0.8));
    }

    #[test]
    fn neutral_stays_neutral() {
        let curve = Sigmoid::default();
        for value in [0.0, 0.01, 0.1845, 0.5, 2.0, 40.0] {
            let [r, g, b] = curve.apply([value, value, value]);
            assert!(
                close(r, g, 1e-5) && close(g, b, 1e-5),
                "{value}: {r} {g} {b}"
            );
        }
    }

    #[test]
    fn hue_preservation_keeps_the_channel_ratio_position() {
        let curve = Sigmoid::default();
        let input = [0.9, 0.3, 0.1];
        let out = curve.apply(input);
        let before = (input[1] - input[2]) / (input[0] - input[2]);
        let after = (out[1] - out[2]) / (out[0] - out[2]);
        assert!(close(before, after, 0.02), "{before} vs {after}");
    }

    #[test]
    fn every_channel_order_is_identified() {
        for pixel in [
            [3.0, 2.0, 1.0],
            [2.0, 1.0, 3.0],
            [3.0, 1.0, 2.0],
            [2.0, 3.0, 1.0],
            [1.0, 2.0, 3.0],
            [1.0, 3.0, 2.0],
            [1.0, 1.0, 1.0],
        ] {
            let (min, mid, max) = channel_order(pixel);
            assert!(
                pixel[min] <= pixel[mid] && pixel[mid] <= pixel[max],
                "{pixel:?}"
            );
        }
    }

    #[test]
    fn negative_values_are_pulled_into_range_without_nan() {
        let curve = Sigmoid::default();
        for pixel in [
            [-0.5, 0.2, 0.3],
            [-1.0, -1.0, -1.0],
            [0.0, 0.0, 0.0],
            [5e3, -2.0, 1e-9],
        ] {
            let out = curve.apply(pixel);
            assert!(
                out.iter().all(|v| v.is_finite() && *v >= -1e-3),
                "{pixel:?} {out:?}"
            );
        }
    }
}
