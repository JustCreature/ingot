//! Runs the develop pipeline in darktable's order.

use std::path::Path;

use rawler::decoders::Orientation;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    DevelopError,
    color::{self, Mat3},
    demosaic::demosaic_rcd,
    sensor::{SensorImage, load_sensor},
    sharpen,
    sigmoid::Sigmoid,
};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum WhiteBalance {
    /// As shot, falling back to daylight if the file has none.
    Camera,
    /// D65, from the camera's color matrix.
    Daylight,
    /// Red, green and blue multipliers.
    Custom([f32; 3]),
}

/// Every develop control. Controls without a darktable range use Lightroom's: -100
/// to 100, with 0 doing nothing. Missing fields load as defaults, so settings saved
/// by an older version keep working.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevelopParams {
    /// In stops. Defaults to darktable's scene-referred starting point of +0.7.
    pub exposure: f32,
    pub white_balance: WhiteBalance,
    /// Cooler or warmer, on top of `white_balance`.
    pub temperature: f32,
    /// Greener or more magenta, on top of `white_balance`.
    pub tint: f32,
    /// Sigmoid contrast, 0.1 to 10.
    pub contrast: f32,
    /// Sigmoid skew, -1 to 1. Positive favors shadows, negative highlights.
    pub skew: f32,
    /// How far the hue is held through tone mapping, 0 to 1.
    pub hue_preservation: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub saturation: f32,
    pub vibrance: f32,
    /// Unsharp mask strength, 0 to 2.
    pub sharpen: f32,
}

impl Default for DevelopParams {
    fn default() -> Self {
        DevelopParams {
            exposure: 0.7,
            white_balance: WhiteBalance::Camera,
            temperature: 0.0,
            tint: 0.0,
            contrast: 1.5,
            skew: 0.0,
            hue_preservation: 1.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            saturation: 0.0,
            vibrance: 0.0,
            // darktable's default strength.
            sharpen: 0.5,
        }
    }
}

impl DevelopParams {
    fn validate(&self) -> Result<(), DevelopError> {
        if !self.exposure.is_finite() {
            return Err("exposure is not a number".into());
        }
        if !(0.1..=10.0).contains(&self.contrast) {
            return Err(format!("contrast {} is outside 0.1 to 10", self.contrast).into());
        }
        if !(-1.0..=1.0).contains(&self.skew) {
            return Err(format!("skew {} is outside -1 to 1", self.skew).into());
        }
        for (name, value) in [
            ("temperature", self.temperature),
            ("tint", self.tint),
            ("highlights", self.highlights),
            ("shadows", self.shadows),
            ("whites", self.whites),
            ("blacks", self.blacks),
            ("saturation", self.saturation),
            ("vibrance", self.vibrance),
        ] {
            if !(-100.0..=100.0).contains(&value) {
                return Err(format!("{name} {value} is outside -100 to 100").into());
            }
        }
        if !(0.0..=2.0).contains(&self.sharpen) {
            return Err(format!("sharpen {} is outside 0 to 2", self.sharpen).into());
        }
        Ok(())
    }

    /// The multipliers for this file, with temperature and tint applied.
    fn gains(
        &self,
        camera: Option<[f32; 3]>,
        daylight: [f32; 3],
    ) -> Result<[f32; 3], DevelopError> {
        let base = match self.white_balance {
            WhiteBalance::Camera => camera.unwrap_or(daylight),
            WhiteBalance::Daylight => daylight,
            WhiteBalance::Custom(custom) => custom,
        };
        if base.iter().any(|m| !m.is_finite() || *m <= 0.0) {
            return Err(format!("invalid white balance {base:?}").into());
        }
        // Up to a stop each way at the ends of the sliders.
        let warm = 2f32.powf(self.temperature / 100.0);
        let magenta = 2f32.powf(self.tint / 200.0);
        Ok([
            base[0] * warm * magenta,
            base[1] / magenta,
            base[2] / warm * magenta,
        ])
    }
}

/// An 8-bit sRGB image, rotated upright.
#[derive(Clone, Debug)]
pub struct DevelopedImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

pub fn develop_file(path: &Path, params: &DevelopParams) -> Result<DevelopedImage, DevelopError> {
    develop(load_sensor(path)?, params)
}

