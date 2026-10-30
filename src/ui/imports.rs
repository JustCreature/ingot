//! The list of imports in the left panel, with their names. Kept in one small file in
//! the app's data folder, swapped in whole on each change.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportEntry {
    pub id: u64,
    pub name: String,
    pub destination: PathBuf,
    /// Photos seen while it was open, or kept once finalized.
    pub photos: usize,
    pub finalized: bool,
}

pub struct ImportLog {
    path: PathBuf,
    pub entries: Vec<ImportEntry>,
}

impl ImportLog {
    /// A missing or damaged file means an empty list: this is a convenience, and must
    /// never stop an import.
    pub fn open(path: PathBuf) -> Self {
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        ImportLog { path, entries }
    }

    fn save(&self) -> io::Result<()> {
        write_atomically(&self.path, &serde_json::to_string_pretty(&self.entries)?)
    }

    pub fn add(&mut self, entry: ImportEntry) -> io::Result<()> {
        self.entries.push(entry);
        self.save()
    }

    pub fn update(&mut self, id: u64, change: impl FnOnce(&mut ImportEntry)) -> io::Result<()> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            change(entry);
        }
        self.save()
    }

    /// Blank names are ignored, as in the design.
    pub fn rename(&mut self, id: u64, name: &str) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(());
        }
        self.update(id, |entry| entry.name = name.to_string())
    }

    pub fn total_photos(&self) -> usize {
        self.entries.iter().map(|entry| entry.photos).sum()
    }
}

fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.to_path_buf();
    tmp.as_mut_os_string().push(".tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64) -> ImportEntry {
        ImportEntry {
            id,
            name: format!("import {id}"),
            destination: "/photos".into(),
            photos: 3,
            finalized: false,
        }
    }

    #[test]
    fn imports_and_their_names_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/imports.json");
        let mut log = ImportLog::open(path.clone());
        log.add(entry(1)).unwrap();
        log.add(entry(2)).unwrap();
        log.rename(2, "  Wedding ").unwrap();
        log.rename(2, "   ").unwrap();
        log.update(1, |e| e.finalized = true).unwrap();

        let log = ImportLog::open(path);
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[1].name, "Wedding");
        assert!(log.entries[0].finalized);
        assert_eq!(log.total_photos(), 6);
    }

    #[test]
    fn a_damaged_file_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("imports.json");
        fs::write(&path, "not json").unwrap();
        assert!(ImportLog::open(path).entries.is_empty());
    }
}
