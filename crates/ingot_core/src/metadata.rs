//! Capture-time + embedded-image extraction, split by read cost.
//!
//! Stage 1 reads only the file *head* (`read_exif_header`, ~128 KiB): from it we
//! slice the date and the tiny 160x120 IFD1 thumbnail (both resident in the
//! prefix) and *cache a reference* to the full-size IFD0 preview without reading
//! it. Stage 2 (`read_embedded_preview`) seek-reads exactly that ~2 MB strip on
//! demand — never the whole 35 MB file, never a RAW decode. Every failure
//! funnels to `None`: one malformed frame must not abort a batch.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;

use chrono::NaiveDateTime;

use crate::asset::{AssetKey, ExifAssetData, PhotoAsset, PreviewStrip};

pub(crate) type MetadataError = Box<dyn std::error::Error + Send + Sync>;

/// Bytes of the file head we read to parse EXIF metadata. Measured against Canon
/// CR2: the EXIF/IFD directories, `DateTimeOriginal`, and the *entire* 160x120
/// IFD1 thumbnail all live in the first ~72 KiB (the thumb spans 54,424..71,768);
/// the 2 MB full-size preview starts at 71,768 and is deliberately excluded.
/// 128 KiB gives headroom for other bodies without dragging the big preview into
/// the metadata read. JPEG EXIF (APP1) sits at the very front, so the same budget
/// covers it. ~273x less I/O than slurping a 35 MB CR2.
const EXIF_HEADER_PREFIX_LEN: u64 = 128 * 1024;

/// Locate (don't read) the full-size embedded preview in a TIFF-like container
/// (CR2): a single-strip JPEG referenced by `StripOffsets`/`StripByteCounts` in
/// IFD0. These values are inline in the IFD0 directory, so they parse from the
/// 128 KiB header read — stage 1 caches the *reference*, stage 2 seek-reads the
/// bytes. A parallel JPEG has no such IFD0 strip, so this yields `None` for it.
pub fn get_embedded_preview_location(exif: &exif::Exif) -> Option<PreviewStrip> {
    let preview_offset_field = exif.get_field(exif::Tag::StripOffsets, exif::In::PRIMARY)?;
    let preview_len_field = exif.get_field(exif::Tag::StripByteCounts, exif::In::PRIMARY)?;

    let offset = match preview_offset_field.value {
        exif::Value::Long(ref v) => *v.first()? as u64,
        _ => return None,
    };

    let len = match preview_len_field.value {
        exif::Value::Long(ref v) => *v.first()? as usize,
        _ => return None,
    };

    Some(PreviewStrip { offset, len })
}

/// Stage 2: seek-read exactly the embedded-preview strip referenced by `strip`
/// from `path` — one `seek` + one `read_exact`, never the whole file. This is
/// the RAW-only fast path: ~2 MB read instead of kamadak's 35 MB slurp.
///
/// A strip cannot physically extend past the end of its file, so the reference
/// is validated against the file length (a free `fstat`, no content read) before
/// allocating: a malformed `StripByteCounts` funnels to `None` rather than
/// over-allocating. The check bounds the allocation by a fact about the file,
/// not a guessed constant.
pub fn read_embedded_preview(path: &Path, strip: PreviewStrip) -> Result<Vec<u8>, MetadataError> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();

    let is_out_of_bounds = match strip.offset.checked_add(strip.len as u64) {
        Some(end) => end > file_len,
        None => true, // Overflow means it's definitely out of bounds
    };

    if strip.len == 0 || is_out_of_bounds {
        return Err(MetadataError::from(
            "identified jpeg preview length is not allowed",
        ));
    }

    file.seek(std::io::SeekFrom::Start(strip.offset))?;

    let mut buf = vec![0u8; strip.len];
    file.read_exact(&mut buf)?;

    Ok(buf)
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

/// Open one file (JPEG-preferred, RAW-fallback) and parse its EXIF container
/// from only the head of the file — the stage-1 skeleton read.
///
/// `kamadak-exif` slurps the *entire* file for a TIFF-like CR2 (35 MB resident
/// to read a 20-byte date). Bounding the read to `EXIF_HEADER_PREFIX_LEN` keeps
/// the metadata pass header-only. The parser needs `BufRead + Seek`, and
/// `Read::take` yields only `Read` (no `Seek`), so we read the prefix into a
/// `Vec` and hand kamadak a `Cursor` over it. The full-size embedded preview is
/// a separate seek-read (stage 2), never this buffer.
pub(crate) fn read_exif_header(asset: &PhotoAsset) -> Option<exif::Exif> {
    let (_, path) = asset.files.get_prioritized();

    read_exif_header_from_path(path, EXIF_HEADER_PREFIX_LEN)
}

