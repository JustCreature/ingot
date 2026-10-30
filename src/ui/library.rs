//! What the app knows about the open photos, and every culling action. No drawing
//! happens here, so all of it is testable without a window.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use chrono::NaiveDateTime;
use eframe::egui::{Key, Modifiers};
use ingot_core::{
    AssetKey, FileKind, ReplicaLocation, ScanResponse, SidecarOutcome, TriageKey, TriageRecord,
    TriageState, TriageStore, VerifiedReplica, write_sidecar,
};
use ingot_develop::DevelopParams;

#[derive(Clone, Debug)]
pub struct Photo {
    pub key: AssetKey,
    pub name: String,
    pub taken: Option<NaiveDateTime>,
    pub orientation: u16,
    pub raw: Option<PathBuf>,
    pub has_jpeg: bool,
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Picked,
    Rejected,
    Unflagged,
    Stars(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Rate(u8),
    Pick,
    Reject,
    Unflag,
    Next,
    Previous,
    RowDown,
    RowUp,
    First,
    Last,
    Grid,
    Loupe,
    Develop,
    Zoom,
    BeforeAfter,
    ToggleAutoAdvance,
    TogglePanels,
    Import,
    CopySettings,
    PasteSettings,
    Help,
    Back,
}

/// The keyboard map, following Lightroom. Holding Shift while rating or flagging
/// also moves on to the next photo.
pub fn command_for(key: Key, modifiers: Modifiers) -> Option<Command> {
    use Command as C;
    if modifiers.command {
        return match (key, modifiers.shift) {
            (Key::I, true) => Some(C::Import),
            (Key::C, true) => Some(C::CopySettings),
            (Key::V, true) => Some(C::PasteSettings),
            _ => None,
        };
    }
    Some(match key {
        Key::Num0 | Key::Backtick => C::Rate(0),
        Key::Num1 => C::Rate(1),
        Key::Num2 => C::Rate(2),
        Key::Num3 => C::Rate(3),
        Key::Num4 => C::Rate(4),
        Key::Num5 => C::Rate(5),
        Key::P => C::Pick,
        Key::X => C::Reject,
        Key::U => C::Unflag,
        Key::ArrowRight => C::Next,
        Key::ArrowLeft => C::Previous,
        Key::ArrowDown => C::RowDown,
        Key::ArrowUp => C::RowUp,
        Key::Home => C::First,
        Key::End => C::Last,
        Key::G => C::Grid,
        Key::E | Key::Enter => C::Loupe,
        Key::D => C::Develop,
        Key::Z => C::Zoom,
        Key::Backslash => C::BeforeAfter,
        Key::A => C::ToggleAutoAdvance,
        Key::Tab => C::TogglePanels,
        Key::Slash => C::Help,
        Key::Escape => C::Back,
        _ => return None,
    })
}

pub struct Library {
    pub store: Arc<RwLock<ScanResponse>>,
    pub photos: Vec<Photo>,
    /// Indexes into `photos` that pass the filter, in display order.
    pub visible: Vec<usize>,
    /// Position in `visible`.
    pub focus: usize,
    pub filter: Filter,
    pub auto_advance: bool,
    pub replicas: Vec<VerifiedReplica>,
    pub warnings: Vec<String>,
    triage: Option<TriageStore>,
}

fn key_of(photo: &Photo) -> TriageKey {
    TriageKey {
        stem: photo.key.stem.clone(),
        taken: photo.taken,
    }
}

impl Library {
    /// Builds the list from a finished scan and restores any saved decisions.
    pub fn new(store: Arc<RwLock<ScanResponse>>, triage_path: &Path) -> Self {
        let mut warnings = vec![];
        let triage = match TriageStore::open(triage_path.to_path_buf()) {
            Ok(triage) => Some(triage),
            Err(err) => {
                warnings.push(format!("Ratings won't be saved: {err}"));
                None
            }
        };

        let mut photos = vec![];
        {
            let mut scan = store.write().unwrap_or_else(|e| e.into_inner());
            for (key, asset) in scan.assets.iter_mut() {
                let (_, primary) = asset.files.get_prioritized();
                let name = primary
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| key.stem.clone());
                let photo = Photo {
                    key: key.clone(),
                    name,
                    taken: asset.exif_data.captured_at,
                    orientation: asset.exif_data.orientation.unwrap_or(1),
                    raw: asset.files.get(FileKind::Raw).map(Path::to_path_buf),
                    has_jpeg: asset.files.get(FileKind::Jpeg).is_some(),
                    files: asset
                        .files
                        .get_all()
                        .iter()
                        .map(|(_, p)| p.clone())
                        .collect(),
                };
                if let Some(saved) = triage.as_ref().and_then(|t| t.get(&key_of(&photo))) {
                    asset.rating = saved.rating;
                    asset.state = saved.state;
                }
                photos.push(photo);
            }
        }
        photos.sort_by(|a, b| (a.taken, &a.name).cmp(&(b.taken, &b.name)));