/// The full-quality path: demosaics every pixel.
pub fn develop(
    mut sensor: SensorImage,
    params: &DevelopParams,
) -> Result<DevelopedImage, DevelopError> {
    params.validate()?;
    let xyz_to_cam = sensor
        .xyz_to_cam
        .ok_or("this camera has no color matrix in the decoder")?;
    let (to_working, daylight) = color::camera_to_working(xyz_to_cam)
        .ok_or("the camera's color matrix can't be inverted")?;
    let wb = params.gains(sensor.camera_wb, daylight)?;

    balance_and_clip(&mut sensor, wb);
    let rgb = demosaic_rcd(&sensor.data, sensor.width, sensor.height, sensor.pattern);
    drop(std::mem::take(&mut sensor.data));

    let look = Look::new(params, &to_working, None);
    let pixels = render(&rgb, sensor.width, &look, 1.0);
    Ok(orient(
        pixels,
        sensor.width,
        sensor.height,
        sensor.orientation,
    ))
}

/// A small copy of a RAW, kept in camera color with no white balance, so every
/// slider, white balance included, re-renders without touching the file again.
#[derive(Clone, Debug)]
pub struct PreviewBase {
    width: usize,
    height: usize,
    rgb: Vec<f32>,
    to_working: Mat3,
    camera_wb: Option<[f32; 3]>,
    daylight: [f32; 3],
    orientation: Orientation,
    /// Preview width over full width, to keep pixel-sized effects in proportion.
    scale: f32,
}

impl PreviewBase {
    /// Bins each 2x2 Bayer cell into one RGB pixel, then box-filters by a whole
    /// factor until the long edge is at most `long_edge` (never below half-size
    /// binning's resolution divided by that factor).
    pub fn new(sensor: &SensorImage, long_edge: usize) -> Result<Self, DevelopError> {
        let xyz_to_cam = sensor
            .xyz_to_cam
            .ok_or("this camera has no color matrix in the decoder")?;
        let (to_working, daylight) = color::camera_to_working(xyz_to_cam)
            .ok_or("the camera's color matrix can't be inverted")?;

        let (cell_w, cell_h) = (sensor.width / 2, sensor.height / 2);
        if cell_w == 0 || cell_h == 0 {
            return Err("image is too small to preview".into());
        }
        let factor = cell_w.max(cell_h).div_ceil(long_edge.max(1)).max(1);
        let (width, height) = ((cell_w / factor).max(1), (cell_h / factor).max(1));
        let (pattern, source_w) = (sensor.pattern, sensor.width);

        let mut rgb = vec![0.0f32; width * height * 3];
        rgb.par_chunks_mut(width * 3)
            .enumerate()
            .for_each(|(y, line)| {
                for x in 0..width {
                    let mut sum = [0.0f32; 3];
                    let mut count = [0.0f32; 3];
                    for row in (y * factor * 2)..((y + 1) * factor * 2) {
                        for col in (x * factor * 2)..((x + 1) * factor * 2) {
                            let color = pattern.0[row & 1][col & 1] as usize;
                            sum[color] += sensor.data[row * source_w + col];
                            count[color] += 1.0;
                        }
                    }
                    for c in 0..3 {
                        line[x * 3 + c] = sum[c] / count[c].max(1.0);
                    }
                }
            });

        Ok(PreviewBase {
            width,
            height,
            rgb,
            to_working,
            camera_wb: sensor.camera_wb,
            daylight,
            orientation: sensor.orientation,
            scale: width as f32 / sensor.width as f32,
        })
    }

    pub fn render(&self, params: &DevelopParams) -> Result<DevelopedImage, DevelopError> {
        params.validate()?;
        let wb = params.gains(self.camera_wb, self.daylight)?;
        let look = Look::new(params, &self.to_working, Some(wb));
        let pixels = render(&self.rgb, self.width, &look, self.scale);
        Ok(orient(pixels, self.width, self.height, self.orientation))
    }
}

/// White balance followed by highlight clipping. Clipping at the lowest multiplier
/// stops blown highlights from turning magenta; darktable's "clip highlights".
fn balance_and_clip(sensor: &mut SensorImage, wb: [f32; 3]) {
    let clip = wb[0].min(wb[1]).min(wb[2]);
    let pattern = sensor.pattern;
    sensor
        .data
        .par_chunks_mut(sensor.width)
        .enumerate()
        .for_each(|(row, line)| {
            let cells = pattern.0[row & 1];
            let gains = [wb[cells[0] as usize], wb[cells[1] as usize]];
            for (col, value) in line.iter_mut().enumerate() {
                *value = (*value * gains[col & 1]).min(clip);
            }
        });
}

