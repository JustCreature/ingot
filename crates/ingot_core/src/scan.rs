//! Source-card enumeration: walk a directory tree and pair files into assets.
//!
//! Batch anomalies (duplicate-kind collisions, walkdir errors) are *data to
//! collect and return*, never errors that abort the scan — one bad frame must
//! not kill a 10k-card ingest.

use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
};

use crate::asset::{AssetKey, FileKind, PhotoAsset};

#[derive(Debug)]
pub struct Collision {
    pub key: AssetKey,
    pub kind: FileKind,
    pub kept: PathBuf,
    pub displaced: PathBuf,
}

#[derive(Debug)]
pub struct ScanResponse {
    pub assets: HashMap<AssetKey, PhotoAsset>,
    pub collisions: Vec<Collision>,
    pub errors: Vec<walkdir::Error>,
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
    use std::collections::BTreeSet;

    use super::*;
    use crate::asset::FileKind;
    use crate::test_support::{build_default_dir_structure, build_tree};

    #[test]
    fn scans_test_tree_into_11_assets() {
        let tmp_dir = tempfile::tempdir().expect("error creating tmp test dir");
        build_default_dir_structure(tmp_dir.path());

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
