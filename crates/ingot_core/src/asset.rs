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

    /// Returns a tuple of (FileKind, &Path) according to a priority, RAW -> JPEG -> None.
    pub fn get_prioritized(&self) -> (FileKind, &Path) {
        if let Some(path) = self.get(FileKind::Raw) {
            return (FileKind::Raw, path);
        }
        let path = self.get(FileKind::Jpeg);
        (
            FileKind::Jpeg,
            path.expect("asset should always have at least one file"),
        )
    }
}

#[derive(Debug)]
pub enum TriageState {
    Pending,
    Accepted,
    Rejected,
}

/// Where the full-size embedded preview lives inside its file, as a byte range —
/// *not* the bytes themselves. The 128 KiB header read parses these inline IFD0
/// values cheaply; stage 2 then seek-reads exactly this range (see
/// `metadata::read_embedded_preview`). Offset is relative to the start of the
/// file the EXIF was parsed from (the RAW, for a RAW-only asset).
#[derive(Debug, Clone, Copy)]
pub struct PreviewStrip {
    pub offset: u64,
    pub len: usize,
}

pub(crate) type ImageProcessingError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct ImageData {
    pub preview: Option<Vec<u8>>,
    pub thumb: Option<Vec<u8>>,
    pub full: Option<Vec<u8>>,
    pub errors: Option<Vec<ImageProcessingError>>,
}

/// Everything we pull out of one EXIF parse: cheap metadata only. The heavy
/// full-size embedded preview is *not* here — only a `PreviewStrip` *reference*
/// to it, so the metadata pass stays header-only. The bytes feed the CPU stage
/// via a separate seek-read. Add a new property by adding a field here plus a
/// line in `metadata::extract_exif_data`.
#[derive(Debug, Clone)]
pub struct ExifAssetData {
    pub captured_at: Option<NaiveDateTime>,
    pub thumbnail: Option<Vec<u8>>,
    pub embedded_preview_file_location: Option<PreviewStrip>,
}

#[derive(Debug)]
pub struct PhotoAsset {
    pub files: AssetFiles,
    pub state: TriageState,
    pub rating: u8,
    pub exif_data: ExifAssetData,
    pub image_data: ImageData,
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
                embedded_preview_file_location: None,
            },
            image_data: ImageData {
                preview: None,
                thumb: None,
                full: None,
                errors: None,
            },
        }
    }
}