/// Middle grey, the pivot for tone controls.
const SCENE_GREY: f32 = 0.1845;
/// Rec.2020 luma weights.
const LUMA: [f32; 3] = [0.2627, 0.6780, 0.0593];

#[inline]
fn luma(p: [f32; 3]) -> f32 {
    LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2]
}

#[inline]
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The per-pixel part of the pipeline, with its parameters worked out once.
struct Look {
    pre_gains: Option<([f32; 3], f32)>,
    gain: f32,
    to_working: Mat3,
    curve: Sigmoid,
    highlights: f32,
    shadows: f32,
    tone_active: bool,
    saturation: f32,
    vibrance: f32,
    black: f32,
    white: f32,
    sharpen: f32,
}

impl Look {
    fn new(params: &DevelopParams, to_working: &Mat3, wb: Option<[f32; 3]>) -> Self {
        Look {
            pre_gains: wb.map(|wb| (wb, wb[0].min(wb[1]).min(wb[2]))),
            gain: 2f32.powf(params.exposure),
            to_working: *to_working,
            curve: Sigmoid::new(
                params.contrast,
                params.skew,
                1.0,
                0.000152,
                params.hue_preservation,
            ),
            // Up to 1.5 stops at the ends of the sliders.
            highlights: params.highlights / 100.0 * 1.5,
            shadows: params.shadows / 100.0 * 1.5,
            tone_active: params.highlights != 0.0 || params.shadows != 0.0,
            saturation: 1.0 + params.saturation / 100.0,
            vibrance: params.vibrance / 100.0,
            black: params.blacks / 100.0 * -0.1,
            white: 1.0 - params.whites / 100.0 * 0.3,
            sharpen: params.sharpen,
        }
    }

