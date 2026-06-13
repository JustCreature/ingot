//! The asset domain model: how a photo (RAW and/or JPEG) is identified and
//! represented. Keyed on `(dir, stem)` case-normalized — never stem alone.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

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

/// A non-empty set of the files that make up one asset. There is no `Default`
/// and no public way to build an empty one: an asset always has at least one
/// file. A `Vec` linear scan is fine at current N and stays swappable behind
/// `get`/`insert`.
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

/// Everything we pull out of one EXIF parse: cheap metadata only. The heavy
/// full-size embedded preview is *not* here — it feeds the CPU stage, not the
/// per-asset metadata bundle. Add a new property by adding a field here plus a
/// line in `metadata::extract_exif_data`.
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
    pub(crate) fn new(kind: FileKind, path: PathBuf) -> Self {
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
