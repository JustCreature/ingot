//! Capture-time + embedded-image extraction from one EXIF parse.
//!
//! Two reaches, one slice idiom: IFD1 (`In::THUMBNAIL`) holds the tiny 160x120
//! thumbnail; IFD0 (`In::PRIMARY`) holds the full-size embedded preview. Both
//! are byte-slices out of the resident TIFF buffer — extract, never decode RAW.
//! Every failure funnels to `None`: one malformed frame must not abort a batch.

use std::collections::HashMap;

use chrono::NaiveDateTime;

use crate::asset::{AssetKey, ExifAssetData, FileKind, PhotoAsset};

/// Full-size embedded preview from a TIFF-like container (CR2): a single-strip
/// JPEG referenced by `StripOffsets`/`StripByteCounts` in IFD0. Feeds the same
/// preview pipeline as a parallel JPEG — the RAW-only path converges here.
pub fn get_embedded_preview_from_tiff_like(exif: &exif::Exif) -> Option<Vec<u8>> {
    let preview_offset_field = exif.get_field(exif::Tag::StripOffsets, exif::In::PRIMARY)?;
    let preview_len_field = exif.get_field(exif::Tag::StripByteCounts, exif::In::PRIMARY)?;

    let preview_offset = match preview_offset_field.value {
        exif::Value::Long(ref v) => *v.first()? as usize,
        _ => return None,
    };

    let preview_len = match preview_len_field.value {
        exif::Value::Long(ref v) => *v.first()? as usize,
        _ => return None,
    };

    let buf = exif.buf();
    let preview = buf.get(preview_offset..preview_offset + preview_len)?;

    Some(preview.to_vec())
}

fn get_thumbnail(exif: &exif::Exif) -> Option<Vec<u8>> {
    let thumb_offset_field =
        exif.get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?;
    let thumb_len_field =
        exif.get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?;

    let thumb_offset = match thumb_offset_field.value {
        exif::Value::Long(ref v) => *v.first()? as usize,
        _ => return None,
    };

    let thumb_len = match thumb_len_field.value {
        exif::Value::Long(ref v) => *v.first()? as usize,
        _ => return None,
    };

    let buf = exif.buf();
    let thumb = buf.get(thumb_offset..thumb_offset + thumb_len)?;

    Some(thumb.to_vec())
}

fn get_capture_time(exif: &exif::Exif) -> Option<NaiveDateTime> {
    let capture_date_time_field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;

    let capture_date_time_val_bytes = match capture_date_time_field.value {
        exif::Value::Ascii(ref vec) => vec.first()?,
        _ => return None,
    };

    let capture_date_time_str_val = std::str::from_utf8(capture_date_time_val_bytes).ok()?;

    let capture_date_time =
        NaiveDateTime::parse_from_str(capture_date_time_str_val, "%Y:%m:%d %H:%M:%S").ok()?;

    Some(capture_date_time)
}

/// Open one file (JPEG-preferred, RAW-fallback) and parse its EXIF container.
pub(crate) fn read_exif_container(asset: &PhotoAsset) -> Option<exif::Exif> {
    let file = std::fs::File::open(
        asset
            .files
            .get(FileKind::Jpeg)
            .or_else(|| asset.files.get(FileKind::Raw))?,
    )
    .ok()?;

    let mut bufreader = std::io::BufReader::new(file);

    let exif_container = exif::Reader::new()
        .read_from_container(&mut bufreader)
        .ok()?;

    Some(exif_container)
}

/// The single extension point: assemble all cheap-metadata properties from one
/// parsed container. Add a property = add a field to `ExifAssetData` + a line
/// here; the exhaustive struct literal makes the compiler enforce it.
pub(crate) fn extract_exif_data(exif: &exif::Exif) -> ExifAssetData {
    ExifAssetData {
        captured_at: get_capture_time(exif),
        thumbnail: get_thumbnail(exif),
    }
}