        let mut library = Library {
            store,
            photos,
            visible: vec![],
            focus: 0,
            filter: Filter::All,
            auto_advance: true,
            replicas: vec![],
            warnings,
            triage,
        };
        library.refresh_visible();
        library
    }

    pub fn focused(&self) -> Option<&Photo> {
        self.photos.get(*self.visible.get(self.focus)?)
    }

    /// (rating, state) of a photo. Takes the store's lock only briefly.
    pub fn decisions(&self, key: &AssetKey) -> (u8, TriageState) {
        let scan = self.store.read().unwrap_or_else(|e| e.into_inner());
        scan.assets
            .get(key)
            .map_or((0, TriageState::Pending), |asset| {
                (asset.rating, asset.state)
            })
    }

    fn passes(&self, key: &AssetKey) -> bool {
        let (rating, state) = self.decisions(key);
        match self.filter {
            Filter::All => true,
            Filter::Picked => state == TriageState::Accepted,
            Filter::Rejected => state == TriageState::Rejected,
            Filter::Unflagged => state == TriageState::Pending,
            Filter::Stars(minimum) => rating >= minimum,
        }
    }

    fn refresh_visible(&mut self) {
        let focused = self.visible.get(self.focus).copied();
        let visible: Vec<usize> = (0..self.photos.len())
            .filter(|i| self.passes(&self.photos[*i].key))
            .collect();
        // Stay on the photo, or on the one that took its place if it left the filter.
        self.focus = match focused {
            Some(index) => visible
                .iter()
                .position(|i| *i >= index)
                .unwrap_or(visible.len().saturating_sub(1)),
            None => 0,
        };
        self.visible = visible;
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.refresh_visible();
    }

    pub fn move_focus(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() as isize - 1;
        self.focus = (self.focus as isize + delta).clamp(0, last) as usize;
    }

    pub fn jump(&mut self, position: usize) {
        self.focus = position.min(self.visible.len().saturating_sub(1));
    }

    /// Applies a rating or flag to the focused photo, saves it, and updates the
    /// sidecars of any copies. Never blocks on the copy: only this photo's entry is
    /// locked, and only for the assignment.
    fn change(&mut self, advance: bool, apply: impl FnOnce(&mut u8, &mut TriageState)) {
        let Some(photo) = self.focused().cloned() else {
            return;
        };
        {
            let mut scan = self.store.write().unwrap_or_else(|e| e.into_inner());
            if let Some(asset) = scan.assets.get_mut(&photo.key) {
                apply(&mut asset.rating, &mut asset.state);
            }
        }
        self.save(&photo);
        let position_key = self.visible.get(self.focus).copied();
        self.refresh_visible();
        // Moving on only makes sense if the photo is still in the list; if the filter
        // removed it, the next one is already in focus.
        if advance && self.visible.get(self.focus).copied() == position_key {
            self.move_focus(1);
        }
    }

    pub fn rate(&mut self, stars: u8, advance: bool) {
        self.change(advance, |rating, _| *rating = stars.min(5));
    }

    pub fn flag(&mut self, flag: TriageState, advance: bool) {
        self.change(advance, |_, state| *state = flag);
    }

    pub fn develop_params(&self, photo: &Photo) -> DevelopParams {
        self.triage
            .as_ref()
            .and_then(|triage| triage.get(&key_of(photo)))
            .and_then(|record| record.develop.clone())
            .and_then(|settings| serde_json::from_value(settings).ok())
            .unwrap_or_default()
    }

