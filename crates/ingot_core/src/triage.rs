//! Culling decisions: kept outside the photo files so they work before any copy
//! exists and survive a restart, then written as XMP sidecars next to the copies.
//!
//! The source card is never written to.

use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::TriageState;

type TriageError = Box<dyn std::error::Error + Send + Sync>;

/// Identifies a photo without trusting its folder: a card can be read from any
/// mount point, and two cards can reuse a name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TriageKey {
    pub stem: String,
    pub taken: Option<NaiveDateTime>,
}

/// One photo's decisions. `develop` is a settings blob the store doesn't interpret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriageRecord {
    pub rating: u8,
    pub state: TriageState,
    #[serde(default)]
    pub develop: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct Line {
    key: TriageKey,
    record: TriageRecord,
}

/// An append-only log, so a crash can lose at most the last decision. The newest
/// line for a key wins.
pub struct TriageStore {
    path: PathBuf,
    records: HashMap<TriageKey, TriageRecord>,
}

impl TriageStore {
    pub fn open(path: PathBuf) -> Result<Self, TriageError> {
        let records = match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter_map(|line| serde_json::from_str::<Line>(line).ok())
                .map(|line| (line.key, line.record))
                .collect(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => return Err(format!("could not read {}: {err}", path.display()).into()),
        };
        Ok(TriageStore { path, records })
    }

    pub fn get(&self, key: &TriageKey) -> Option<&TriageRecord> {
        self.records.get(key)
    }

    pub fn set(&mut self, key: TriageKey, record: TriageRecord) -> Result<(), TriageError> {
        if self.records.get(&key) == Some(&record) {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_string(&Line {
            key: key.clone(),
            record: record.clone(),
        })?;
        line.push('\n');
        fs::File::options()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())?;
        self.records.insert(key, record);
        Ok(())
    }
}

/// Marks a sidecar as Ingot's own, which is what allows updating it later.
const CREATOR_MARK: &str = "xmp:CreatorTool=\"Ingot\"";

#[derive(Debug, PartialEq, Eq)]
pub enum SidecarOutcome {
    Written(PathBuf),
    /// A sidecar from another program is there. It may hold edits we don't
    /// understand, so it was left alone and nothing was written.
    LeftForeignSidecar(PathBuf),
}

/// The sidecar for a photo: `IMG_1.xmp` beside `IMG_1.CR2`, Lightroom's convention,
/// which a RAW and JPEG of the same stem share.
pub fn sidecar_path(photo: &Path) -> PathBuf {
    photo.with_extension("xmp")
}

