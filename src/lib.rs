pub mod preview;
use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
};

use chrono::{Datelike, NaiveDate, NaiveDateTime};

#[derive(Debug)]
pub struct Collision {
    pub key: AssetKey,
    pub kind: FileKind,
    pub kept: PathBuf,
    pub displaced: PathBuf,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum FileKind {
    Raw,
    Jpeg,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct AssetKey {
    pub dir: PathBuf,
    pub stem: String,
}

#[derive(Debug)]
pub struct AssetFiles {
    files: Vec<(FileKind, PathBuf)>,
}

impl AssetFiles {
    pub fn new(kind: FileKind, path: PathBuf) -> Self {
        AssetFiles {
            files: vec![(kind, path)],
        }
    }

    pub fn insert(&mut self, kind: FileKind, path: PathBuf) -> Result<(), PathBuf> {
        let found = self.get(kind);
        if found.is_some() {
            return Err(path);
        }

        self.files.push((kind, path));

        Ok(())
    }

    pub fn get(&self, kind: FileKind) -> Option<&Path> {
        for existing in &self.files {
            if existing.0 == kind {
                return Some(existing.1.as_path());
            }
        }

        None
    }
}

#[derive(Debug)]
pub enum TriageState {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug)]
pub struct ExifAssetData {
    pub captured_at: Option<NaiveDateTime>,
    pub thumbnail: Option<Vec<u8>>,
}

#[derive(Debug)]
pub struct PhotoAsset {
    pub files: AssetFiles,
    pub state: TriageState,
    pub rating: u8,
    pub exif_data: ExifAssetData,
}

impl PhotoAsset {
    fn new(kind: FileKind, path: PathBuf) -> Self {
        PhotoAsset {
            files: AssetFiles::new(kind, path),
            state: TriageState::Pending,
            rating: 0,
            exif_data: ExifAssetData {
                captured_at: None,
                thumbnail: None,
            },
        }
    }
}

#[derive(Debug)]
pub struct ScanResponse {
    pub assets: HashMap<AssetKey, PhotoAsset>,
    pub collisions: Vec<Collision>,
    pub errors: Vec<walkdir::Error>,
}

#[derive(Clone, Debug)]
pub enum TargetKind {
    LocalNvme,
    LocalSpinning,
    Network,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub root: PathBuf,
    pub kind: TargetKind,
    pub write_permits: usize,
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

fn read_exif_container(asset: &PhotoAsset) -> Option<exif::Exif> {
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

fn extract_exif_data(exif: &exif::Exif) -> ExifAssetData {
    ExifAssetData {
        captured_at: get_capture_time(exif),
        thumbnail: get_thumbnail(exif),
    }
}

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

pub fn build_destination_path(
    target: &Target,
    captured: NaiveDate,
    file: &Path,
) -> Option<PathBuf> {
    let year_dir = captured.year().to_string();
    let last_dir = captured.format("%Y-%m-%d").to_string();
    let file_name = file.file_name()?;
    Some(target.root.join(year_dir).join(last_dir).join(file_name))
}

pub fn scan_source_dir(source: &Path) -> ScanResponse {
    let mut asset_map: HashMap<AssetKey, PhotoAsset> = HashMap::new();
    let mut collisions: Vec<Collision> = vec![];
    let mut errors: Vec<walkdir::Error> = vec![];
    for e in walkdir::WalkDir::new(source) {
        match e {
            Err(e) => errors.push(e),
            Ok(e) => {
                let Some(ext) = e.path().extension() else {
                    continue;
                };
                let Some(kind) = classify(&ext.to_string_lossy()) else {
                    continue;
                };
                let Some(dir) = e.path().parent() else {
                    continue;
                };
                let key: AssetKey = AssetKey {
                    dir: dir.to_path_buf(),
                    stem: e
                        .path()
                        .file_stem()
                        .expect("path stem error")
                        .to_string_lossy()
                        .to_lowercase(),
                };

                let path = e.path().to_path_buf();
                match asset_map.entry(key) {
                    Entry::Occupied(mut o) => {
                        if let Err(displaced) = o.get_mut().files.insert(kind, path) {
                            let Some(kept_file) = o.get().files.get(kind) else {
                                continue;
                            };
                            collisions.push(Collision {
                                key: o.key().clone(),
                                kind,
                                kept: kept_file.to_path_buf(),
                                displaced,
                            });
                        };
                    }
                    Entry::Vacant(v) => {
                        v.insert(PhotoAsset::new(kind, path));
                    }
                }
            }
        };
    }
    ScanResponse {
        assets: asset_map,
        collisions,
        errors,
    }
}

fn classify(extension: &str) -> Option<FileKind> {
    let ext: String = extension.to_lowercase();
    match ext.as_str() {
        "cr2" => Some(FileKind::Raw),
        "jpg" | "jpeg" => Some(FileKind::Jpeg),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs::{self, File},
    };

    use super::*;

    fn build_tree(root: &Path, files: &[&str]) {
        if let Err(err) = fs::create_dir_all(root) {
            panic!("error building test tree: {err}")
        };

        for file in files {
            if let Err(err) = File::create(root.join(file)) {
                panic!("error creating test file: {err}")
            };
        }
    }

    fn build_default_dir_structure(tmp_dir: &Path) {
        build_tree(
            tmp_dir.join("source/DCIM/100CANON").as_path(),
            &[
                "IMG_1800.CR2",
                "IMG_1800.JPG",
                "IMG_1868.CR2",
                "IMG_1868.JPG",
                "IMG_1875.CR2",
                "IMG_1875.JPG",
                "IMG_1881.CR2",
                "IMG_1881.JPG",
                "IMG_1891.CR2",
                "IMG_1891.JPG",
                "IMG_1907.CR2",
                "IMG_1907.JPG",
                "IMG_1939.CR2",
                "IMG_1915.JPG",
            ],
        );

        build_tree(
            tmp_dir.join("source/DCIM/101CANON").as_path(),
            &[
                "IMG_1800.CR2",
                "IMG_1800.JPG",
                "IMG_1939.CR2",
                "IMG_1915.JPG",
            ],
        );
    }

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

    #[test]
    fn build_target_dir_test() {
        let target = Target {
            root: "/test_target_root".into(),
            kind: TargetKind::LocalNvme,
            write_permits: 16,
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        let got =
            build_destination_path(&target, date, Path::new("card/DCIM/100CANON/IMG_001.CR2"));
        assert!(got.is_some());
        assert_eq!(
            got.unwrap(),
            PathBuf::from("/test_target_root/2026/2026-05-22/IMG_001.CR2")
        );
    }

    #[test]
    fn scans_test_tree_into_11_assets() {
        let tmp_dir = tempfile::tempdir().expect("error creating tmp test dir");
        build_default_dir_structure(tmp_dir.path());

        // let result: ScanResponse = scan_source_dir(Path::new("photodata/testing/source"));
        let result: ScanResponse = scan_source_dir(tmp_dir.path());

        let result_assets: Vec<PhotoAsset> = result.assets.into_values().collect();
        assert!(result.errors.is_empty());
        assert!(result.collisions.is_empty());
        assert_eq!(result_assets.len(), 11);

        let mut double_assets = 0;
        let mut jpg_only_assets = 0;
        let mut raw_only_assets = 0;
        for asset in result_assets {
            if asset.files.get(FileKind::Jpeg).is_some() && asset.files.get(FileKind::Raw).is_some()
            {
                double_assets += 1;
            } else if asset.files.get(FileKind::Jpeg).is_some()
                && asset.files.get(FileKind::Raw).is_none()
            {
                jpg_only_assets += 1;
            } else if asset.files.get(FileKind::Jpeg).is_none()
                && asset.files.get(FileKind::Raw).is_some()
            {
                raw_only_assets += 1;
            }
        }
        assert_eq!(double_assets, 7);
        assert_eq!(jpg_only_assets, 2);
        assert_eq!(raw_only_assets, 2);
    }

    #[test]
    fn scans_test_tree_spot_collision() {
        let tmp_dir = tempfile::tempdir().expect("error creating tmp test dir");

        build_tree(
            tmp_dir.path().join("source/DCIM/100CANON").as_path(),
            &[
                "IMG_1800.JPEG",
                "IMG_1800.JPG",
                "IMG_1868.CR2",
                "IMG_1868.JPG",
            ],
        );

        let result: ScanResponse = scan_source_dir(tmp_dir.path());

        let result_asset: Vec<PhotoAsset> = result.assets.into_values().collect();
        assert!(result.errors.is_empty());
        assert_eq!(result_asset.len(), 2);
        assert_eq!(result.collisions.len(), 1);
        assert_eq!(result.collisions[0].kind, FileKind::Jpeg);
        let got: BTreeSet<_> = BTreeSet::from([
            result.collisions[0].kept.clone(),
            result.collisions[0].displaced.clone(),
        ]);
        let want: BTreeSet<_> = BTreeSet::from([
            tmp_dir.path().join("source/DCIM/100CANON/IMG_1800.JPEG"),
            tmp_dir.path().join("source/DCIM/100CANON/IMG_1800.JPG"),
        ]);
        assert_eq!(got, want);
    }
}