    /// Scene light in, display-linear Rec.2020 out.
    #[inline]
    fn scene(&self, src: [f32; 3]) -> [f32; 3] {
        let camera = match self.pre_gains {
            Some((wb, clip)) => [
                (src[0] * wb[0]).min(clip),
                (src[1] * wb[1]).min(clip),
                (src[2] * wb[2]).min(clip),
            ],
            None => src,
        };
        let exposed = [
            camera[0] * self.gain,
            camera[1] * self.gain,
            camera[2] * self.gain,
        ];
        let mut p = color::apply(&self.to_working, exposed);

        if self.tone_active {
            // Weights stay away from the other end of the range, so highlights and
            // shadows act on their own tones and leave middle grey almost alone.
            let stops = (luma(p).max(1e-6) / SCENE_GREY).log2();
            let lows = 1.0 - smoothstep(-5.0, 0.0, stops);
            let highs = smoothstep(0.0, 3.0, stops);
            let tone = 2f32.powf(self.shadows * lows + self.highlights * highs);
            p = [p[0] * tone, p[1] * tone, p[2] * tone];
        }

        if self.saturation != 1.0 || self.vibrance != 0.0 {
            let y = luma(p);
            let max = p[0].max(p[1]).max(p[2]);
            let min = p[0].min(p[1]).min(p[2]);
            let chroma = if max > 1e-6 {
                ((max - min) / max).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Vibrance works hardest on the colors that have the least already.
            let amount = (self.saturation + self.vibrance * (1.0 - chroma)).max(0.0);
            p = [
                y + (p[0] - y) * amount,
                y + (p[1] - y) * amount,
                y + (p[2] - y) * amount,
            ];
        }
        self.curve.apply(p)
    }

    #[inline]
    fn levels(&self, value: f32) -> f32 {
        (value - self.black) / (self.white - self.black)
    }
}

fn render(rgb: &[f32], width: usize, look: &Look, scale: f32) -> Vec<u8> {
    let mut encoded = vec![0.0f32; rgb.len()];
    encoded
        .par_chunks_exact_mut(3)
        .zip(rgb.par_chunks_exact(3))
        .for_each(|(dst, src)| {
            let display = look.scene([src[0], src[1], src[2]]);
            let srgb = color::apply(&color::REC2020_TO_SRGB, display);
            for c in 0..3 {
                dst[c] = color::srgb_encode(look.levels(srgb[c]));
            }
        });

    if look.sharpen > 0.0 {
        sharpen::unsharp_mask(&mut encoded, width, look.sharpen, scale);
    }

    let mut out = vec![0u8; encoded.len()];
    out.par_iter_mut()
        .zip(encoded.par_iter())
        .for_each(|(dst, src)| {
            *dst = (src.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        });
    out
}

/// Rotates an 8-bit RGB image upright according to an EXIF orientation value.
pub fn orient_exif(pixels: Vec<u8>, width: usize, height: usize, exif: u16) -> DevelopedImage {
    orient(pixels, width, height, Orientation::from_u16(exif))
}

fn orient(
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    orientation: Orientation,
) -> DevelopedImage {
    use Orientation as O;
    let swaps_axes = matches!(
        orientation,
        O::Transpose | O::Rotate90 | O::Transverse | O::Rotate270
    );
    let (out_w, out_h) = if swaps_axes {
        (height, width)
    } else {
        (width, height)
    };
    if matches!(orientation, O::Normal | O::Unknown) {
        return DevelopedImage {
            width,
            height,
            rgb: pixels,
        };
    }

    let (w, h) = (width, height);
    let source = |x: usize, y: usize| -> (usize, usize) {
        match orientation {
            O::HorizontalFlip => (w - 1 - x, y),
            O::Rotate180 => (w - 1 - x, h - 1 - y),
            O::VerticalFlip => (x, h - 1 - y),
            O::Transpose => (y, x),
            O::Rotate90 => (y, h - 1 - x),
            O::Transverse => (w - 1 - y, h - 1 - x),
            O::Rotate270 => (w - 1 - y, x),
            O::Normal | O::Unknown => (x, y),
        }
    };
    let mut rgb = vec![0u8; pixels.len()];
    rgb.par_chunks_mut(out_w * 3)
        .enumerate()
        .for_each(|(y, line)| {
            for x in 0..out_w {
                let (sx, sy) = source(x, y);
                let from = (sy * w + sx) * 3;
                line[x * 3..x * 3 + 3].copy_from_slice(&pixels[from..from + 3]);
            }
        });
    DevelopedImage {
        width: out_w,
        height: out_h,
        rgb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demosaic::BayerPattern;

    const RGGB: BayerPattern = BayerPattern([[0, 1], [1, 2]]);
    /// Rec.2020 primaries as a camera's XYZ matrix would describe them: a "camera"
    /// that sees exactly the working space.
    const IDENTITY_CAMERA: Mat3 = [
        [1.716_651, -0.355_671, -0.253_366],
        [-0.666_684, 1.616_481, 0.015_769],
        [0.017_640, -0.042_771, 0.942_103],
    ];

    fn flat_sensor(width: usize, height: usize, value: f32) -> SensorImage {
        SensorImage {
            width,
            height,
            data: vec![value; width * height],
            pattern: RGGB,
            camera_wb: Some([1.0, 1.0, 1.0]),
            xyz_to_cam: Some(IDENTITY_CAMERA),
            orientation: Orientation::Normal,
        }
    }

    #[test]
    fn middle_grey_develops_to_display_middle_grey() {
        let params = DevelopParams {
            exposure: 0.0,
            ..Default::default()
        };
        let image = develop(flat_sensor(64, 64, 0.1845), &params).unwrap();
        // 18.45% linear is 118 in sRGB.
        for value in &image.rgb {
            assert!((*value as i32 - 118).abs() <= 1, "{value}");
        }
    }

    #[test]
    fn one_stop_of_exposure_doubles_scene_light() {
        let dark = DevelopParams {
            exposure: 0.0,
            ..Default::default()
        };
        let bright = DevelopParams {
            exposure: 1.0,
            ..Default::default()
        };
        let a = develop(flat_sensor(64, 64, 0.1845), &bright).unwrap();
        let b = develop(flat_sensor(64, 64, 0.369), &dark).unwrap();
        assert_eq!(a.rgb, b.rgb);
    }

    #[test]
    fn balance_multiplies_each_cfa_cell_and_clips_at_the_lowest_gain() {
        let mut sensor = flat_sensor(2, 2, 0.25);
        balance_and_clip(&mut sensor, [2.0, 1.0, 1.5]);
        assert_eq!(sensor.data, [0.5, 0.25, 0.25, 0.375]);

        let mut sensor = flat_sensor(2, 2, 1.0);
        balance_and_clip(&mut sensor, [2.0, 1.0, 1.5]);
        assert_eq!(sensor.data, [1.0; 4]);
    }

    #[test]
    fn blown_highlights_stay_neutral_with_a_strong_white_balance() {
        let mut sensor = flat_sensor(64, 64, 1.0);
        sensor.camera_wb = Some([2.2, 1.0, 1.6]);
        let image = develop(sensor, &DevelopParams::default()).unwrap();
        for pixel in image.rgb.chunks(3) {
            assert!((pixel[0] as i32 - pixel[1] as i32).abs() <= 1, "{pixel:?}");
            assert!((pixel[1] as i32 - pixel[2] as i32).abs() <= 1, "{pixel:?}");
        }
    }

    /// Mean of each channel, skipping the fallback-filtered border.
    fn mean_rgb(image: &DevelopedImage) -> [f64; 3] {
        let mut sum = [0.0f64; 3];
        for pixel in image.rgb.chunks(3) {
            for c in 0..3 {
                sum[c] += pixel[c] as f64;
            }
        }
        let n = (image.rgb.len() / 3) as f64;
        [sum[0] / n, sum[1] / n, sum[2] / n]
    }

    /// A color patch on the sensor: red, green and blue cells at these levels.
    fn patch(r: f32, g: f32, b: f32) -> SensorImage {
        let mut sensor = flat_sensor(64, 64, 0.0);
        for row in 0..64 {
            for col in 0..64 {
                sensor.data[row * 64 + col] = match RGGB.0[row & 1][col & 1] {
                    0 => r,
                    1 => g,
                    _ => b,
                };
            }
        }
        sensor
    }

    fn preview(sensor: &SensorImage, params: DevelopParams) -> [f64; 3] {
        let params = DevelopParams {
            sharpen: 0.0,
            ..params
        };
        mean_rgb(
            &PreviewBase::new(sensor, 64)
                .unwrap()
                .render(&params)
                .unwrap(),
        )
    }

    #[test]
    fn preview_matches_the_full_develop_on_flat_color() {
        let params = DevelopParams {
            sharpen: 0.0,
            ..Default::default()
        };
        let full = mean_rgb(&develop(patch(0.3, 0.2, 0.1), &params).unwrap());
        let quick = preview(&patch(0.3, 0.2, 0.1), params);
        for c in 0..3 {
            assert!((full[c] - quick[c]).abs() < 1.5, "{full:?} vs {quick:?}");
        }
    }

    #[test]
    fn preview_size_respects_the_requested_edge_and_keeps_orientation() {
        let mut sensor = flat_sensor(600, 400, 0.2);
        let image = PreviewBase::new(&sensor, 100)
            .unwrap()
            .render(&DevelopParams::default())
            .unwrap();
        assert!(
            image.width <= 100 && image.height <= 100,
            "{}x{}",
            image.width,
            image.height
        );

        sensor.orientation = Orientation::Rotate90;
        let image = PreviewBase::new(&sensor, 100)
            .unwrap()
            .render(&DevelopParams::default())
            .unwrap();
        assert!(image.height > image.width);
    }

    #[test]
    fn temperature_warms_and_cools() {
        let sensor = patch(0.2, 0.2, 0.2);
        let base = preview(&sensor, DevelopParams::default());
        let warm = preview(
            &sensor,
            DevelopParams {
                temperature: 50.0,
                ..Default::default()
            },
        );
        let cool = preview(
            &sensor,
            DevelopParams {
                temperature: -50.0,
                ..Default::default()
            },
        );
        assert!(warm[0] > base[0] && warm[2] < base[2], "{warm:?} {base:?}");
        assert!(cool[0] < base[0] && cool[2] > base[2], "{cool:?} {base:?}");
    }

    #[test]
    fn tint_moves_green_against_magenta() {
        let sensor = patch(0.2, 0.2, 0.2);
        let base = preview(&sensor, DevelopParams::default());
        let magenta = preview(
            &sensor,
            DevelopParams {
                tint: 60.0,
                ..Default::default()
            },
        );
        assert!(magenta[1] < base[1], "{magenta:?} {base:?}");
    }

    #[test]
    fn saturation_at_minus_100_is_neutral() {
        let sensor = patch(0.4, 0.2, 0.1);
        let [r, g, b] = preview(
            &sensor,
            DevelopParams {
                saturation: -100.0,
                ..Default::default()
            },
        );
        assert!((r - g).abs() < 1.5 && (g - b).abs() < 1.5, "{r} {g} {b}");
    }

    #[test]
    fn vibrance_lifts_muted_colors_more_than_vivid_ones() {
        let spread = |c: [f64; 3]| c[0] - c[2];
        let gain = |sensor: &SensorImage| {
            let base = preview(sensor, DevelopParams::default());
            let vib = preview(
                sensor,
                DevelopParams {
                    vibrance: 60.0,
                    ..Default::default()
                },
            );
            spread(vib) / spread(base)
        };
        assert!(gain(&patch(0.22, 0.2, 0.18)) > gain(&patch(0.5, 0.1, 0.02)));
    }

    #[test]
    fn shadows_lift_dark_tones_far_more_than_bright_ones() {
        let lift = |level: f32| {
            let sensor = patch(level, level, level);
            let base = preview(
                &sensor,
                DevelopParams {
                    exposure: 0.0,
                    ..Default::default()
                },
            )[1];
            preview(
                &sensor,
                DevelopParams {
                    exposure: 0.0,
                    shadows: 80.0,
                    ..Default::default()
                },
            )[1] - base
        };
        assert!(lift(0.01) > 10.0, "{}", lift(0.01));
        assert!(lift(0.9) < 2.0, "{}", lift(0.9));
    }

    #[test]
    fn highlights_pull_bright_tones_and_leave_dark_ones() {
        let change = |level: f32| {
            let sensor = patch(level, level, level);
            let base = preview(
                &sensor,
                DevelopParams {
                    exposure: 0.0,
                    ..Default::default()
                },
            )[1];
            base - preview(
                &sensor,
                DevelopParams {
                    exposure: 0.0,
                    highlights: -80.0,
                    ..Default::default()
                },
            )[1]
        };
        assert!(change(0.9) > 5.0, "{}", change(0.9));
        assert!(change(0.01).abs() < 1.0, "{}", change(0.01));
    }

    #[test]
    fn whites_and_blacks_stretch_the_ends() {
        let sensor = patch(0.2, 0.2, 0.2);
        let base = preview(&sensor, DevelopParams::default())[1];
        assert!(
            preview(
                &sensor,
                DevelopParams {
                    whites: 60.0,
                    ..Default::default()
                }
            )[1] > base
        );
        assert!(
            preview(
                &sensor,
                DevelopParams {
                    blacks: -60.0,
                    ..Default::default()
                }
            )[1] < base
        );
    }

    #[test]
    fn settings_from_an_older_version_still_load() {
        let params: DevelopParams = serde_json::from_str(r#"{"exposure": 1.25}"#).unwrap();
        assert_eq!(params.exposure, 1.25);
        assert_eq!(params.contrast, 1.5);

        let json = serde_json::to_string(&DevelopParams::default()).unwrap();
        let back: DevelopParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back, DevelopParams::default());
    }

    #[test]
    fn invalid_settings_are_rejected() {
        let check = |params: DevelopParams| develop(flat_sensor(8, 8, 0.2), &params).is_err();
        assert!(check(DevelopParams {
            contrast: 0.0,
            ..Default::default()
        }));
        assert!(check(DevelopParams {
            skew: 2.0,
            ..Default::default()
        }));
        assert!(check(DevelopParams {
            exposure: f32::NAN,
            ..Default::default()
        }));
        assert!(check(DevelopParams {
            white_balance: WhiteBalance::Custom([0.0, 1.0, 1.0]),
            ..Default::default()
        }));

        let mut no_matrix = flat_sensor(8, 8, 0.2);
        no_matrix.xyz_to_cam = None;
        assert!(develop(no_matrix, &DevelopParams::default()).is_err());
    }

    #[test]
    fn every_orientation_moves_the_marked_pixel_where_it_belongs() {
        // 3x2 image, marker in the top-left pixel.
        let mut pixels = vec![0u8; 18];
        pixels[0] = 255;
        let at = |image: &DevelopedImage| {
            let index = image.rgb.iter().position(|v| *v == 255).unwrap() / 3;
            (
                index % image.width,
                index / image.width,
                image.width,
                image.height,
            )
        };
        for (orientation, want) in [
            (Orientation::Normal, (0, 0, 3, 2)),
            (Orientation::HorizontalFlip, (2, 0, 3, 2)),
            (Orientation::Rotate180, (2, 1, 3, 2)),
            (Orientation::VerticalFlip, (0, 1, 3, 2)),
            (Orientation::Transpose, (0, 0, 2, 3)),
            (Orientation::Rotate90, (1, 0, 2, 3)),
            (Orientation::Transverse, (1, 2, 2, 3)),
            (Orientation::Rotate270, (0, 2, 2, 3)),
        ] {
            assert_eq!(
                at(&orient(pixels.clone(), 3, 2, orientation)),
                want,
                "{orientation:?}"
            );
        }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ingot_core/testdata/test_exif_read")
            .join(name)
    }

    #[test]
    fn real_canon_file_develops_to_a_plausible_image() {
        let image = develop_file(&fixture("IMG_1939.CR2"), &DevelopParams::default()).unwrap();
        assert_eq!(image.rgb.len(), image.width * image.height * 3);
        assert!(
            image.width.max(image.height) > 5000,
            "{}x{}",
            image.width,
            image.height
        );
        let mean = image.rgb.iter().map(|v| *v as f64).sum::<f64>() / image.rgb.len() as f64;
        assert!((20.0..235.0).contains(&mean), "mean {mean}");
    }

    #[test]
    fn portrait_shot_comes_out_upright() {
        let image = develop_file(&fixture("IMG_1800.CR2"), &DevelopParams::default()).unwrap();
        assert_eq!((image.width, image.height), (4160, 6240));
    }

    /// Averages an interleaved RGB image down to a small grid.
    fn grid(rgb: &[u8], width: usize, height: usize) -> Vec<[f64; 3]> {
        const CELLS: (usize, usize) = (60, 40);
        let mut sums = vec![[0.0f64; 3]; CELLS.0 * CELLS.1];
        let mut counts = vec![0.0f64; CELLS.0 * CELLS.1];
        for y in 0..height {
            for x in 0..width {
                let cell = (y * CELLS.1 / height) * CELLS.0 + x * CELLS.0 / width;
                for c in 0..3 {
                    sums[cell][c] += rgb[(y * width + x) * 3 + c] as f64;
                }
                counts[cell] += 1.0;
            }
        }
        for (sum, count) in sums.iter_mut().zip(&counts) {
            for value in sum.iter_mut() {
                *value /= count;
            }
        }
        sums
    }

    #[test]
    fn develop_agrees_with_the_cameras_own_jpeg_of_the_same_shot() {
        let mut sensor = load_sensor(&fixture("IMG_1800.CR2")).unwrap();
        // The stored JPEG pixels aren't rotated either, so compare before rotating.
        sensor.orientation = Orientation::Normal;
        let ours = develop(sensor, &DevelopParams::default()).unwrap();
        let camera = image::open(fixture("IMG_1800.JPG")).unwrap().to_rgb8();
        assert_eq!(
            (camera.width() as usize, camera.height() as usize),
            (ours.width, ours.height)
        );

        let ours = grid(&ours.rgb, ours.width, ours.height);
        let camera = grid(
            camera.as_raw(),
            camera.width() as usize,
            camera.height() as usize,
        );

        // Structure: tones land in the same places.
        let luma = |p: &[f64; 3]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
        let (a, b): (Vec<f64>, Vec<f64>) = ours
            .iter()
            .zip(&camera)
            .map(|(x, y)| (luma(x), luma(y)))
            .unzip();
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let (ma, mb) = (mean(&a), mean(&b));
        let cov: f64 = a.iter().zip(&b).map(|(x, y)| (x - ma) * (y - mb)).sum();
        let var_a: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
        let var_b: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
        let correlation = cov / (var_a * var_b).sqrt();
        assert!(correlation > 0.95, "correlation {correlation}");

        // Color: the overall cast matches. Both are white-balanced as shot.
        let cast = |image: &[[f64; 3]]| {
            let total = image.iter().fold([0.0; 3], |sum, p| {
                [sum[0] + p[0], sum[1] + p[1], sum[2] + p[2]]
            });
            [total[0] / total[1], total[2] / total[1]]
        };
        let (ours_cast, camera_cast) = (cast(&ours), cast(&camera));
        for i in 0..2 {
            assert!(
                (ours_cast[i] - camera_cast[i]).abs() < 0.08,
                "cast {ours_cast:?} vs camera {camera_cast:?}"
            );
        }
    }

    #[test]
    fn file_that_is_not_a_raw_is_an_error() {
        let tmp = std::env::temp_dir().join("ingot_develop_not_a_raw.cr2");
        std::fs::write(&tmp, b"nope").unwrap();
        let result = develop_file(&tmp, &DevelopParams::default());
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_err());
    }
}