/// The XMP rating: stars are 0 to 5, and a rejected photo is -1.
fn xmp_rating(rating: u8, state: TriageState) -> i32 {
    match state {
        TriageState::Rejected => -1,
        _ => i32::from(rating.min(5)),
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sidecar_contents(record: &TriageRecord) -> String {
    let rating = xmp_rating(record.rating, record.state);
    let pick = match record.state {
        TriageState::Accepted => "1",
        TriageState::Rejected => "-1",
        TriageState::Pending => "0",
    };
    let develop = record
        .develop
        .as_ref()
        .map(|settings| {
            format!(
                "\n   ingot:Develop=\"{}\"",
                xml_escape(&settings.to_string())
            )
        })
        .unwrap_or_default();
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
   xmlns:xmp="http://ns.adobe.com/xap/1.0/"
   xmlns:ingot="https://github.com/JustCreature/ingot/ns/1.0/"
   {CREATOR_MARK}
   xmp:Rating="{rating}"
   ingot:Pick="{pick}"{develop}/>
 </rdf:RDF>
</x:xmpmeta>
"#
    )
}

/// Writes a photo's sidecar, swapped in whole so a crash can't leave half of one.
/// Only ever replaces a sidecar Ingot wrote itself.
pub fn write_sidecar(photo: &Path, record: &TriageRecord) -> Result<SidecarOutcome, TriageError> {
    let path = sidecar_path(photo);
    match fs::read_to_string(&path) {
        Ok(existing) if !existing.contains(CREATOR_MARK) => {
            return Ok(SidecarOutcome::LeftForeignSidecar(path));
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Ok(SidecarOutcome::LeftForeignSidecar(path)),
    }

    let mut tmp = path.clone();
    tmp.as_mut_os_string().push(".tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(sidecar_contents(record).as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(SidecarOutcome::Written(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(stem: &str) -> TriageKey {
        TriageKey {
            stem: stem.to_string(),
            taken: None,
        }
    }

    fn record(rating: u8, state: TriageState) -> TriageRecord {
        TriageRecord {
            rating,
            state,
            develop: None,
        }
    }

    #[test]
    fn newest_decision_survives_a_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/triage.jsonl");

        let mut store = TriageStore::open(path.clone()).unwrap();
        store
            .set(key("a"), record(2, TriageState::Pending))
            .unwrap();
        store
            .set(key("a"), record(5, TriageState::Accepted))
            .unwrap();
        store
            .set(key("b"), record(0, TriageState::Rejected))
            .unwrap();

        let store = TriageStore::open(path).unwrap();
        assert_eq!(
            store.get(&key("a")),
            Some(&record(5, TriageState::Accepted))
        );
        assert_eq!(
            store.get(&key("b")),
            Some(&record(0, TriageState::Rejected))
        );
        assert_eq!(store.get(&key("c")), None);
    }

    #[test]
    fn a_damaged_last_line_loses_only_that_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("triage.jsonl");
        let mut store = TriageStore::open(path.clone()).unwrap();
        store
            .set(key("a"), record(3, TriageState::Pending))
            .unwrap();
        fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"key\":{\"st")
            .unwrap();

        let store = TriageStore::open(path).unwrap();
        assert_eq!(store.get(&key("a")), Some(&record(3, TriageState::Pending)));
    }

    #[test]
    fn sidecar_carries_rating_and_rejection_as_xmp() {
        let tmp = tempfile::tempdir().unwrap();
        let photo = tmp.path().join("IMG_1.CR2");

        write_sidecar(&photo, &record(4, TriageState::Accepted)).unwrap();
        let text = fs::read_to_string(tmp.path().join("IMG_1.xmp")).unwrap();
        assert!(text.contains("xmp:Rating=\"4\""), "{text}");

        write_sidecar(&photo, &record(4, TriageState::Rejected)).unwrap();
        let text = fs::read_to_string(tmp.path().join("IMG_1.xmp")).unwrap();
        assert!(text.contains("xmp:Rating=\"-1\""), "{text}");
    }

    #[test]
    fn develop_settings_are_escaped_in_the_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let photo = tmp.path().join("IMG_1.CR2");
        let mut with_develop = record(0, TriageState::Pending);
        with_develop.develop = Some(serde_json::json!({"exposure": 1.5}));

        write_sidecar(&photo, &with_develop).unwrap();

        let text = fs::read_to_string(tmp.path().join("IMG_1.xmp")).unwrap();
        assert!(text.contains("&quot;exposure&quot;"), "{text}");
    }

    #[test]
    fn another_programs_sidecar_is_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let photo = tmp.path().join("IMG_1.CR2");
        let sidecar = tmp.path().join("IMG_1.xmp");
        fs::write(&sidecar, "<lightroom edits/>").unwrap();

        let outcome = write_sidecar(&photo, &record(5, TriageState::Accepted)).unwrap();

        assert_eq!(outcome, SidecarOutcome::LeftForeignSidecar(sidecar.clone()));
        assert_eq!(fs::read_to_string(&sidecar).unwrap(), "<lightroom edits/>");
    }

    #[test]
    fn ingots_own_sidecar_is_updated_and_no_temp_file_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let photo = tmp.path().join("IMG_1.CR2");
        write_sidecar(&photo, &record(1, TriageState::Pending)).unwrap();
        write_sidecar(&photo, &record(2, TriageState::Pending)).unwrap();

        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["IMG_1.xmp"]);
    }
}