/// One open / one parse / drop-after-use per asset: the container is consumed
/// inside the closure and dropped before the next asset, so memory stays
/// bounded even for multi-MB CR2 buffers. This is the serial precursor to the
/// step-4 streaming loop.
pub fn enrich_assets(assets: &mut HashMap<AssetKey, PhotoAsset>) {
    let asset_keys_with_exif_data: Vec<(AssetKey, ExifAssetData)> = assets
        .iter()
        .filter_map(|(key, asset)| {
            read_exif_container(asset)
                .map(|exif_container| (key.clone(), extract_exif_data(&exif_container)))
        })
        .collect();

    for (key, exif_data) in asset_keys_with_exif_data.into_iter() {
        let Some(asset) = assets.get_mut(&key) else {
            continue;
        };
        asset.exif_data = exif_data;
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::scan::{ScanResponse, scan_source_dir};
    use crate::test_support::build_default_dir_structure;

    #[test]
    fn enrich_assets_test_no_exif_container_exif_data_not_filled_no_error() {
        let tmp_dir = tempfile::tempdir().expect("error creating tmp test dir");
        build_default_dir_structure(tmp_dir.path());

        let mut result: ScanResponse = scan_source_dir(tmp_dir.path());
        enrich_assets(&mut result.assets);

        let result_assets: Vec<PhotoAsset> = result.assets.into_values().collect();
        for asset in result_assets {
            assert!(asset.exif_data.captured_at.is_none());
            assert!(asset.exif_data.thumbnail.is_none());
        }
    }

    #[test]
    fn get_embedded_preview_from_tiff_like_test_preview_extracted_successfully() {
        let file = std::fs::File::open(Path::new("testdata/test_exif_read/IMG_1939.CR2"))
            .ok()
            .unwrap();

        let mut bufreader = std::io::BufReader::new(file);

        let exif_container = exif::Reader::new()
            .read_from_container(&mut bufreader)
            .ok()
            .unwrap();
        let preview =
            get_embedded_preview_from_tiff_like(&exif_container).expect("no preview extracted");
        assert!(preview.starts_with(&[0xFF, 0xD8]));
        assert!(preview.ends_with(&[0xFF, 0xD9]));
        assert!(
            (500_000..6_000_000).contains(&preview.len()),
            "preview size out of expected band: {} bytes",
            preview.len()
        );
    }

    #[test]
    fn enrich_assets_test_thumbnail_filled_successfully() {
        let root_dir = "testdata/test_exif_read";
        let mut result: ScanResponse = scan_source_dir(Path::new(root_dir));

        enrich_assets(&mut result.assets);
        let got_thumb_from_jpeg = &result
            .assets
            .get(&AssetKey {
                dir: PathBuf::from(root_dir),
                stem: "img_1868".to_string(),
            })
            .expect("no asset found")
            .exif_data
            .thumbnail;
        assert!(got_thumb_from_jpeg.is_some());
        let thumb = got_thumb_from_jpeg.as_ref().expect("thumbnail missing");
        assert!(thumb.starts_with(&[0xFF, 0xD8]));
        assert!(thumb.ends_with(&[0xFF, 0xD9]));
        assert!(
            (2_000..64_000).contains(&thumb.len()),
            "thumb size out of expected band: {} bytes",
            thumb.len()
        );
        let got_thumb_from_cr2 = &result
            .assets
            .get(&AssetKey {
                dir: PathBuf::from(root_dir),
                stem: "img_1939".to_string(),
            })
            .expect("no asset found")
            .exif_data
            .thumbnail;
        assert!(got_thumb_from_cr2.is_some());
        let thumb = got_thumb_from_cr2.as_ref().expect("thumbnail missing");
        assert!(thumb.starts_with(&[0xFF, 0xD8]));
        assert!(thumb.ends_with(&[0xFF, 0xD9]));
        assert!(
            (2_000..64_000).contains(&thumb.len()),
            "thumb size out of expected band: {} bytes",
            thumb.len()
        );
    }

    #[test]
    fn enrich_assets_test_captured_at_filled_successfully() {
        let root_dir = "testdata/test_exif_read";
        let mut result: ScanResponse = scan_source_dir(Path::new(root_dir));

        enrich_assets(&mut result.assets);
        assert_eq!(
            result
                .assets
                .get(&AssetKey {
                    dir: PathBuf::from(root_dir),
                    stem: "img_1800".to_string()
                })
                .expect("no asset found")
                .exif_data
                .captured_at
                .expect("no captured_at field value found")
                .to_string(),
            "2026-01-10 12:26:35"
        );
        assert_eq!(
            result
                .assets
                .get(&AssetKey {
                    dir: PathBuf::from(root_dir),
                    stem: "img_1868".to_string()
                })
                .expect("no asset found")
                .exif_data
                .captured_at
                .expect("no captured_at field value found")
                .to_string(),
            "2026-01-10 12:46:27"
        );
        assert_eq!(
            // CR2 only file, this case tests the fallback raw dt read after jpg not found
            result
                .assets
                .get(&AssetKey {
                    dir: PathBuf::from(root_dir),
                    stem: "img_1939".to_string()
                })
                .expect("no asset found")
                .exif_data
                .captured_at
                .expect("no captured_at field value found")
                .to_string(),
            "2026-01-10 13:05:16"
        );
    }
}
