//! What the app knows about the open photos, and every culling action. No drawing
//! happens here, so all of it is testable without a window.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, RwLock, mpsc},
};

use chrono::{DateTime, Local, NaiveDateTime, Timelike};
use eframe::egui::{Key, Modifiers};
use ingot_core::{
    AssetKey, ClusterItem, DEFAULT_SENSITIVITY, FileKind, Fingerprint, ReplicaLocation,
    ScanResponse, SidecarOutcome, TargetId, TriageKey, TriageRecord, TriageState, TriageStore,
    VerifiedReplica, cluster_ranges, write_sidecar,
};
use ingot_develop::DevelopParams;

#[derive(Clone, Debug)]
pub struct Photo {
    pub key: AssetKey,
    pub name: String,
    /// The file's own date, from the directory walk. Known before any file is opened,
    /// so a photo's identity and place in the list never shift as headers arrive.
    pub taken: Option<NaiveDateTime>,
    /// From EXIF, once the header has been read.
    pub captured: Option<NaiveDateTime>,
    pub size: u64,
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
    Ticked,
    NotTicked,
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
    Clusters,
    Tick,
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
        Key::L => C::Clusters,
        Key::T => C::Tick,
        Key::Z => C::Zoom,
        Key::Backslash => C::BeforeAfter,
        Key::A => C::ToggleAutoAdvance,
        Key::Tab => C::TogglePanels,
        Key::Slash => C::Help,
        Key::Escape => C::Back,
        _ => return None,
    })
}

/// How far the import has got, for the always-visible count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// Ticked photos, and their files and bytes: what this import will bring in.
    pub photos: usize,
    pub files: usize,
    pub bytes: u64,
    /// Ticked files on every target.
    pub copied: usize,
    /// Ticked files that failed on some target.
    pub failed: usize,
    /// Unticked photos whose copy stays, because it was made before the untick.
    pub kept: usize,
}

impl ImportCounts {
    /// Ticked files still on their way.
    pub fn pending(&self) -> usize {
        self.files.saturating_sub(self.copied + self.failed)
    }
}

#[derive(Default)]
struct FileCopy {
    targets: HashSet<TargetId>,
    failed: bool,
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
    /// Photos the import will bring in. Everything else is only looked at.
    pub ticked: HashSet<AssetKey>,
    /// Photos the import history says were already brought in.
    pub already_imported: HashSet<AssetKey>,
    /// How many targets a file has to reach to count as copied.
    pub target_count: usize,
    pub sensitivity: f32,
    warnings: Arc<Mutex<Vec<String>>>,
    sidecars: mpsc::Sender<(PathBuf, TriageRecord)>,
    file_copies: HashMap<PathBuf, FileCopy>,
    fingerprints: HashMap<AssetKey, (Fingerprint, bool)>,
    clusters: Vec<Range<usize>>,
    clusters_dirty: bool,
    index_of: HashMap<AssetKey, usize>,
    triage: Option<TriageStore>,
}