    pub fn set_develop_params(&mut self, photo: &Photo, params: &DevelopParams) {
        let (rating, state) = self.decisions(&photo.key);
        self.record(
            photo,
            TriageRecord {
                rating,
                state,
                develop: serde_json::to_value(params).ok(),
            },
        );
    }

    fn save(&mut self, photo: &Photo) {
        let (rating, state) = self.decisions(&photo.key);
        let develop = self
            .triage
            .as_ref()
            .and_then(|triage| triage.get(&key_of(photo)))
            .and_then(|record| record.develop.clone());
        self.record(
            photo,
            TriageRecord {
                rating,
                state,
                develop,
            },
        );
    }

    fn record(&mut self, photo: &Photo, record: TriageRecord) {
        if let Some(triage) = &mut self.triage
            && let Err(err) = triage.set(key_of(photo), record.clone())
        {
            self.warnings
                .push(format!("Could not save a rating: {err}"));
        }
        self.write_sidecars_for(photo, &record);
    }

    /// Writes one sidecar beside each local copy. The source is never written to.
    fn write_sidecars_for(&mut self, photo: &Photo, record: &TriageRecord) {
        let copies: Vec<PathBuf> = self
            .replicas
            .iter()
            .filter(|replica| photo.files.iter().any(|file| file == replica.source()))
            .filter_map(|replica| match replica.location() {
                ReplicaLocation::Local(path) if path != replica.source() => Some(path.clone()),
                _ => None,
            })
            .collect();
        for copy in copies {
            match write_sidecar(&copy, record) {
                Ok(SidecarOutcome::Written(_)) => {}
                Ok(SidecarOutcome::LeftForeignSidecar(path)) => self.warnings.push(format!(
                    "Left another program's sidecar alone: {}",
                    path.display()
                )),
                Err(err) => self.warnings.push(format!(
                    "Could not write a sidecar for {}: {err}",
                    copy.display()
                )),
            }
        }
    }

