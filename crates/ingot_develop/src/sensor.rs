// Ported to Rust from darktable (https://github.com/darktable-org/darktable),
// src/iop/rawprepare.c, Copyright (C) the darktable developers, GPL-3.0-or-later.
// Modified by the Ingot contributors (2026).
//! Loads sensor data and applies raw prepare: crop to the image area, subtract
//! black and scale to white. Decoding itself is rawler's.

use std::path::Path;

use rawler::{
    decoders::{Orientation, RawDecodeParams},
    imgop::xyz::Illuminant,
    rawimage::{RawImage, RawImageData, RawPhotometricInterpretation},
    rawsource::RawSource,
};
use rayon::prelude::*;

use crate::{DevelopError, demosaic::BayerPattern};

/// Normalized Bayer data, ready for white balance. Black is 0 and white is 1.
#[derive(Clone, Debug)]
pub struct SensorImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
    pub pattern: BayerPattern,
    /// As-shot white balance, green normalized to 1. `None` if the file has none.
    pub camera_wb: Option<[f32; 3]>,
    /// XYZ to camera, for the first three camera channels.
    pub xyz_to_cam: Option<[[f32; 3]; 3]>,
    pub orientation: Orientation,
}

pub fn load_sensor(path: &Path) -> Result<SensorImage, DevelopError> {
    let decode = || -> Result<_, DevelopError> {
        let source = RawSource::new(path)?;
        let decoder = rawler::get_decoder(&source)?;
        let params = RawDecodeParams::default();
        let raw = decoder.raw_image(&source, &params, false)?;
        // Some decoders, CR2 among them, leave `raw.orientation` at Normal and only
        // report the real value in the EXIF metadata.
        let exif_orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|metadata| metadata.exif.orientation)
            .map(Orientation::from_u16)
            .filter(|orientation| *orientation != Orientation::Unknown);
        Ok((raw, exif_orientation))
    };
    let (raw, exif_orientation) =
        decode().map_err(|err| format!("could not decode {}: {err}", path.display()))?;
    let mut sensor = prepare(raw, path)?;
    if let Some(orientation) = exif_orientation {
        sensor.orientation = orientation;
    }
    Ok(sensor)
}

fn prepare(raw: RawImage, path: &Path) -> Result<SensorImage, DevelopError> {
    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return Err(format!(
            "{}: demosaiced and linear RAW files are not supported yet",
            path.display()
        )
        .into());
    };
    if raw.cpp != 1 || config.cfa.width != 2 || config.cfa.height != 2 {
        return Err(format!(
            "{}: this sensor layout ({}x{} {}) is not supported yet",
            path.display(),
            config.cfa.width,
            config.cfa.height,
            config.cfa.name
        )
        .into());
    }

    // The usable area: the crop if the file gives one, else the active area.
    let area = raw.crop_area.or(raw.active_area);
    let (x0, y0, width, height) = match area {
        Some(rect) => (rect.p.x, rect.p.y, rect.d.w, rect.d.h),
        None => (0, 0, raw.width, raw.height),
    };
    if x0 + width > raw.width || y0 + height > raw.height || width == 0 || height == 0 {
        return Err(format!("{}: image area is outside the sensor data", path.display()).into());
    }

    let cell = |row: usize, col: usize| {
        let color = config.cfa.color_at(row + y0, col + x0);
        // Four-color sensors label the second green 3.
        if color > 2 { 1 } else { color as u8 }
    };
    let pattern = BayerPattern([[cell(0, 0), cell(0, 1)], [cell(1, 0), cell(1, 1)]]);

    let black = raw.blacklevel.as_bayer_array();
    let white = raw.whitelevel.as_bayer_array();
    let mut scale = [0.0f32; 4];
    for i in 0..4 {
        let range = white[i] - black[i];
        if range <= 0.0 {
            return Err(format!("{}: white level is not above black level", path.display()).into());
        }
        scale[i] = 1.0 / range;
    }

    // Black and white levels are indexed by the position on the full sensor.
    let full_width = raw.width;
    let mut data = vec![0.0f32; width * height];
    let normalize_line = |row: usize, line: &mut [f32], read: &(dyn Fn(usize) -> f32 + Sync)| {
        let sensor_row = row + y0;
        let start = sensor_row * full_width + x0;
        for (col, out) in line.iter_mut().enumerate() {
            let i = (sensor_row & 1) * 2 + ((col + x0) & 1);
            *out = (read(start + col) - black[i]) * scale[i];
        }
    };
    match &raw.data {
        RawImageData::Integer(values) => {
            data.par_chunks_mut(width)
                .enumerate()
                .for_each(|(row, line)| {
                    normalize_line(row, line, &|i| values[i] as f32);
                });
        }
        RawImageData::Float(values) => {
            data.par_chunks_mut(width)
                .enumerate()
                .for_each(|(row, line)| {
                    normalize_line(row, line, &|i| values[i]);
                });
        }
    }

    let camera_wb = {
        let [r, g, b, _] = raw.wb_coeffs;
        (r.is_finite() && g.is_finite() && b.is_finite() && r > 0.0 && g > 0.0 && b > 0.0)
            .then(|| [r / g, 1.0, b / g])
    };

    let xyz_to_cam = raw
        .color_matrix
        .get(&Illuminant::D65)
        .or_else(|| raw.color_matrix.values().next())
        .filter(|matrix| matrix.len() >= 9)
        .map(|m| [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]]);

    Ok(SensorImage {
        width,
        height,
        data,
        pattern,
        camera_wb,
        xyz_to_cam,
        orientation: raw.orientation,
    })
}
