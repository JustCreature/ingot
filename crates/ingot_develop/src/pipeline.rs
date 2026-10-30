//! Runs the develop pipeline in darktable's order.

use std::path::Path;

use rawler::decoders::Orientation;
use rayon::prelude::*;

use crate::{
    DevelopError,
    color::{self, Mat3},
    demosaic::demosaic_rcd,
    sensor::{SensorImage, load_sensor},
    sigmoid::Sigmoid,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WhiteBalance {
    /// As shot, falling back to daylight if the file has none.
    Camera,
    /// D65, from the camera's color matrix.
    Daylight,
    /// Red, green and blue multipliers.
    Custom([f32; 3]),
}

#[derive(Clone, Copy, Debug)]
pub struct DevelopParams {
    /// In stops. Defaults to darktable's scene-referred starting point of +0.7.
    pub exposure: f32,
    pub white_balance: WhiteBalance,
    /// Sigmoid contrast, 0.1 to 10.
    pub contrast: f32,
    /// Sigmoid skew, -1 to 1. Positive favors shadows, negative highlights.
    pub skew: f32,
    /// How far the hue is held through tone mapping, 0 to 1.
    pub hue_preservation: f32,
}

impl Default for DevelopParams {
    fn default() -> Self {
        DevelopParams {
            exposure: 0.7,
            white_balance: WhiteBalance::Camera,
            contrast: 1.5,
            skew: 0.0,
            hue_preservation: 1.0,
        }
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

pub fn develop(
    mut sensor: SensorImage,
    params: &DevelopParams,
) -> Result<DevelopedImage, DevelopError> {
    if !params.exposure.is_finite() {
        return Err("exposure is not a number".into());
    }
    if !(0.1..=10.0).contains(&params.contrast) {
        return Err(format!("contrast {} is outside 0.1 to 10", params.contrast).into());
    }
    if !(-1.0..=1.0).contains(&params.skew) {
        return Err(format!("skew {} is outside -1 to 1", params.skew).into());
    }

    let xyz_to_cam = sensor
        .xyz_to_cam
        .ok_or("this camera has no color matrix in the decoder")?;
    let (to_working, daylight) = color::camera_to_working(xyz_to_cam)
        .ok_or("the camera's color matrix can't be inverted")?;

    let wb = match params.white_balance {
        WhiteBalance::Camera => sensor.camera_wb.unwrap_or(daylight),
        WhiteBalance::Daylight => daylight,
        WhiteBalance::Custom(custom) => custom,
    };
    if wb.iter().any(|m| !m.is_finite() || *m <= 0.0) {
        return Err(format!("invalid white balance {wb:?}").into());
    }

    balance_and_clip(&mut sensor, wb);
    let mut rgb = demosaic_rcd(&sensor.data, sensor.width, sensor.height, sensor.pattern);
    drop(std::mem::take(&mut sensor.data));

    let curve = Sigmoid::new(
        params.contrast,
        params.skew,
        1.0,
        0.000152,
        params.hue_preservation,
    );
    let pixels = render(&mut rgb, 2f32.powf(params.exposure), &to_working, &curve);
    Ok(orient(
        pixels,
        sensor.width,
        sensor.height,
        sensor.orientation,
    ))
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

fn render(rgb: &mut [f32], gain: f32, to_working: &Mat3, curve: &Sigmoid) -> Vec<u8> {
    let mut out = vec![0u8; rgb.len()];
    out.par_chunks_exact_mut(3)
        .zip(rgb.par_chunks_exact(3))
        .for_each(|(dst, src)| {
            let exposed = [src[0] * gain, src[1] * gain, src[2] * gain];
            let working = color::apply(to_working, exposed);
            let display = curve.apply(working);
            let srgb = color::apply(&color::REC2020_TO_SRGB, display);
            for c in 0..3 {
                dst[c] = (color::srgb_encode(srgb[c]) * 255.0 + 0.5) as u8;
            }
        });
    out
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