    /// After an import: keeps the proofs and writes sidecars for photos already culled.
    pub fn add_replicas(&mut self, replicas: Vec<VerifiedReplica>) {
        self.replicas.extend(replicas);
        for photo in self.photos.clone() {
            let (rating, state) = self.decisions(&photo.key);
            let develop = self
                .triage
                .as_ref()
                .and_then(|triage| triage.get(&key_of(&photo)))
                .and_then(|record| record.develop.clone());
            if rating != 0 || state != TriageState::Pending || develop.is_some() {
                self.write_sidecars_for(
                    &photo,
                    &TriageRecord {
                        rating,
                        state,
                        develop,
                    },
                );
            }
        }
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let scan = self.store.read().unwrap_or_else(|e| e.into_inner());
        let count = |state| scan.assets.values().filter(|a| a.state == state).count();
        (
            scan.assets.len(),
            count(TriageState::Accepted),
            count(TriageState::Rejected),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ingot_core::{Engine, EngineConfig};
    use tempfile::TempDir;

    use super::*;

    fn library_of(names: &[&str]) -> (TempDir, Library) {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        fs::create_dir_all(&card).unwrap();
        for name in names {
            fs::write(card.join(name), b"not a real photo").unwrap();
        }
        let mut engine = Engine::new(EngineConfig::default());
        engine.scan(&card);
        let library = Library::new(
            engine.scan_response.clone(),
            &tmp.path().join("triage.jsonl"),
        );
        (tmp, library)
    }

    fn focused_name(library: &Library) -> &str {
        &library.focused().unwrap().name
    }

    const THREE: [&str; 3] = ["A.JPG", "B.JPG", "C.JPG"];

    #[test]
    fn keys_follow_lightroom() {
        let none = Modifiers::NONE;
        assert_eq!(command_for(Key::Num3, none), Some(Command::Rate(3)));
        assert_eq!(command_for(Key::P, none), Some(Command::Pick));
        assert_eq!(command_for(Key::X, none), Some(Command::Reject));
        assert_eq!(command_for(Key::U, none), Some(Command::Unflag));
        assert_eq!(command_for(Key::G, none), Some(Command::Grid));
        assert_eq!(command_for(Key::E, none), Some(Command::Loupe));
        assert_eq!(command_for(Key::D, none), Some(Command::Develop));
        assert_eq!(command_for(Key::Z, none), Some(Command::Zoom));
        assert_eq!(
            command_for(Key::Backslash, none),
            Some(Command::BeforeAfter)
        );
        let command_shift = Modifiers {
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(command_for(Key::I, command_shift), Some(Command::Import));
        assert_eq!(command_for(Key::Q, none), None);
    }

    #[test]
    fn photos_are_listed_in_order_and_focus_stops_at_the_ends() {
        let (_tmp, mut library) = library_of(&THREE);
        assert_eq!(focused_name(&library), "A.JPG");
        library.move_focus(-1);
        assert_eq!(focused_name(&library), "A.JPG");
        library.move_focus(2);
        assert_eq!(focused_name(&library), "C.JPG");
        library.move_focus(5);
        assert_eq!(focused_name(&library), "C.JPG");
    }

    #[test]
    fn rating_and_flagging_apply_to_the_focused_photo() {
        let (_tmp, mut library) = library_of(&THREE);
        library.rate(4, false);
        library.flag(TriageState::Accepted, false);
        let key = library.focused().unwrap().key.clone();
        assert_eq!(library.decisions(&key), (4, TriageState::Accepted));
        assert_eq!(library.counts(), (3, 1, 0));
    }

    #[test]
    fn advancing_moves_on_after_a_flag() {
        let (_tmp, mut library) = library_of(&THREE);
        library.flag(TriageState::Rejected, true);
        assert_eq!(focused_name(&library), "B.JPG");
    }

    #[test]
    fn decisions_survive_reopening_the_folder() {
        let (tmp, mut library) = library_of(&THREE);
        library.rate(5, false);
        let store = library.store.clone();
        drop(library);
        {
            let mut scan = store.write().unwrap();
            for asset in scan.assets.values_mut() {
                asset.rating = 0;
            }
        }

        let library = Library::new(store, &tmp.path().join("triage.jsonl"));
        let key = library.photos[0].key.clone();
        assert_eq!(library.decisions(&key).0, 5);
    }

    #[test]
    fn flagging_inside_a_filter_lands_on_the_next_photo_without_skipping_one() {
        let (_tmp, mut library) = library_of(&THREE);
        library.set_filter(Filter::Unflagged);
        library.flag(TriageState::Rejected, true);
        assert_eq!(library.visible.len(), 2);
        assert_eq!(focused_name(&library), "B.JPG");
    }

    #[test]
    fn filters_select_by_flag_and_stars() {
        let (_tmp, mut library) = library_of(&THREE);
        library.flag(TriageState::Accepted, false);
        library.jump(1);
        library.rate(3, false);
        library.jump(2);
        library.flag(TriageState::Rejected, false);

        library.set_filter(Filter::Picked);
        assert_eq!(library.visible.len(), 1);
        library.set_filter(Filter::Rejected);
        assert_eq!(focused_name(&library), "C.JPG");
        library.set_filter(Filter::Stars(3));
        assert_eq!(focused_name(&library), "B.JPG");
        library.set_filter(Filter::All);
        assert_eq!(library.visible.len(), 3);
    }

    #[test]
    fn empty_filter_results_are_safe_to_act_on() {
        let (_tmp, mut library) = library_of(&THREE);
        library.set_filter(Filter::Picked);
        assert!(library.focused().is_none());
        library.move_focus(1);
        library.rate(5, true);
        library.flag(TriageState::Rejected, true);
        assert_eq!(library.counts(), (3, 0, 0));
    }

    #[test]
    fn develop_settings_are_kept_alongside_the_rating() {
        let (_tmp, mut library) = library_of(&THREE);
        let photo = library.focused().unwrap().clone();
        let params = DevelopParams {
            exposure: 1.5,
            ..Default::default()
        };
        library.set_develop_params(&photo, &params);
        library.rate(2, false);

        assert_eq!(library.develop_params(&photo), params);
        assert_eq!(library.decisions(&photo.key).0, 2);
    }

    #[test]
    fn the_source_folder_never_gets_a_sidecar() {
        let (tmp, mut library) = library_of(&THREE);
        library.rate(5, false);
        library.flag(TriageState::Rejected, false);
        let names: Vec<String> = fs::read_dir(tmp.path().join("card"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.ends_with(".xmp")),
            "{names:?}"
        );
    }
}