/// Read at most `max_bytes` from the head of `path` and parse it as an EXIF
/// container. `take(max_bytes).read_to_end` caps the read *and* tolerates files
/// shorter than the budget (no `UnexpectedEof`); `with_capacity` makes it a
/// single allocation. A field whose value lands beyond the prefix simply fails
/// to slice later (funnels to `None`); it does not abort the parse.
fn read_exif_header_from_path(path: &Path, max_bytes: u64) -> Option<exif::Exif> {
    let file = std::fs::File::open(path).ok()?;

    let mut head = Vec::with_capacity(max_bytes as usize);
    file.take(max_bytes).read_to_end(&mut head).ok()?;

    exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(head))
        .ok()
}

/// The single extension point: assemble all cheap-metadata properties from one
/// parsed container. Add a property = add a field to `ExifAssetData` + a line
/// here; the exhaustive struct literal makes the compiler enforce it.
pub(crate) fn extract_exif_data(exif: &exif::Exif) -> ExifAssetData {
    ExifAssetData {
        captured_at: get_capture_time(exif),
        thumbnail: get_thumbnail(exif),
        embedded_preview_file_location: get_embedded_preview_location(exif),
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
            read_exif_header(asset)
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
    fn embedded_preview_ref_from_header_then_seek_read_extracts_full_preview() {
        // The 128 KiB header read yields only a
        // *reference* (offset/len, inline in IFD0), then a targeted seek-read
        // pulls the ~2 MB strip — never the whole 35 MB file.
        let path = Path::new("testdata/test_exif_read/IMG_1939.CR2");
        let exif =
            read_exif_header_from_path(path, EXIF_HEADER_PREFIX_LEN).expect("header parse failed");

        let strip = get_embedded_preview_location(&exif).expect("no preview ref in header");
        // Reference points past the 128 KiB header: the data is NOT in the buffer.
        assert!(strip.offset + strip.len as u64 > EXIF_HEADER_PREFIX_LEN);

        let preview = read_embedded_preview(path, strip).expect("seek-read failed");
        assert!(preview.starts_with(&[0xFF, 0xD8]));
        assert!(preview.ends_with(&[0xFF, 0xD9]));
        assert_eq!(
            preview.len(),
            strip.len,
            "seek-read must return exactly the referenced strip length"
        );
        assert!(
            (500_000..6_000_000).contains(&preview.len()),
            "preview size out of expected band: {} bytes",
            preview.len()
        );
    }

    #[test]
    fn read_exif_header_default_prefix_yields_date_and_thumb_without_slurp() {
        // The CR2 is ~35 MB; everything stage 1 needs — DateTimeOriginal and the
        // full 160x120 IFD1 thumbnail (ends at byte 71,768) — is in the first
        // ~72 KiB. Prove the default 128 KiB prefix parses a truncated CR2 and
        // recovers both, so no full-file read is ever required for metadata.
        let path = Path::new("testdata/test_exif_read/IMG_1939.CR2");
        let exif = read_exif_header_from_path(path, EXIF_HEADER_PREFIX_LEN)
            .expect("header parse failed on 128 KiB-truncated CR2");

        let data = extract_exif_data(&exif);
        assert_eq!(
            data.captured_at.expect("no date from prefix").to_string(),
            "2026-01-10 13:05:16"
        );
        let thumb = data.thumbnail.expect("no thumbnail from prefix");
        assert!(thumb.starts_with(&[0xFF, 0xD8]) && thumb.ends_with(&[0xFF, 0xD9]));

        // The big embedded preview's *reference* is cached from the header (the
        // offset/len are inline in IFD0), but the ~2 MB of data itself lies past the 128 KiB prefix.
        let strip = data
            .embedded_preview_file_location
            .expect("preview reference should be cached from the header");
        assert!(
            strip.offset + strip.len as u64 > EXIF_HEADER_PREFIX_LEN,
            "preview data must lie beyond the header read"
        );
    }

    #[test]
    fn read_exif_header_prefix_below_thumb_keeps_date_drops_thumb() {
        // Documents what "only necessary bytes" means: the thumbnail spans
        // 54,424..71,768. A 64 KiB (65,536) prefix covers the IFD directories +
        // date but truncates the thumbnail data, so the date still resolves while
        // the thumb slice fails *gracefully* to None — the failure funnel, never
        // an abort.
        let path = Path::new("testdata/test_exif_read/IMG_1939.CR2");
        let exif =
            read_exif_header_from_path(path, 64 * 1024).expect("header parse failed at 64 KiB");

        let data = extract_exif_data(&exif);
        assert!(
            data.captured_at.is_some(),
            "date must survive a 64 KiB prefix"
        );
        assert!(
            data.thumbnail.is_none(),
            "thumb data extends past 64 KiB, so the slice must funnel to None"
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