/// Sidecars go to the destination disk, which may be slow, so they never block the
/// keypress that changed them.
fn spawn_sidecar_writer(
    warnings: Arc<Mutex<Vec<String>>>,
) -> mpsc::Sender<(PathBuf, TriageRecord)> {
    let (tx, rx) = mpsc::channel::<(PathBuf, TriageRecord)>();
    std::thread::spawn(move || {
        for (copy, record) in rx {
            let warning = match write_sidecar(&copy, &record) {
                Ok(SidecarOutcome::Written(_)) => continue,
                Ok(SidecarOutcome::LeftForeignSidecar(path)) => {
                    format!("Left another program's sidecar alone: {}", path.display())
                }
                Err(err) => format!("Could not write a sidecar for {}: {err}", copy.display()),
            };
            warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(warning);
        }
    });
    tx
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
        let warnings = Arc::new(Mutex::new(vec![]));
        let sidecars = spawn_sidecar_writer(Arc::clone(&warnings));
        let triage = match TriageStore::open(triage_path.to_path_buf()) {
            Ok(triage) => Some(triage),
            Err(err) => {
                warnings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(format!("Ratings won't be saved: {err}"));
                None
            }
        };

        let mut photos = vec![];
        {
            let mut scan = store.write().unwrap_or_else(|e| e.into_inner());
            let scan = &mut *scan;
            let file_meta = &scan.file_meta;
            let meta_of = |path: &Path| file_meta.get(path).copied();
            for (key, asset) in scan.assets.iter_mut() {
                let (_, primary) = asset.files.get_prioritized();
                let name = primary
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| key.stem.clone());
                let file_paths: Vec<&Path> = asset
                    .files
                    .get_all()
                    .iter()
                    .map(|(_, path)| path.as_path())
                    .collect();
                let file_date = file_paths
                    .iter()
                    .filter_map(|path| meta_of(path)?.modified)
                    .min()
                    .map(|at| DateTime::<Local>::from(at).naive_local())
                    .and_then(|at| at.with_nanosecond(0));
                let size = file_paths
                    .iter()
                    .filter_map(|path| meta_of(path))
                    .map(|meta| meta.size)
                    .sum();
                let photo = Photo {
                    key: key.clone(),
                    name,
                    taken: file_date.or(asset.exif_data.captured_at),
                    captured: asset.exif_data.captured_at,
                    size,
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
            // A file the scanner displaced from its asset is still copied with it.
            for collision in &scan.collisions {
                if let Some(photo) = photos.iter_mut().find(|p| p.key == collision.key) {
                    photo.size += meta_of(&collision.displaced).map_or(0, |meta| meta.size);
                    photo.files.push(collision.displaced.clone());
                }
            }
        }
        photos.sort_by(|a, b| (a.taken, &a.name).cmp(&(b.taken, &b.name)));
        let index_of = photos
            .iter()
            .enumerate()
            .map(|(index, photo)| (photo.key.clone(), index))
            .collect();
        let ticked = photos.iter().map(|photo| photo.key.clone()).collect();

        let mut library = Library {
            store,
            photos,
            visible: vec![],
            focus: 0,
            filter: Filter::All,
            auto_advance: true,
            replicas: vec![],
            ticked,
            already_imported: HashSet::new(),
            target_count: 1,
            sensitivity: DEFAULT_SENSITIVITY,
            warnings,
            sidecars,
            file_copies: HashMap::new(),
            fingerprints: HashMap::new(),
            clusters: vec![],
            clusters_dirty: true,
            index_of,
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
            Filter::Ticked => self.ticked.contains(key),
            Filter::NotTicked => !self.ticked.contains(key),
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
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
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
            let _ = self.sidecars.send((copy, record.clone()));
        }
    }

    /// A file reached a target: keeps the proof, and writes its sidecar if the photo
    /// was already culled. Culling doesn't wait for copies; the decisions catch up.
    pub fn add_replica(&mut self, replica: VerifiedReplica) {
        let source = replica.source().to_path_buf();
        let entry = self.file_copies.entry(source.clone()).or_default();
        entry.targets.insert(replica.target());
        entry.failed = false;
        self.replicas.push(replica);

        let Some(photo) = self
            .photos
            .iter()
            .find(|photo| photo.files.contains(&source))
            .cloned()
        else {
            return;
        };
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

    pub fn record_failure(&mut self, source: &Path) {
        self.file_copies
            .entry(source.to_path_buf())
            .or_default()
            .failed = true;
    }

    /// On every target already.
    pub fn is_copied(&self, file: &Path) -> bool {
        self.file_copies
            .get(file)
            .is_some_and(|copy| copy.targets.len() >= self.target_count)
    }

    pub fn clear_failure(&mut self, file: &Path) {
        if let Some(copy) = self.file_copies.get_mut(file) {
            copy.failed = false;
        }
    }

    pub fn photo_copied(&self, photo: &Photo) -> bool {
        !photo.files.is_empty() && photo.files.iter().all(|file| self.is_copied(file))
    }

    pub fn import_counts(&self) -> ImportCounts {
        let mut counts = ImportCounts::default();
        for photo in &self.photos {
            if self.ticked.contains(&photo.key) {
                counts.photos += 1;
                counts.files += photo.files.len();
                counts.bytes += photo.size;
                for file in &photo.files {
                    if self.is_copied(file) {
                        counts.copied += 1;
                    } else if self.file_copies.get(file).is_some_and(|copy| copy.failed) {
                        counts.failed += 1;
                    }
                }
            } else if self.photo_copied(photo) {
                counts.kept += 1;
            }
        }
        counts
    }

    pub fn set_ticked(&mut self, key: &AssetKey, ticked: bool) {
        if ticked {
            self.ticked.insert(key.clone());
        } else {
            self.ticked.remove(key);
        }
        if matches!(self.filter, Filter::Ticked | Filter::NotTicked) {
            self.refresh_visible();
        }
    }

    /// Ticks everything, leaving out what was already imported if asked.
    pub fn tick_all(&mut self, skip_already_imported: bool) {
        self.ticked = self
            .photos
            .iter()
            .filter(|photo| !(skip_already_imported && self.already_imported.contains(&photo.key)))
            .map(|photo| photo.key.clone())
            .collect();
        self.refresh_visible();
    }

    pub fn tick_none(&mut self) {
        self.ticked.clear();
        self.refresh_visible();
    }

    pub fn mark_already_imported(&mut self, files: &HashSet<PathBuf>) {
        self.already_imported = self
            .photos
            .iter()
            .filter(|photo| photo.files.iter().any(|file| files.contains(file)))
            .map(|photo| photo.key.clone())
            .collect();
    }

    /// A photo's header has been read.
    pub fn apply_header(
        &mut self,
        key: &AssetKey,
        orientation: Option<u16>,
        captured: Option<NaiveDateTime>,
    ) {
        if let Some(index) = self.index_of.get(key).copied() {
            let photo = &mut self.photos[index];
            photo.orientation = orientation.unwrap_or(photo.orientation);
            photo.captured = captured;
        }
    }

    /// Stores a photo's fingerprint. One made from the grid thumbnail replaces one
    /// from the tiny header image, and not the other way round.
    pub fn set_fingerprint(&mut self, key: &AssetKey, print: Fingerprint, from_thumb: bool) {
        if !from_thumb && self.fingerprints.contains_key(key) {
            return;
        }
        self.fingerprints.insert(key.clone(), (print, from_thumb));
        self.clusters_dirty = true;
    }

    pub fn has_fingerprint(&self, key: &AssetKey) -> bool {
        self.fingerprints.contains_key(key)
    }

    pub fn fingerprint_count(&self) -> usize {
        self.fingerprints.len()
    }

    /// Only regroups: the fingerprints stay, so this is instant.
    pub fn set_sensitivity(&mut self, sensitivity: f32) {
        self.sensitivity = sensitivity;
        self.clusters_dirty = true;
    }

    /// Groups as ranges of `photos`, which are in capture order.
    pub fn clusters(&mut self) -> &[Range<usize>] {
        if self.clusters_dirty {
            let items: Vec<ClusterItem> = self
                .photos
                .iter()
                .map(|photo| ClusterItem {
                    taken: photo.taken,
                    fingerprint: self.fingerprints.get(&photo.key).map(|(print, _)| *print),
                })
                .collect();
            self.clusters = cluster_ranges(&items, self.sensitivity);
            self.clusters_dirty = false;
        }
        &self.clusters
    }

    pub fn last_warning(&self) -> Option<String> {
        self.warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
    }

    pub fn warn(&self, warning: String) {
        self.warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(warning);
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
    fn counts_follow_the_ticks_instantly() {
        let (_tmp, mut library) = library_of(&THREE);
        let counts = library.import_counts();
        assert_eq!((counts.photos, counts.files), (3, 3));
        assert_eq!(counts.bytes, 3 * b"not a real photo".len() as u64);

        let key = library.photos[0].key.clone();
        library.set_ticked(&key, false);
        assert_eq!(library.import_counts().photos, 2);
        library.tick_none();
        assert_eq!(library.import_counts().files, 0);
        library.tick_all(false);
        assert_eq!(library.import_counts().files, 3);
    }

    #[test]
    fn a_pair_ticks_as_one_photo_and_counts_both_files() {
        let (_tmp, library) = library_of(&["A.CR2", "A.JPG", "B.JPG"]);
        let counts = library.import_counts();
        assert_eq!((counts.photos, counts.files), (2, 3));
    }

    #[test]
    fn already_imported_photos_can_be_left_unticked() {
        let (_tmp, mut library) = library_of(&THREE);
        let first = library.photos[0].files[0].clone();
        library.mark_already_imported(&HashSet::from([first]));
        library.tick_all(true);
        assert_eq!(library.import_counts().photos, 2);
        library.tick_all(false);
        assert_eq!(library.import_counts().photos, 3);
    }

    #[test]
    fn the_ticked_filter_lists_what_will_be_imported() {
        let (_tmp, mut library) = library_of(&THREE);
        let key = library.photos[1].key.clone();
        library.set_ticked(&key, false);
        library.set_filter(Filter::Ticked);
        assert_eq!(library.visible.len(), 2);
        library.set_filter(Filter::NotTicked);
        assert_eq!(focused_name(&library), "B.JPG");
    }

    #[test]
    fn regrouping_keeps_the_fingerprints_and_a_header_print_never_replaces_a_thumb_print() {
        let (_tmp, mut library) = library_of(&THREE);
        let keys: Vec<_> = library.photos.iter().map(|p| p.key.clone()).collect();
        let flat = |value: u8| vec![value; 16 * 16 * 3];
        let dark = Fingerprint::from_rgb(&flat(10), 16, 16).unwrap();
        let bright = Fingerprint::from_rgb(&flat(240), 16, 16).unwrap();

        library.set_fingerprint(&keys[0], dark, true);
        library.set_fingerprint(&keys[1], dark, true);
        library.set_fingerprint(&keys[2], bright, true);
        assert_eq!(library.clusters().len(), 2);
        assert_eq!(library.clusters()[0], 0..2);

        library.set_sensitivity(100.0);
        assert_eq!(library.fingerprint_count(), 3);
        assert_eq!(library.clusters().len(), 2);

        library.set_fingerprint(&keys[2], dark, false);
        assert_eq!(library.clusters().len(), 2);
        library.set_fingerprint(&keys[2], dark, true);
        assert_eq!(library.clusters().len(), 1);
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
