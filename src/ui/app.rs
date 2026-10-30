//! The window: everything drawn, and the background work it starts.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use ingot_core::{
    AssetKey, DEFAULT_SENSITIVITY, DRIVE_ROOT_FOLDER, DateFolders, DriveConfig, DriveCredentials,
    DriveTarget, Engine, EngineConfig, ExtensionCase, FileInfo, Fingerprint, ImportHistory,
    ImportOptions, ImportQueue, LocalTarget, NameTemplate, Organize, PlannedFile, RemovalPlan,
    ReplicaLocation, ReplicationEvent, ReplicationReport, ReplicationTarget, ScanOptions,
    Selection, SweepReport, Target, TargetId, TargetKind, TriageState, UploadFile, VerifiedReplica,
    execute_removal, execute_sweep, plan_import, plan_removal, plan_sweep, planned_relative,
    preview, upload_blocking,
};
use ingot_develop::{DevelopParams, DevelopedImage, PreviewBase, WhiteBalance};

use super::{
    AppPaths,
    images::{ImageCache, Tier, decode_jpeg},
    imports::{ImportEntry, ImportLog},
    library::{Command, Filter, Library, Photo, command_for},
};

// Colors from docs/design/ingot_v2/styles.css.
const BG: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0f);
const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x19);
const PANEL_2: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1f);
const PANEL_3: Color32 = Color32::from_rgb(0x23, 0x23, 0x28);
const STAGE: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0b);
const LINE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x30);
const LINE_2: Color32 = Color32::from_rgb(0x34, 0x34, 0x3b);
const ACCENT_2: Color32 = Color32::from_rgb(0x60, 0xa5, 0xfa);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x1e, 0x3a, 0x6b);
const STAR_OFF: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x40);
const INK: Color32 = Color32::from_rgb(0xec, 0xed, 0xf0);
const INK_2: Color32 = Color32::from_rgb(0xa4, 0xa5, 0xad);
const INK_3: Color32 = Color32::from_rgb(0x6c, 0x6d, 0x77);
const ACCENT: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
const ACCEPT: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
const REJECT: Color32 = Color32::from_rgb(0xf0, 0x55, 0x6b);
const STAR: Color32 = Color32::from_rgb(0xf5, 0xc4, 0x51);
const CLUSTER_COLORS: [Color32; 6] = [
    ACCENT,
    ACCEPT,
    STAR,
    Color32::from_rgb(0xc0, 0x84, 0xfc),
    Color32::from_rgb(0xfb, 0x92, 0x3c),
    Color32::from_rgb(0x22, 0xd3, 0xee),
];

const PREVIEW_EDGE: usize = 1600;
const DRIVE_TARGET_ID: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Grid,
    Clusters,
    Loupe,
    Survey,
    Develop,
}

enum Msg {
    Environment {
        cards: Vec<PathBuf>,
        gdrive: bool,
    },
    /// The folder's names are known. No photo has been opened.
    Listed {
        engine: Arc<Engine>,
        already_imported: HashSet<PathBuf>,
        ignored: usize,
        errors: usize,
    },
    Header(AssetKey, Option<u16>, Option<chrono::NaiveDateTime>),
    Preview(AssetKey),
    PreviewsDone,
    Hashed(AssetKey, Fingerprint, Vec<u32>, bool),
    Full(AssetKey, Result<egui::ColorImage, String>),
    Base(AssetKey, Result<Arc<PreviewBase>, String>),
    Rendered(AssetKey, Result<DevelopedImage, String>),
    Copy(ReplicationEvent),
    CopyDone(ReplicationReport),
    Exported(Result<String, String>),
    SweepPlanned {
        paths: Vec<PathBuf>,
        left_in_cloud: usize,
    },
    SweepDone(Result<SweepReport, String>),
    Finalized(FinalizeResult),
}

/// What a finalized import did, for the report at the end.
#[derive(Clone, Debug, Default)]
struct FinalizeResult {
    kept_photos: usize,
    trashed: Vec<PathBuf>,
    sidecars_trashed: usize,
    already_there: usize,
    uploaded: usize,
    problems: Vec<String>,
}

/// Slider renders: only the newest request matters, so a fast drag never queues up.
#[derive(Default)]
struct RenderSlot {
    job: Mutex<Option<(AssetKey, Arc<PreviewBase>, DevelopParams)>>,
    wake: Condvar,
}

struct ImportForm {
    name: String,
    destination: String,
    subfolder: String,
    organize: usize,
    date_format: String,
    rename: bool,
    template: String,
    custom_text: String,
    start_number: u64,
    extensions: ExtensionCase,
    second_copy: String,
    gdrive: bool,
    only_new: bool,
    error: Option<String>,
}

impl Default for ImportForm {
    fn default() -> Self {
        ImportForm {
            name: default_import_name(),
            destination: String::new(),
            subfolder: String::new(),
            organize: 0,
            date_format: DateFolders::YEAR_DATE.to_string(),
            rename: false,
            template: "{custom}-{seq:4}".to_string(),
            custom_text: String::new(),
            start_number: 1,
            extensions: ExtensionCase::AsIs,
            second_copy: String::new(),
            gdrive: false,
            only_new: true,
            error: None,
        }
    }
}

impl ImportForm {
    fn options(&self) -> Result<ImportOptions, String> {
        let organize = match self.organize {
            0 => Organize::IntoOneFolder,
            1 => Organize::ByOriginalFolders,
            _ => Organize::ByDate(DateFolders::new(&self.date_format).map_err(|e| e.to_string())?),
        };
        let rename = if self.rename {
            Some(NameTemplate::parse(&self.template).map_err(|e| e.to_string())?)
        } else {
            None
        };
        Ok(ImportOptions {
            subfolder: (!self.subfolder.trim().is_empty())
                .then(|| PathBuf::from(self.subfolder.trim())),
            organize,
            rename,
            custom_text: self.custom_text.clone(),
            start_number: self.start_number,
            extension_case: self.extensions,
            // Nothing has been opened yet to date a file by, so names and folders use
            // the file's own date. Duplicates are handled by the app, which still shows
            // them.
            skip_suspected_duplicates: false,
            use_file_dates: true,
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct CopyProgress {
    bytes: u64,
    failed: usize,
    started: Option<Instant>,
    last_errors: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Everything is being copied, and the photographer is choosing.
    Copying,
    /// The choice is made: finishing what was chosen and taking back the rest.
    Finalizing,
    Finalized,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetType {
    Local,
    Cloud,
}

/// A destination, for the ingest bar and the inspector.
struct TargetInfo {
    id: TargetId,
    name: String,
    kind: TargetType,
    slots: usize,
}

/// A running import. It starts the moment a source is chosen.
struct Session {
    id: u64,
    name: String,
    queue: Arc<ImportQueue>,
    options: ImportOptions,
    destination: PathBuf,
    next_number: u64,
    /// Each file keeps the name and number it was first given, even if it leaves the
    /// queue and comes back.
    planned: HashMap<PathBuf, PlannedFile>,
    targets: Vec<TargetInfo>,
    local_targets: usize,
    /// Only gets files at Finalize, because Ingot never deletes from Drive.
    drive: Option<Arc<dyn ReplicationTarget>>,
    phase: Phase,
    keep: HashSet<PathBuf>,
}

/// The confirmation shown before Finalize.
struct FinalizeDialog {
    keep_unflagged: bool,
}

/// How Finalize takes copies back. A field so tests don't fill the real Trash.
type Remover = fn(&RemovalPlan<'_>) -> SweepReport;

struct SweepDialog {
    paths: Vec<PathBuf>,
    left_in_cloud: usize,
    planning: bool,
    running: bool,
}

pub struct IngotApp {
    paths: AppPaths,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    ctx: egui::Context,
    images: ImageCache,
    render: Arc<RenderSlot>,

    source: Option<PathBuf>,
    engine: Option<Arc<Engine>>,
    library: Option<Library>,
    scanning: bool,
    previews_done: bool,
    previews_ready: usize,
    ignored: usize,
    scan_errors: usize,
    status: Option<String>,

    view: View,
    show_panels: bool,
    cell: f32,
    scroll_to_focus: bool,
    columns: usize,
    show_help: bool,
    survey: Vec<usize>,
    toast: Option<(String, Instant)>,
    renaming: Option<(u64, String)>,

    zoomed: bool,
    pan: Vec2,
    full: Option<(AssetKey, TextureHandle)>,
    full_loading: Option<AssetKey>,

    base: Option<(AssetKey, Arc<PreviewBase>)>,
    base_loading: Option<AssetKey>,
    develop_error: Option<(AssetKey, String)>,
    rendered: Option<(AssetKey, TextureHandle)>,
    params: DevelopParams,
    params_for: Option<AssetKey>,
    params_dirty: bool,
    clipboard: Option<DevelopParams>,
    show_before: bool,
    render_requested: bool,

    cards: Vec<PathBuf>,
    suggested_source: Option<PathBuf>,
    gdrive_available: bool,
    import_open: bool,
    imports: ImportLog,
    session: Option<Session>,
    finalize: Option<FinalizeDialog>,
    finalize_result: Option<FinalizeResult>,
    save_open: bool,
    remover: Remover,
    drive_override: Option<Arc<dyn ReplicationTarget>>,
    hashes: mpsc::Sender<(AssetKey, Vec<u8>, bool)>,
    form: ImportForm,
    copy: CopyProgress,
    sweep: Option<SweepDialog>,
}

fn io_err(err: impl std::fmt::Display) -> String {
    err.to_string()
}

impl IngotApp {
    pub fn new(ctx: &egui::Context, paths: AppPaths, folder: Option<PathBuf>) -> Self {
        apply_style(ctx);
        let (tx, rx) = mpsc::channel();
        let render = Arc::new(RenderSlot::default());
        spawn_render_thread(Arc::clone(&render), tx.clone(), ctx.clone());
        spawn_environment_thread(paths.gdrive.clone(), tx.clone(), ctx.clone());
        let hashes = spawn_hash_thread(tx.clone(), ctx.clone());
        let imports = ImportLog::open(paths.imports.clone());
        let mut app = IngotApp {
            paths,
            tx,
            rx,
            ctx: ctx.clone(),
            images: ImageCache::new(ctx),
            render,
            source: None,
            engine: None,
            library: None,
            scanning: false,
            previews_done: false,
            previews_ready: 0,
            ignored: 0,
            scan_errors: 0,
            status: None,
            view: View::Grid,
            show_panels: true,
            cell: 190.0,
            scroll_to_focus: false,
            columns: 1,
            show_help: false,
            survey: vec![],
            toast: None,
            renaming: None,
            zoomed: false,
            pan: Vec2::ZERO,
            full: None,
            full_loading: None,
            base: None,
            base_loading: None,
            develop_error: None,
            rendered: None,
            params: DevelopParams::default(),
            params_for: None,
            params_dirty: false,
            clipboard: None,
            show_before: false,
            render_requested: false,
            cards: vec![],
            suggested_source: None,
            gdrive_available: false,
            import_open: false,
            imports,
            session: None,
            finalize: None,
            finalize_result: None,
            save_open: false,
            remover: execute_removal,
            drive_override: None,
            hashes,
            form: ImportForm::default(),
            copy: CopyProgress::default(),
            sweep: None,
        };
        // A folder from the command line is offered as a source. It can't start yet,
        // because the destination comes first.
        app.suggested_source = folder;
        app
    }

    /// The source was chosen, which is what starts the import. Needs a destination.
    fn choose_source(&mut self, folder: PathBuf) {
        self.form.error = None;
        if self.form.destination.trim().is_empty() {
            self.form.error = Some("Choose a destination first.".to_string());
            return;
        }
        if let Err(err) = self.form.options() {
            self.form.error = Some(err);
            return;
        }
        if self.form.gdrive
            && self.drive_override.is_none()
            && let Err(err) = DriveCredentials::load(&self.paths.gdrive)
        {
            self.form.error = Some(err.to_string());
            return;
        }
        if !self.leave_session() {
            return;
        }
        self.open_folder(folder);
    }

    /// Lists a folder in the background. Once it is listed, the import starts.
    fn open_folder(&mut self, folder: PathBuf) {
        self.flush_params();
        self.source = Some(folder.clone());
        self.engine = None;
        self.library = None;
        self.scanning = true;
        self.previews_done = false;
        self.previews_ready = 0;
        self.view = View::Grid;
        self.status = None;
        self.survey.clear();
        self.finalize_result = None;
        self.copy = CopyProgress::default();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let history_path = self.paths.history.clone();
        std::thread::spawn(move || {
            let mut engine = Engine::new(EngineConfig::default());
            engine.scan_names(&folder, &ScanOptions::default());
            let (ignored, errors) = {
                let scan = engine
                    .scan_response
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                (scan.ignored.len(), scan.errors.len())
            };
            let already_imported = ImportHistory::open(history_path)
                .ok()
                .and_then(|history| {
                    let options = ImportOptions {
                        use_file_dates: true,
                        ..Default::default()
                    };
                    plan_import(&engine.scan_response, &folder, &options, Some(&history)).ok()
                })
                .map(|plan| plan.skipped_duplicates.into_iter().collect())
                .unwrap_or_default();
            let _ = tx.send(Msg::Listed {
                engine: Arc::new(engine),
                already_imported,
                ignored,
                errors,
            });
            ctx.request_repaint();
        });
    }

    /// Whether the current import can be left. Not while files are moving, so a copy
    /// can't be walked away from by accident.
    fn leave_session(&mut self) -> bool {
        let pending = self
            .library
            .as_ref()
            .map_or(0, |library| library.import_counts().pending());
        let finalizing = self
            .session
            .as_ref()
            .is_some_and(|session| session.phase == Phase::Finalizing);
        // Once finalized, files that were not kept no longer count as on their way.
        let copying = self
            .session
            .as_ref()
            .is_some_and(|session| session.phase == Phase::Copying);
        if finalizing || (copying && pending > 0) {
            self.status = Some("Finalize this import or wait for it to finish first.".into());
            self.form.error = self.status.clone();
            return false;
        }
        if let Some(session) = self.session.take() {
            session.queue.close();
        }
        true
    }

    fn open_launchpad(&mut self) {
        self.form.name = default_import_name();
        self.form.error = None;
        self.import_open = true;
    }

    /// Starts the import, the moment the folder is listed. The copy, the headers and
    /// the previews all start together, and the grid opens.
    fn start_session(&mut self) -> Result<(), String> {
        let destination = self.form.destination.trim().to_string();
        let engine = self.engine.clone().ok_or("The folder is still loading.")?;
        let options = self.form.options()?;
        let local = |id, root: &str| -> Arc<dyn ReplicationTarget> {
            Arc::new(LocalTarget::new(
                TargetId(id),
                Target {
                    root: PathBuf::from(root.trim()),
                    kind: TargetKind::LocalNvme,
                    write_permits: 4,
                },
            ))
        };
        let short_name = |root: &str| {
            Path::new(root.trim())
                .file_name()
                .map_or_else(|| root.to_string(), |n| n.to_string_lossy().into_owned())
        };
        let primary = local(1, &destination);
        let mut targets = vec![TargetInfo {
            id: TargetId(1),
            name: short_name(&destination),
            kind: TargetType::Local,
            slots: 4,
        }];
        let mut secondaries = vec![];
        if !self.form.second_copy.trim().is_empty() {
            secondaries.push(local(2, &self.form.second_copy));
            targets.push(TargetInfo {
                id: TargetId(2),
                name: short_name(&self.form.second_copy),
                kind: TargetType::Local,
                slots: 4,
            });
        }
        let local_targets = targets.len();
        let drive: Option<Arc<dyn ReplicationTarget>> = if !self.form.gdrive {
            None
        } else if let Some(fake) = &self.drive_override {
            Some(Arc::clone(fake))
        } else {
            let credentials = DriveCredentials::load(&self.paths.gdrive).map_err(io_err)?;
            Some(Arc::new(
                DriveTarget::new(
                    TargetId(DRIVE_TARGET_ID),
                    DriveConfig::default(),
                    credentials,
                )
                .map_err(io_err)?,
            ))
        };
        if let Some(drive) = &drive {
            targets.push(TargetInfo {
                id: drive.id(),
                name: "Google Drive".to_string(),
                kind: TargetType::Cloud,
                slots: drive.max_concurrent_writes(),
            });
        }

        // The history is written at Finalize: it records what was kept.
        let handle = engine.start_session(primary, secondaries, None, vec![]);
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |at| at.as_millis() as u64);
        let name = match self.form.name.trim() {
            "" => default_import_name(),
            name => name.to_string(),
        };
        let photos = self.library.as_ref().map_or(0, |l| l.photos.len());
        if let Err(err) = self.imports.add(ImportEntry {
            id,
            name: name.clone(),
            destination: PathBuf::from(&destination),
            photos,
            finalized: false,
        }) {
            self.status = Some(format!("Could not save the import list: {err}"));
        }
        self.session = Some(Session {
            id,
            name,
            queue: Arc::clone(&handle.queue),
            next_number: options.start_number,
            options,
            destination: PathBuf::from(destination),
            planned: HashMap::new(),
            targets,
            local_targets,
            drive,
            phase: Phase::Copying,
            keep: HashSet::new(),
        });
        self.copy = CopyProgress {
            started: Some(Instant::now()),
            ..Default::default()
        };
        if let Some(library) = &mut self.library {
            library.target_count = local_targets;
        }
        let everything: Vec<usize> = self.library.as_ref().map_or(vec![], |library| {
            (0..library.photos.len())
                .filter(|index| {
                    !(self.form.only_new
                        && library
                            .already_imported
                            .contains(&library.photos[*index].key))
                })
                .collect()
        });
        self.queue_photos(&everything);

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            for event in handle.progress.iter() {
                let _ = tx.send(Msg::Copy(event));
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::CopyDone(handle.wait()));
            ctx.request_repaint();
        });

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            engine.enrich_streaming(|key, data| {
                let _ = tx.send(Msg::Header(key.clone(), data.orientation, data.captured_at));
                ctx.request_repaint();
            });
            for event in engine.ingest().iter() {
                let ingot_core::IngestEvent::Preview(message) = event;
                let _ = tx.send(Msg::Preview(message.key));
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::PreviewsDone);
            ctx.request_repaint();
        });
        Ok(())
    }

    /// Puts photos' files on the copy queue. Uses only what the folder listing already
    /// knows, so it's instant however slow the card is.
    fn queue_photos(&mut self, indexes: &[usize]) {
        let (Some(session), Some(library), Some(source)) =
            (&mut self.session, &mut self.library, &self.source)
        else {
            return;
        };
        let mut wanted = HashSet::new();
        for index in indexes {
            for file in &library.photos[*index].files {
                if !library.is_copied(file) {
                    wanted.insert(file.clone());
                }
            }
        }
        let unplanned: HashSet<PathBuf> = wanted
            .iter()
            .filter(|file| !session.planned.contains_key(*file))
            .cloned()
            .collect();
        if !unplanned.is_empty() {
            let options = ImportOptions {
                start_number: session.next_number,
                selection: Selection {
                    paths: Some(unplanned),
                    ..Default::default()
                },
                ..session.options.clone()
            };
            match plan_import(&library.store, source, &options, None) {
                Ok(plan) => {
                    session.next_number = plan.next_number;
                    for file in plan.files {
                        session.planned.insert(file.source.clone(), file);
                    }
                    for failure in plan.failures {
                        self.copy.failed += 1;
                        self.copy.last_errors.push(failure.error.to_string());
                        if let Some(source) = &failure.source {
                            library.record_failure(source);
                        }
                    }
                }
                Err(err) => self.status = Some(err.to_string()),
            }
        }
        for index in indexes {
            for file in library.photos[*index].files.clone() {
                if wanted.contains(&file)
                    && let Some(planned) = session.planned.get(&file)
                {
                    library.clear_failure(&file);
                    session.queue.push(planned.clone());
                }
            }
        }
    }

    /// Flags the current photos and keeps the queue in step, as the design's "held": a
    /// rejected photo not read yet waits, and comes back if the flag is cleared.
    fn apply_flag(&mut self, flag: TriageState, advance: bool) {
        let Some(library) = &mut self.library else {
            return;
        };
        let targets = library.action_targets();
        library.flag(flag, advance);
        let mut requeue = vec![];
        for index in targets {
            let photo = &library.photos[index];
            if library.decisions(&photo.key).1 == TriageState::Rejected {
                if let Some(session) = &self.session {
                    for file in &photo.files {
                        session.queue.remove(file);
                    }
                }
            } else if !library.already_imported.contains(&photo.key) {
                requeue.push(index);
            }
        }
        let copying = self
            .session
            .as_ref()
            .is_some_and(|session| session.phase == Phase::Copying);
        if copying {
            self.queue_photos(&requeue);
        }
    }

    /// The choice is confirmed. Stops queued copies of everything else and lets the
    /// copy finish. The rest happens when it reports in (`finish_finalize`).
    fn confirm_finalize(&mut self, keep_unflagged: bool) {
        let Some(library) = &mut self.library else {
            return;
        };
        let keep = library.files_to_keep(keep_unflagged);
        library.sidecar_keep = Some(keep.clone());
        let mut missing = vec![];
        for (index, photo) in library.photos.iter().enumerate() {
            let mut chosen_and_missing = false;
            for file in &photo.files {
                if keep.contains(file) {
                    chosen_and_missing |= !library.is_copied(file);
                } else if let Some(session) = &self.session {
                    session.queue.remove(file);
                }
            }
            if chosen_and_missing {
                missing.push(index);
            }
        }
        // Chosen files not copied yet, such as ones flagged rejected earlier.
        self.queue_photos(&missing);
        if let Some(session) = &mut self.session {
            session.keep = keep;
            session.phase = Phase::Finalizing;
            session.queue.close();
        }
    }

    /// The copy has stopped. In the background: takes back what wasn't chosen, sends
    /// what was to Google Drive, and records the result.
    fn finish_finalize(&mut self, report: ReplicationReport) {
        let (Some(session), Some(library)) = (&self.session, &self.library) else {
            return;
        };
        let flushed = library.sidecars_flushed();
        let keep = session.keep.clone();
        let destination = session.destination.clone();
        let planned = session.planned.clone();
        let local_targets = session.local_targets;
        let drive = session.drive.clone();
        let history_path = self.paths.history.clone();
        let remover = self.remover;
        let kept_photos = library
            .photos
            .iter()
            .filter(|photo| photo.files.iter().any(|file| keep.contains(file)))
            .count();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (progress_tx, progress) = crossbeam_channel::unbounded();
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                for event in progress.iter() {
                    let _ = tx.send(Msg::Copy(event));
                    ctx.request_repaint();
                }
            });
        }
        std::thread::spawn(move || {
            // A sidecar still being written can't reappear after its copy has gone.
            let _ = flushed.recv();
            let mut result = FinalizeResult {
                kept_photos,
                ..Default::default()
            };
            for failure in &report.failures {
                let is_chosen = failure.source.as_ref().is_none_or(|s| keep.contains(s));
                if is_chosen {
                    let name = failure
                        .source
                        .as_deref()
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    result.problems.push(format!("{name}: {}", failure.error));
                }
            }

            let plan = plan_removal(&report.verified, &keep);
            result.already_there = plan.already_there.len();
            let removal = remover(&plan);
            result.sidecars_trashed = removal.sidecars_trashed.len();
            for (path, why) in &removal.failures {
                result.problems.push(format!("{}: {why}", path.display()));
            }
            result.trashed = removal.trashed;

            let mut uploaded: HashSet<PathBuf> = HashSet::new();
            let uses_drive = drive.is_some();
            if let Some(drive) = drive {
                let mut files = vec![];
                for replica in &report.verified {
                    let ReplicaLocation::Local(local) = replica.location() else {
                        continue;
                    };
                    if replica.target() != TargetId(1) || !keep.contains(replica.source()) {
                        continue;
                    }
                    let Ok(relative) = local.strip_prefix(&destination) else {
                        result.problems.push(format!(
                            "{} is outside the destination, so it wasn't uploaded",
                            local.display()
                        ));
                        continue;
                    };
                    files.push(UploadFile {
                        info: FileInfo {
                            source: replica.source().to_path_buf(),
                            relative: relative.to_path_buf(),
                            modified: planned.get(replica.source()).and_then(|f| f.modified),
                            src_hash: replica.hash(),
                        },
                        local: local.clone(),
                    });
                }
                let upload = upload_blocking(drive, files, Some(progress_tx));
                for replica in &upload.verified {
                    uploaded.insert(replica.source().to_path_buf());
                }
                result.uploaded = upload.verified.len();
                for failure in &upload.failures {
                    result.problems.push(failure.error.to_string());
                }
            } else {
                drop(progress_tx);
            }

            // Only what was kept, and only once it is everywhere it should be.
            let mut reached: HashMap<&Path, usize> = HashMap::new();
            for replica in &report.verified {
                *reached.entry(replica.source()).or_default() += 1;
            }
            let done: Vec<_> = reached
                .iter()
                .filter(|(source, count)| {
                    **count >= local_targets
                        && keep.contains(**source)
                        && (!uses_drive || uploaded.contains(**source))
                })
                .filter_map(|(source, _)| planned.get(*source))
                .map(|file| file.duplicate_key.clone())
                .collect();
            match ImportHistory::open(history_path) {
                Ok(mut history) => {
                    if let Err(err) = history.record(done) {
                        result.problems.push(format!("Import history: {err}"));
                    }
                }
                Err(err) => result.problems.push(format!("Import history: {err}")),
            }
            let _ = tx.send(Msg::Finalized(result));
            ctx.request_repaint();
        });
    }

    /// Queues a photo's fingerprint from an image already in memory.
    fn request_hash(&self, key: &AssetKey, from_thumb: bool) {
        let Some(library) = &self.library else {
            return;
        };
        if !from_thumb && library.has_fingerprint(key) {
            return;
        }
        let bytes = {
            let scan = library.store.read().unwrap_or_else(|e| e.into_inner());
            scan.assets.get(key).and_then(|asset| {
                if from_thumb {
                    asset.image_data.thumb.clone()
                } else {
                    asset.exif_data.thumbnail.clone()
                }
            })
        };
        if let Some(bytes) = bytes {
            let _ = self.hashes.send((key.clone(), bytes, from_thumb));
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::Environment { cards, gdrive } => {
                    self.cards = cards;
                    self.gdrive_available = gdrive;
                }
                Msg::Listed {
                    engine,
                    already_imported,
                    ignored,
                    errors,
                } => {
                    self.ignored = ignored;
                    self.scan_errors = errors;
                    let mut library =
                        Library::new(Arc::clone(&engine.scan_response), &self.paths.triage);
                    library.mark_already_imported(&already_imported);
                    self.library = Some(library);
                    self.engine = Some(engine);
                    self.scanning = false;
                    match self.start_session() {
                        Ok(()) => {
                            self.import_open = false;
                            self.form.error = None;
                        }
                        Err(err) => {
                            self.library = None;
                            self.engine = None;
                            self.form.error = Some(err);
                            self.import_open = true;
                        }
                    }
                }
                Msg::Header(key, orientation, captured) => {
                    if let Some(library) = &mut self.library {
                        library.apply_header(&key, orientation, captured);
                    }
                    self.request_hash(&key, false);
                }
                Msg::Preview(key) => {
                    self.previews_ready += 1;
                    self.request_hash(&key, true);
                }
                Msg::PreviewsDone => self.previews_done = true,
                Msg::Hashed(key, print, histogram, from_thumb) => {
                    if let Some(library) = &mut self.library {
                        library.set_histogram(&key, histogram);
                        library.set_fingerprint(&key, print, from_thumb);
                    }
                }
                Msg::Full(key, result) => {
                    self.full_loading = None;
                    match result {
                        Ok(image) => {
                            let max_side = self.ctx.input(|input| input.max_texture_side);
                            let image = fit_texture(image, max_side);
                            let texture =
                                self.ctx.load_texture("full", image, TextureOptions::LINEAR);
                            self.full = Some((key, texture));
                        }
                        Err(err) => self.status = Some(format!("Could not load 1:1: {err}")),
                    }
                }
                Msg::Base(key, result) => {
                    self.base_loading = None;
                    match result {
                        Ok(base) => {
                            self.base = Some((key, base));
                            self.render_requested = true;
                        }
                        Err(err) => self.develop_error = Some((key, err)),
                    }
                }
                Msg::Rendered(key, result) => match result {
                    Ok(image) => {
                        let color =
                            egui::ColorImage::from_rgb([image.width, image.height], &image.rgb);
                        let texture =
                            self.ctx
                                .load_texture("develop", color, TextureOptions::LINEAR);
                        self.rendered = Some((key, texture));
                    }
                    Err(err) => self.develop_error = Some((key, err)),
                },
                Msg::Copy(event) => match event {
                    ReplicationEvent::Verified {
                        size,
                        newly_written,
                        replica,
                        ..
                    } => {
                        if newly_written {
                            self.copy.bytes += size;
                        }
                        if let Some(library) = &mut self.library {
                            library.add_replica(replica);
                        }
                    }
                    ReplicationEvent::Failed { source, error, .. } => {
                        self.copy.failed += 1;
                        if let (Some(library), Some(source)) = (&mut self.library, &source) {
                            library.record_failure(source);
                        }
                        if self.copy.last_errors.len() < 20 {
                            let name = source
                                .as_deref()
                                .and_then(Path::file_name)
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            self.copy.last_errors.push(format!("{name}: {error}"));
                        }
                    }
                },
                Msg::CopyDone(report) => {
                    self.finish_finalize(report);
                }
                Msg::Finalized(result) => {
                    if let Some(library) = &mut self.library {
                        library.forget_trashed(&result.trashed);
                    }
                    if let Some(session) = &mut self.session {
                        session.phase = Phase::Finalized;
                        let (id, kept) = (session.id, result.kept_photos);
                        if let Err(err) = self.imports.update(id, |entry| {
                            entry.photos = kept;
                            entry.finalized = true;
                        }) {
                            self.status = Some(format!("Could not save the import list: {err}"));
                        }
                    }
                    self.finalize_result = Some(result);
                }
                Msg::Exported(result) => {
                    self.status = Some(match result {
                        Ok(message) => message,
                        Err(err) => format!("Export failed: {err}"),
                    });
                }
                Msg::SweepPlanned {
                    paths,
                    left_in_cloud,
                } => {
                    self.sweep = Some(SweepDialog {
                        paths,
                        left_in_cloud,
                        planning: false,
                        running: false,
                    });
                }
                Msg::SweepDone(result) => {
                    self.sweep = None;
                    match result {
                        Ok(report) => {
                            self.status = Some(format!(
                                "{} copies moved to the Trash, {} kept",
                                report.trashed.len(),
                                report.failures.len()
                            ));
                            if let Some(library) = &mut self.library {
                                for (path, why) in &report.failures {
                                    library.warn(format!("{}: {why}", path.display()));
                                }
                                library.forget_trashed(&report.trashed);
                            }
                        }
                        Err(err) => self.status = Some(err),
                    }
                }
            }
        }
    }

    fn modal_open(&self) -> bool {
        self.sweep.is_some()
            || self.finalize.is_some()
            || self.finalize_result.is_some()
            || self.save_open
    }

    fn is_grid_view(&self) -> bool {
        matches!(self.view, View::Grid | View::Clusters)
    }

    fn run_command(&mut self, command: Command, shift: bool) {
        let grid = self.is_grid_view();
        let Some(library) = &mut self.library else {
            if command == Command::Help {
                self.show_help = !self.show_help;
            }
            return;
        };
        let advance = shift || library.auto_advance;
        let mut focus_moved = true;
        match command {
            Command::Rate(stars) => library.rate(stars, shift),
            Command::Pick => self.apply_flag(TriageState::Accepted, advance),
            Command::Reject => self.apply_flag(TriageState::Rejected, advance),
            Command::Unflag => self.apply_flag(TriageState::Pending, shift),
            Command::Next => library.move_focus(1),
            Command::Previous => library.move_focus(-1),
            Command::RowDown => library.move_focus(if grid { self.columns as isize } else { 1 }),
            Command::RowUp => library.move_focus(if grid { -(self.columns as isize) } else { -1 }),
            Command::First => library.jump(0),
            Command::Last => library.jump(usize::MAX),
            Command::Grid => self.view = View::Grid,
            Command::Loupe => self.view = View::Loupe,
            Command::Develop => self.view = View::Develop,
            Command::Clusters => self.view = View::Clusters,
            Command::Survey => {
                self.survey = library.survey_positions();
                self.view = View::Survey;
            }
            Command::Zoom => {
                if self.is_grid_view() {
                    self.view = View::Loupe;
                }
                self.zoomed = !self.zoomed;
                self.pan = Vec2::ZERO;
            }
            Command::BeforeAfter => self.show_before = !self.show_before,
            Command::ToggleAutoAdvance => library.auto_advance = !library.auto_advance,
            Command::TogglePanels => self.show_panels = !self.show_panels,
            Command::Import => self.open_launchpad(),
            Command::SelectAll => library.select_all(),
            Command::CopySettings => self.clipboard = Some(self.params),
            Command::PasteSettings => {
                if let Some(pasted) = self.clipboard {
                    self.params = pasted;
                    self.commit_params();
                }
            }
            Command::Help => self.show_help = !self.show_help,
            Command::Back => {
                if self.zoomed {
                    self.zoomed = false;
                } else if grid {
                    library.clear_selection();
                } else {
                    self.view = View::Grid;
                }
                focus_moved = false;
            }
        }
        if focus_moved {
            self.scroll_to_focus = true;
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Before an import starts there is nothing open to act on.
        if ctx.egui_wants_keyboard_input()
            || self.session.is_none()
            || self.import_open
            || self.modal_open()
        {
            return;
        }
        let pressed: Vec<(Key, egui::Modifiers)> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, modifiers) in pressed {
            if let Some(command) = command_for(key, modifiers) {
                self.run_command(command, modifiers.shift);
            }
        }
    }

    /// Loads the focused photo's develop state when focus changes.
    fn sync_develop(&mut self) {
        let Some(photo) = self.library.as_ref().and_then(|l| l.focused().cloned()) else {
            return;
        };
        if self.params_for.as_ref() != Some(&photo.key) {
            self.flush_params();
            if let Some(library) = &self.library {
                self.params = library.develop_params(&photo);
            }
            self.params_for = Some(photo.key.clone());
            self.render_requested = true;
            self.show_before = false;
        }
        if self.view != View::Develop {
            return;
        }
        let have_base = self.base.as_ref().is_some_and(|(key, _)| *key == photo.key);
        let failed = self
            .develop_error
            .as_ref()
            .is_some_and(|(key, _)| *key == photo.key);
        if !have_base && !failed && self.base_loading.as_ref() != Some(&photo.key) {
            self.start_base(&photo);
        }
        if self.render_requested && have_base {
            self.render_requested = false;
            if let Some((_, base)) = &self.base {
                let params = if self.show_before {
                    DevelopParams::default()
                } else {
                    self.params
                };
                *self.render.job.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((photo.key.clone(), Arc::clone(base), params));
                self.render.wake.notify_one();
            }
        }
    }

    fn start_base(&mut self, photo: &Photo) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let Some(raw) = photo.raw.clone() else {
            self.develop_error = Some((
                photo.key.clone(),
                "Only RAW files can be developed. This photo is a JPEG.".to_string(),
            ));
            return;
        };
        self.base_loading = Some(photo.key.clone());
        let (key, tx, ctx) = (photo.key.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = (|| {
                let sensor = {
                    let _permit = engine.card_permit();
                    ingot_develop::load_sensor(&raw).map_err(io_err)?
                };
                PreviewBase::new(&sensor, PREVIEW_EDGE)
                    .map(Arc::new)
                    .map_err(io_err)
            })();
            let _ = tx.send(Msg::Base(key, result));
            ctx.request_repaint();
        });
    }

    fn commit_params(&mut self) {
        self.render_requested = true;
        self.params_dirty = true;
    }

    /// Saves slider changes. Called once a drag ends rather than on every tick,
    /// because each save also rewrites the photo's sidecars.
    fn flush_params(&mut self) {
        if !self.params_dirty {
            return;
        }
        self.params_dirty = false;
        let Some(key) = self.params_for.clone() else {
            return;
        };
        if let Some(library) = &mut self.library
            && let Some(photo) = library.photos.iter().find(|p| p.key == key).cloned()
        {
            library.set_develop_params(&photo, &self.params);
        }
    }

    fn request_full(&mut self) {
        let Some(photo) = self.library.as_ref().and_then(|l| l.focused().cloned()) else {
            return;
        };
        let have = self.full.as_ref().is_some_and(|(key, _)| *key == photo.key);
        if have || self.full_loading.as_ref() == Some(&photo.key) {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        self.full_loading = Some(photo.key.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = engine
                .read_full_jpeg(&photo.key)
                .map_err(io_err)
                .and_then(|bytes| decode_jpeg(&bytes, photo.orientation));
            let _ = tx.send(Msg::Full(photo.key, result));
            ctx.request_repaint();
        });
    }

    fn export(&mut self) {
        let Some(photo) = self.library.as_ref().and_then(|l| l.focused().cloned()) else {
            return;
        };
        let Some(raw) = photo.raw.clone() else {
            return;
        };
        let stem = raw
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(output) = rfd::FileDialog::new()
            .set_file_name(format!("{stem}.jpg"))
            .save_file()
        else {
            return;
        };
        let (params, tx, ctx) = (self.params, self.tx.clone(), self.ctx.clone());
        let engine = self.engine.clone();
        self.status = Some(format!("Exporting {stem}…"));
        std::thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                let sensor = {
                    let _permit = engine.as_ref().map(|engine| engine.card_permit());
                    ingot_develop::load_sensor(&raw).map_err(io_err)?
                };
                let image = ingot_develop::develop(sensor, &params).map_err(io_err)?;
                let jpeg = ingot_core::preview::compress(turbojpeg::Image {
                    pixels: image.rgb,
                    width: image.width,
                    pitch: image.width * 3,
                    height: image.height,
                    format: turbojpeg::PixelFormat::RGB,
                })
                .map_err(io_err)?;
                // A new file only: an existing one stays as it is.
                let mut file = std::fs::File::create_new(&output)
                    .map_err(|err| format!("{}: {err}", output.display()))?;
                std::io::Write::write_all(&mut file, &jpeg).map_err(io_err)?;
                Ok(format!("Exported {}", output.display()))
            })();
            let _ = tx.send(Msg::Exported(result));
            ctx.request_repaint();
        });
    }

    /// Plans in the background: it reads file state from disk and can be slow.
    fn open_sweep(&mut self) {
        let Some(library) = &self.library else {
            return;
        };
        self.sweep = Some(SweepDialog {
            paths: vec![],
            left_in_cloud: 0,
            planning: true,
            running: false,
        });
        let (store, replicas): (_, Vec<VerifiedReplica>) =
            (Arc::clone(&library.store), library.replicas.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let message = match plan_sweep(&store, &replicas) {
                Ok(plan) => Msg::SweepPlanned {
                    paths: plan.paths().iter().map(|p| p.to_path_buf()).collect(),
                    left_in_cloud: plan.left_in_cloud,
                },
                Err(err) => Msg::SweepDone(Err(err.to_string())),
            };
            let _ = tx.send(message);
            ctx.request_repaint();
        });
    }

    /// Hashes each file again before trashing it, so this never runs on the UI thread.
    fn confirm_sweep(&mut self) {
        let Some(library) = &self.library else {
            return;
        };
        if let Some(dialog) = &mut self.sweep {
            dialog.running = true;
        }
        let (store, replicas): (_, Vec<VerifiedReplica>) =
            (Arc::clone(&library.store), library.replicas.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = plan_sweep(&store, &replicas)
                .map(|plan| execute_sweep(&plan))
                .map_err(io_err);
            let _ = tx.send(Msg::SweepDone(result));
            ctx.request_repaint();
        });
    }

    /// Draws one frame. Separate from `eframe::App` so tests can run it headless.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.ctx = ctx.clone();
        self.drain_messages();
        self.images.collect(&ctx);
        self.handle_keys(&ctx);
        if self.session.is_some() {
            self.sync_develop();
        }
        if self.params_dirty && !ctx.input(|input| input.pointer.any_down()) {
            self.flush_params();
        }
        if self.zoomed && self.view == View::Loupe {
            self.request_full();
        }

        self.top_bar(ui);
        self.ingest_bar(ui);
        if self.session.is_none() {
            self.launchpad_screen(ui);
        } else {
            if self.show_panels {
                self.left_panel(ui);
                self.right_panel(ui);
            }
            self.filmstrip(ui);
            self.stage(ui);
        }
        if self.import_open && self.session.is_some() {
            self.launchpad_overlay(&ctx);
        }
        self.finalize_window(&ctx);
        self.finalize_result_window(&ctx);
        self.save_window(&ctx);
        self.sweep_window(&ctx);
        self.help_window(&ctx);
        self.draw_toast(&ctx);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let counts = self.library.as_ref().map(Library::import_counts);
        let phase = self.session.as_ref().map(|session| session.phase);
        egui::Panel::top("top")
            .exact_size(46.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let (mark, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                    ui.painter()
                        .rect_filled(mark, CornerRadius::same(5), ACCENT);
                    ui.painter().text(
                        mark.center(),
                        Align2::CENTER_CENTER,
                        "I",
                        FontId::proportional(13.0),
                        Color32::WHITE,
                    );
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(RichText::new("IN").strong().color(INK).size(14.0));
                    ui.label(RichText::new("GOT").strong().color(ACCENT_2).size(14.0));
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.add_space(10.0);
                    if button(ui, "+  Import", ButtonKind::Primary)
                        .on_hover_text("Cmd+Shift+I")
                        .clicked()
                    {
                        self.open_launchpad();
                    }
                    let open = self.session.is_some();
                    ui.add_enabled_ui(open, |ui| {
                        egui::Frame::new()
                            .fill(PANEL_2)
                            .stroke(Stroke::new(1.0, LINE))
                            .corner_radius(CornerRadius::same(5))
                            .inner_margin(2)
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                for (label, view, key) in [
                                    ("Grid", View::Grid, "G"),
                                    ("Loupe", View::Loupe, "E"),
                                    ("Clusters", View::Clusters, "L"),
                                    ("Develop", View::Develop, "D"),
                                ] {
                                    if ui
                                        .selectable_label(
                                            self.view == view,
                                            RichText::new(label).size(12.0),
                                        )
                                        .on_hover_text(key)
                                        .clicked()
                                    {
                                        self.view = view;
                                    }
                                }
                            });
                    });
                    if self.view == View::Survey {
                        ui.label(
                            RichText::new(format!("Comparing {}", self.survey.len()))
                                .color(ACCENT_2),
                        );
                        if ui.small_button("x").on_hover_text("Exit compare").clicked() {
                            self.view = View::Grid;
                        }
                    }
                    if let Some(session) = &self.session {
                        ui.label(
                            RichText::new(&session.name)
                                .monospace()
                                .size(11.5)
                                .color(INK_2),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("?")
                            .on_hover_text("Keyboard shortcuts ( / )")
                            .clicked()
                        {
                            self.show_help = true;
                        }
                        let Some(counts) = counts else {
                            return;
                        };
                        let finalize = match phase {
                            Some(Phase::Copying) => Some("Finalize import"),
                            Some(Phase::Finalizing) => Some("Finalizing…"),
                            _ => None,
                        };
                        if let Some(label) = finalize {
                            let enabled = phase == Some(Phase::Copying);
                            if ui
                                .add_enabled_ui(enabled, |ui| {
                                    button(ui, label, ButtonKind::Primary)
                                })
                                .inner
                                .on_hover_text(
                                    "Keep your picks, take back the rest, and finish the copy.",
                                )
                                .clicked()
                            {
                                self.finalize = Some(FinalizeDialog {
                                    keep_unflagged: false,
                                });
                            }
                        }
                        if ui
                            .add_enabled_ui(counts.rejects > 0, |ui| {
                                button(
                                    ui,
                                    &format!("Delete rejected  {}", counts.rejects),
                                    ButtonKind::Danger,
                                )
                            })
                            .inner
                            .clicked()
                        {
                            self.open_sweep();
                        }
                        if button(ui, "Save metadata", ButtonKind::Normal).clicked() {
                            self.save_open = true;
                        }
                        ui.label(
                            RichText::new(format!("{}", counts.rated))
                                .monospace()
                                .size(11.5)
                                .color(INK_3),
                        );
                        paint_inline_star(ui);
                        ui.label(RichText::new(format!("{}", counts.rejects)).color(REJECT));
                        paint_inline_dot(ui, REJECT);
                        ui.label(RichText::new(format!("{}", counts.picks)).color(ACCEPT));
                        paint_inline_dot(ui, ACCEPT);
                    });
                });
            });
    }

    /// The design's ingest bar: status, then a track per destination.
    fn ingest_bar(&mut self, ui: &mut egui::Ui) {
        let counts = self
            .library
            .as_ref()
            .map(Library::import_counts)
            .unwrap_or_default();
        egui::Panel::bottom("ingest")
            .exact_size(46.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let phase = self.session.as_ref().map(|session| session.phase);
                    let running = phase == Some(Phase::Finalizing)
                        || (phase == Some(Phase::Copying) && counts.pending() > 0);
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                    ui.painter().circle_filled(
                        dot.center(),
                        4.0,
                        if running { ACCEPT } else { INK_3 },
                    );
                    let title = match (self.scanning, phase) {
                        (true, _) => "Listing".to_string(),
                        (_, Some(Phase::Finalizing)) => "Finalizing".to_string(),
                        (_, Some(Phase::Finalized)) => "Finalized".to_string(),
                        (_, Some(Phase::Copying)) if running => "Ingesting".to_string(),
                        (_, Some(Phase::Copying)) => "Copied, choose and finalize".to_string(),
                        _ => "Idle".to_string(),
                    };
                    ui.label(RichText::new(title).color(INK).strong().size(12.0));
                    if self.session.is_some() {
                        ui.label(
                            RichText::new(format!("{}/{}", counts.copied, counts.files))
                                .monospace()
                                .size(11.0)
                                .color(INK_3),
                        );
                        ui.separator();
                    }
                    self.target_tracks(ui, counts.files);
                    if self.copy.failed > 0 {
                        let errors = self.copy.last_errors.join("\n");
                        ui.label(
                            RichText::new(format!("{} failed", self.copy.failed)).color(REJECT),
                        )
                        .on_hover_text(errors);
                    }
                    if let Some(status) = &self.status {
                        ui.label(RichText::new(status).color(INK_2).size(11.0));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running
                            && let Some(eta) = self.eta_seconds(counts.pending(), counts.copied)
                        {
                            ui.label(
                                RichText::new(format!("ETA {}:{:02}", eta / 60, eta % 60))
                                    .monospace()
                                    .size(11.0)
                                    .color(INK_2),
                            );
                        }
                        if let Some(warning) = self.library.as_ref().and_then(Library::last_warning)
                        {
                            ui.label(RichText::new("Warning").color(STAR))
                                .on_hover_text(warning);
                        }
                        if self.ignored > 0 {
                            ui.label(
                                RichText::new(format!("{} files not imported", self.ignored))
                                    .color(INK_3)
                                    .size(11.0),
                            )
                            .on_hover_text("Videos and other unsupported file types are skipped.");
                        }
                        if self.scan_errors > 0 {
                            ui.label(
                                RichText::new(format!("{} scan errors", self.scan_errors))
                                    .color(REJECT),
                            );
                        }
                        if self.session.is_some() && !self.previews_done {
                            ui.spinner();
                        }
                        if let Some(line) = self.destination_line(counts.files) {
                            ui.add(
                                egui::Label::new(RichText::new(line).color(INK_3).size(10.5))
                                    .truncate(),
                            );
                        }
                    });
                });
            });
    }

    fn target_tracks(&self, ui: &mut egui::Ui, files: usize) {
        let (Some(session), Some(library)) = (&self.session, &self.library) else {
            return;
        };
        for target in &session.targets {
            let done = library.target_done(target.id);
            let waiting = target.kind == TargetType::Cloud && session.phase == Phase::Copying;
            ui.label(RichText::new(&target.name).color(INK_2).size(11.0));
            let active = if waiting || done >= files {
                0
            } else {
                target.slots
            };
            ui.label(
                RichText::new(if waiting {
                    "after Finalize".to_string()
                } else {
                    format!("{active}/{}", target.slots)
                })
                .monospace()
                .size(10.5)
                .color(INK_3),
            );
            let (track, _) = ui.allocate_exact_size(Vec2::new(90.0, 4.0), Sense::hover());
            ui.painter()
                .rect_filled(track, CornerRadius::same(2), PANEL_3);
            let fraction = if files == 0 {
                0.0
            } else {
                (done as f32 / files as f32).min(1.0)
            };
            let mut filled = track;
            filled.set_width(track.width() * fraction);
            ui.painter().rect_filled(
                filled,
                CornerRadius::same(2),
                if target.kind == TargetType::Cloud {
                    ACCENT_2
                } else {
                    ACCENT
                },
            );
            ui.add_space(8.0);
        }
    }

    fn eta_seconds(&self, pending: usize, copied: usize) -> Option<u64> {
        let elapsed = self.copy.started?.elapsed().as_secs_f64();
        (copied > 0).then(|| (elapsed / copied as f64 * pending as f64) as u64)
    }

    /// One example path and how many more. Instant, and touches no disk.
    fn destination_line(&self, files: usize) -> Option<String> {
        let session = self.session.as_ref()?;
        let library = self.library.as_ref()?;
        let source = self.source.as_ref()?;
        let photo = library.photos.first()?;
        let file = photo.files.first()?;
        let relative = planned_relative(
            file,
            source,
            &session.options,
            session.options.start_number,
            photo.taken?,
        )
        .ok()?;
        let home = dirs::home_dir();
        Some(destination_summary(
            &session.destination.join(relative),
            files,
            home.as_deref(),
        ))
    }

    /// The first screen, until an import is running.
    fn launchpad_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| {
                if self.scanning {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(30.0);
                        ui.allocate_ui(Vec2::new(860.0_f32.min(ui.available_width()), 0.0), |ui| {
                            self.launchpad_card(ui, false);
                        });
                    });
                });
            });
    }

    fn launchpad_overlay(&mut self, ctx: &egui::Context) {
        egui::Modal::new(egui::Id::unique("launchpad")).show(ctx, |ui| {
            ui.set_width(860.0_f32.min(ctx.content_rect().width() - 80.0));
            self.launchpad_card(ui, true);
        });
    }

    /// The design's launchpad, in the order the import really happens: name and
    /// destination first, and choosing the source is what starts it.
    fn launchpad_card(&mut self, ui: &mut egui::Ui, closable: bool) {
        let mut chosen = None;
        ui.vertical(|ui| {
            ui.label(RichText::new("New import").size(22.0).color(INK).strong());
            ui.label(
                RichText::new(
                    "Choose where the photos go, then pick the card. Everything starts copying at once and the grid opens, so you can choose images while it runs. Finalize then keeps your picks and takes back the rest.",
                )
                .color(INK_3),
            );
            ui.add_space(12.0);
            ui.columns(2, |columns| {
                let form_ui = &mut columns[0];
                section(form_ui, "1  DESTINATION");
                self.import_form(form_ui);
                let source_ui = &mut columns[1];
                section(source_ui, "2  SOURCE");
                let ready = !self.form.destination.trim().is_empty();
                source_ui.label(
                    RichText::new(if ready {
                        "Choosing a source starts the import."
                    } else {
                        "Choose a destination first."
                    })
                    .color(INK_3)
                    .small(),
                );
                source_ui.add_space(4.0);
                source_ui.add_enabled_ui(ready, |ui| {
                    let mut sources = self.cards.clone();
                    if let Some(suggested) = &self.suggested_source
                        && !sources.contains(suggested)
                    {
                        sources.insert(0, suggested.clone());
                    }
                    for source in sources {
                        if source_row(ui, &source).clicked() {
                            chosen = Some(source);
                        }
                    }
                    if button(ui, "Choose a folder…", ButtonKind::Normal).clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        chosen = Some(folder);
                    }
                });
                if let Some(error) = &self.form.error {
                    source_ui.add_space(8.0);
                    source_ui.label(RichText::new(error).color(REJECT));
                }
            });
            if closable {
                ui.add_space(10.0);
                if button(ui, "Close", ButtonKind::Normal).clicked() {
                    self.import_open = false;
                }
            }
        });
        if let Some(source) = chosen {
            self.choose_source(source);
        }
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        let mut pick_all = false;
        let mut survey = false;
        let renaming = self.renaming.clone();
        let mut new_renaming = None;
        let mut rename_commit: Option<(u64, String)> = None;
        let current = self.session.as_ref().map(|session| session.id);
        let total = self.imports.total_photos();
        let entries = self.imports.entries.clone();
        let Some(library) = &mut self.library else {
            return;
        };
        egui::Panel::left("left")
            .exact_size(258.0)
            .frame(egui::Frame::new().fill(PANEL))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(8.0);
                    psec_h(ui, "IMPORTS");
                    panel_row(ui, false, "All Imports", None, total.to_string());
                    ui.add_space(4.0);
                    for entry in entries.iter().rev() {
                        if let Some((id, draft)) = &renaming
                            && *id == entry.id
                        {
                            let mut draft = draft.clone();
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut draft)
                                    .desired_width(230.0)
                                    .hint_text("Import name"),
                            );
                            response.request_focus();
                            let done = ui.input(|i| i.key_pressed(Key::Enter));
                            let cancelled = ui.input(|i| i.key_pressed(Key::Escape));
                            if done {
                                rename_commit = Some((entry.id, draft));
                            } else if cancelled {
                                new_renaming = Some(None);
                            } else {
                                new_renaming = Some(Some((entry.id, draft)));
                            }
                            continue;
                        }
                        let response = panel_row(
                            ui,
                            Some(entry.id) == current,
                            &entry.name,
                            None,
                            entry.photos.to_string(),
                        )
                        .on_hover_text(format!(
                            "{}\nDouble-click to rename{}",
                            entry.destination.display(),
                            if entry.finalized { "\nFinalized" } else { "" }
                        ));
                        if response.double_clicked() {
                            new_renaming = Some(Some((entry.id, entry.name.clone())));
                        }
                    }
                    ui.add_space(8.0);

                    if library.selected.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.add_space(9.0);
                            ui.label(
                                RichText::new(format!("{} selected", library.selected.len()))
                                    .color(ACCENT_2),
                            );
                            survey = ui.small_button("Compare").clicked();
                            if ui.small_button("Clear").clicked() {
                                library.clear_selection();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(9.0);
                            ui.label(
                                RichText::new("P/X flag, 1-5 rate, N compare: applies to all")
                                    .color(INK_3)
                                    .small(),
                            );
                        });
                        ui.add_space(8.0);
                    }

                    psec_h(ui, "FILTER");
                    let [all, picks, unflagged, rejected] = library.filter_counts();
                    let current_filter = library.filter;
                    for (label, filter, dot, count) in [
                        ("All photos", Filter::All, None, all),
                        ("Picks", Filter::Picked, Some(ACCEPT), picks),
                        ("Unflagged", Filter::Unflagged, Some(INK_3), unflagged),
                        ("Rejected", Filter::Rejected, Some(REJECT), rejected),
                    ] {
                        if panel_row(ui, current_filter == filter, label, dot, count.to_string())
                            .clicked()
                        {
                            library.set_filter(filter);
                        }
                    }
                    ui.add_space(6.0);
                    let up = match current_filter {
                        Filter::Stars(stars) => stars,
                        _ => 0,
                    };
                    let down = match current_filter {
                        Filter::StarsDown(stars) => stars,
                        _ => 0,
                    };
                    if let Some(stars) = star_filter_row(ui, up, "& up") {
                        library.set_filter(if stars == up {
                            Filter::All
                        } else {
                            Filter::Stars(stars)
                        });
                    }
                    if let Some(stars) = star_filter_row(ui, down, "& down") {
                        library.set_filter(if stars == down {
                            Filter::All
                        } else {
                            Filter::StarsDown(stars)
                        });
                    }
                    ui.add_space(10.0);
                    psec_h(ui, "CULLING");
                    ui.horizontal(|ui| {
                        ui.add_space(9.0);
                        ui.checkbox(&mut library.auto_advance, "Move on after a flag")
                            .on_hover_text("A");
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(9.0);
                        pick_all = ui
                            .small_button("Pick all shown")
                            .on_hover_text("Marks every photo shown as a pick.")
                            .clicked();
                    });
                    if matches!(self.view, View::Grid | View::Clusters) {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add_space(9.0);
                            ui.add(
                                egui::Slider::new(&mut self.cell, 110.0..=420.0)
                                    .show_value(false)
                                    .text("Size"),
                            );
                        });
                    }
                });
            });
        if let Some((id, name)) = rename_commit {
            if let Err(err) = self.imports.rename(id, &name) {
                self.status = Some(format!("Could not save the name: {err}"));
            }
            if let Some(session) = &mut self.session
                && session.id == id
                && !name.trim().is_empty()
            {
                session.name = name.trim().to_string();
            }
            self.renaming = None;
        } else if let Some(next) = new_renaming {
            self.renaming = next;
        }
        if pick_all && let Some(library) = &mut self.library {
            library.select_all();
            let already = library.import_counts().picks == library.visible.len();
            if !already {
                self.apply_flag(TriageState::Accepted, false);
            }
            if let Some(library) = &mut self.library {
                library.clear_selection();
            }
        }
        if survey && let Some(library) = &self.library {
            self.survey = library.survey_positions();
            self.view = View::Survey;
        }
    }

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        if self.library.as_ref().and_then(|l| l.focused()).is_none() {
            return;
        }
        let mut changed = false;
        let mut export = false;
        egui::Panel::right("right")
            .exact_size(296.0)
            .frame(egui::Frame::new().fill(PANEL))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.photo_details(ui);
                    if self.view == View::Develop {
                        ui.add_space(12.0);
                        egui::Frame::new().inner_margin(13).show(ui, |ui| {
                            changed = self.develop_controls(ui, &mut export);
                        });
                    }
                });
            });
        if changed {
            self.commit_params();
        }
        if export {
            self.export();
        }
    }

    fn photo_details(&mut self, ui: &mut egui::Ui) {
        let Some(library) = &self.library else {
            return;
        };
        let Some(photo) = library.focused().cloned() else {
            return;
        };
        let (rating, state) = library.decisions(&photo.key);
        let capture = library.capture(&photo.key);
        let histogram = library.histogram(&photo.key).map(<[u32]>::to_vec);
        let selected = library.selected.len();
        let sizes: Vec<(String, u64)> = {
            let scan = library.store.read().unwrap_or_else(|e| e.into_inner());
            photo
                .files
                .iter()
                .map(|file| {
                    (
                        file.extension()
                            .map(|e| e.to_string_lossy().to_uppercase())
                            .unwrap_or_default(),
                        scan.file_meta.get(file).map_or(0, |meta| meta.size),
                    )
                })
                .collect()
        };
        let replication: Vec<(String, f32, String, bool)> =
            self.session.as_ref().map_or(vec![], |session| {
                session
                    .targets
                    .iter()
                    .map(|target| {
                        let done = photo
                            .files
                            .iter()
                            .filter(|file| library.reached(file, target.id))
                            .count();
                        let total = photo.files.len().max(1);
                        let finished = done >= total;
                        let label = if finished {
                            "done"
                        } else if target.kind == TargetType::Cloud
                            && session.phase == Phase::Copying
                        {
                            "after Finalize"
                        } else if state == TriageState::Rejected {
                            "held"
                        } else {
                            "queued"
                        };
                        (
                            target.name.clone(),
                            done as f32 / total as f32,
                            label.to_string(),
                            finished,
                        )
                    })
                    .collect()
            });

        egui::Frame::new().inner_margin(13).show(ui, |ui| {
            let path = Path::new(&photo.name);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(
                    RichText::new(
                        path.file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    )
                    .monospace()
                    .strong()
                    .color(INK),
                );
                if let Some(extension) = path.extension() {
                    ui.label(
                        RichText::new(format!(".{}", extension.to_string_lossy()))
                            .monospace()
                            .color(ACCENT_2),
                    );
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for (extension, size) in &sizes {
                    pair_tag(ui, extension, *size);
                }
            });
            if let Some(taken) = photo.captured.or(photo.taken) {
                ui.label(
                    RichText::new(format!("Captured {}", taken.format("%Y-%m-%d %H:%M:%S")))
                        .color(INK_3)
                        .size(11.0),
                );
            }
        });
        ui.separator();

        let mut flag = None;
        let mut rate = None;
        egui::Frame::new().inner_margin(13).show(ui, |ui| {
            ui.horizontal(|ui| {
                section(ui, "FLAG & RATING");
                if selected > 1 {
                    ui.label(
                        RichText::new(format!("{selected} selected"))
                            .monospace()
                            .size(11.0)
                            .color(ACCENT_2),
                    );
                }
            });
            ui.columns(2, |columns| {
                if flag_button(
                    &mut columns[0],
                    "Pick  P",
                    state == TriageState::Accepted,
                    ACCEPT,
                ) {
                    flag = Some(TriageState::Accepted);
                }
                if flag_button(
                    &mut columns[1],
                    "Reject  X",
                    state == TriageState::Rejected,
                    REJECT,
                ) {
                    flag = Some(TriageState::Rejected);
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                rate = star_row(ui, rating, 22.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(if rating == 0 {
                            "-/5".to_string()
                        } else {
                            format!("{rating}/5")
                        })
                        .monospace()
                        .size(11.0)
                        .color(INK_3),
                    );
                });
            });
        });
        ui.separator();

        psec_h(ui, "HISTOGRAM");
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 64.0), Sense::hover());
        if let Some(histogram) = &histogram {
            paint_histogram(ui.painter(), rect.shrink2(Vec2::new(13.0, 8.0)), histogram);
        }
        ui.separator();

        psec_h(ui, "CAPTURE");
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(13, 4))
            .show(ui, |ui| {
                egui::Grid::new("capture").num_columns(2).show(ui, |ui| {
                    for (label, value) in [
                        ("Camera", &capture.camera),
                        ("Lens", &capture.lens),
                        ("Focal", &capture.focal),
                        ("Aperture", &capture.aperture),
                        ("Shutter", &capture.shutter),
                        ("ISO", &capture.iso),
                        ("Pixels", &capture.pixels),
                    ] {
                        ui.label(RichText::new(label).color(INK_3).size(11.0));
                        ui.label(
                            RichText::new(value.as_deref().unwrap_or("-"))
                                .monospace()
                                .size(11.0)
                                .color(INK),
                        );
                        ui.end_row();
                    }
                });
            });
        ui.separator();

        psec_h(ui, "REPLICATION");
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(13, 4))
            .show(ui, |ui| {
                for (name, fraction, label, finished) in &replication {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(name).color(INK_2).size(11.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(label).monospace().size(10.5).color(INK_2));
                        });
                    });
                    let (track, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
                    ui.painter()
                        .rect_filled(track, CornerRadius::same(2), PANEL_3);
                    let mut filled = track;
                    filled.set_width(track.width() * fraction);
                    ui.painter().rect_filled(
                        filled,
                        CornerRadius::same(2),
                        if *finished { ACCEPT } else { ACCENT },
                    );
                    ui.add_space(6.0);
                }
            });

        if let Some(flag) = flag {
            self.apply_flag(flag, false);
        }
        if let (Some(stars), Some(library)) = (rate, &mut self.library) {
            library.rate(if stars == rating { 0 } else { stars }, false);
        }
    }

    fn develop_controls(&mut self, ui: &mut egui::Ui, export: &mut bool) -> bool {
        let mut changed = false;
        let params = &mut self.params;
        section(ui, "WHITE BALANCE");
        ui.horizontal(|ui| {
            for (label, balance) in [
                ("As shot", WhiteBalance::Camera),
                ("Daylight", WhiteBalance::Daylight),
            ] {
                if ui
                    .selectable_label(params.white_balance == balance, label)
                    .clicked()
                {
                    params.white_balance = balance;
                    changed = true;
                }
            }
        });
        changed |= slider(ui, "Temp", &mut params.temperature, -100.0..=100.0, 0.0);
        changed |= slider(ui, "Tint", &mut params.tint, -100.0..=100.0, 0.0);
        ui.add_space(8.0);
        section(ui, "TONE");
        changed |= slider(ui, "Exposure", &mut params.exposure, -5.0..=5.0, 0.7);
        changed |= slider(ui, "Contrast", &mut params.contrast, 0.5..=4.0, 1.5);
        changed |= slider(
            ui,
            "Highlights",
            &mut params.highlights,
            -100.0..=100.0,
            0.0,
        );
        changed |= slider(ui, "Shadows", &mut params.shadows, -100.0..=100.0, 0.0);
        changed |= slider(ui, "Whites", &mut params.whites, -100.0..=100.0, 0.0);
        changed |= slider(ui, "Blacks", &mut params.blacks, -100.0..=100.0, 0.0);
        changed |= slider(ui, "Skew", &mut params.skew, -1.0..=1.0, 0.0);
        ui.add_space(8.0);
        section(ui, "PRESENCE");
        changed |= slider(ui, "Vibrance", &mut params.vibrance, -100.0..=100.0, 0.0);
        changed |= slider(
            ui,
            "Saturation",
            &mut params.saturation,
            -100.0..=100.0,
            0.0,
        );
        changed |= slider(ui, "Hue hold", &mut params.hue_preservation, 0.0..=1.0, 1.0);
        ui.add_space(8.0);
        section(ui, "DETAIL");
        changed |= slider(ui, "Sharpening", &mut params.sharpen, 0.0..=2.0, 0.5);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Reset").clicked() {
                *params = DevelopParams::default();
                changed = true;
            }
            if ui.button("Export JPEG…").clicked() {
                *export = true;
            }
        });
        ui.label(
            RichText::new("Double-click a slider to reset it.")
                .color(INK_3)
                .small(),
        );
        changed
    }

    fn stage(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(STAGE))
            .show(ui, |ui| match self.view {
                View::Grid => self.grid(ui),
                View::Clusters => self.clusters_view(ui),
                View::Survey => self.survey_view(ui),
                View::Loupe => self.loupe(ui),
                View::Develop => self.develop_stage(ui),
            });
    }

    /// The best image ready for a photo, asking for better ones as they become available.
    fn texture_for(&mut self, photo: &Photo, want_preview: bool) -> Option<TextureHandle> {
        let library = self.library.as_ref()?;
        let store = Arc::clone(&library.store);
        let bytes = |pick: fn(&ingot_core::PhotoAsset) -> Option<Vec<u8>>| {
            let store = Arc::clone(&store);
            let key = photo.key.clone();
            move || {
                let scan = store.read().unwrap_or_else(|e| e.into_inner());
                scan.assets.get(&key).and_then(pick)
            }
        };
        let preview = want_preview
            .then(|| {
                self.images.get(
                    &photo.key,
                    Tier::Preview,
                    photo.orientation,
                    bytes(|asset| asset.image_data.preview.clone()),
                )
            })
            .flatten();
        if preview.is_some() {
            return preview;
        }
        let grid = self.images.get(
            &photo.key,
            Tier::Grid,
            photo.orientation,
            bytes(|asset| asset.image_data.thumb.clone()),
        );
        if grid.is_some() {
            return grid;
        }
        self.images.get(
            &photo.key,
            Tier::Header,
            photo.orientation,
            bytes(|asset| asset.exif_data.thumbnail.clone()),
        )
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        let Some(library) = &self.library else {
            return;
        };
        let count = library.visible.len();
        if count == 0 {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No photos match this filter.").color(INK_3));
            });
            return;
        }
        let spacing = 12.0;
        let width = ui.available_width() - 28.0;
        let columns = (((width + spacing) / (self.cell + spacing)) as usize).max(1);
        self.columns = columns;
        let cell = (width - spacing * (columns - 1) as f32) / columns as f32;
        let row_height = cell * 2.0 / 3.0 + spacing;
        let rows = count.div_ceil(columns);

        ui.add_space(14.0);
        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if self.scroll_to_focus {
            let focus_row = library.focus / columns;
            let offset = (focus_row as f32 * row_height + row_height / 2.0
                - ui.available_height() / 2.0)
                .max(0.0);
            area = area.vertical_scroll_offset(offset);
            self.scroll_to_focus = false;
        }
        let mut clicked = None;
        let mut opened = None;
        area.show_rows(ui, row_height - spacing, rows, |ui, range| {
            ui.spacing_mut().item_spacing = Vec2::splat(spacing);
            for row in range {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    for column in 0..columns {
                        let position = row * columns + column;
                        let Some(index) = self
                            .library
                            .as_ref()
                            .and_then(|library| library.visible.get(position).copied())
                        else {
                            break;
                        };
                        let response =
                            self.grid_cell(ui, index, position, cell, row_height - spacing);
                        if response.double_clicked() {
                            opened = Some(position);
                        } else if response.clicked() {
                            clicked = Some(position);
                        }
                    }
                });
            }
        });
        if let Some(library) = &mut self.library {
            if let Some(position) = clicked {
                let modifiers = ui.input(|input| input.modifiers);
                library.click(position, modifiers.command, modifiers.shift);
            }
            if let Some(position) = opened {
                library.jump(position);
                self.view = View::Loupe;
            }
        }
    }

    /// A thumbnail per the design: overlays on the image, with the flag as a chip and
    /// as the border, and a rejected photo dimmed. The image is fitted rather than
    /// cropped as in the design, because a culler has to see the whole frame.
    fn grid_cell(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        position: usize,
        width: f32,
        height: f32,
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let Some(library) = &self.library else {
            return response;
        };
        let photo = library.photos[index].clone();
        let focused = library.focus == position;
        let selected = library.selected.contains(&index) && library.selected.len() > 1;
        let imported = library.already_imported.contains(&photo.key);
        let (rating, state) = library.decisions(&photo.key);
        let texture = self.texture_for(&photo, false);

        let painter = ui.painter_at(rect);
        painter.rect_filled(
            rect,
            CornerRadius::same(5),
            Color32::from_rgb(0x1a, 0x1a, 0x1d),
        );
        if let Some(texture) = texture {
            let tint = if state == TriageState::Rejected {
                Color32::from_gray(100)
            } else {
                Color32::WHITE
            };
            draw_fit(&painter, &texture, rect.shrink(2.0), tint);
        }
        if width >= 90.0 && photo.raw.is_some() {
            let label = if photo.has_jpeg { "RAW+JPG" } else { "RAW" };
            let badge = Rect::from_min_size(
                rect.min + Vec2::splat(7.0),
                Vec2::new(if photo.has_jpeg { 52.0 } else { 30.0 }, 15.0),
            );
            painter.rect_filled(badge, CornerRadius::same(4), Color32::from_black_alpha(170));
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::monospace(9.0),
                Color32::from_rgb(0xd6, 0xd7, 0xdb),
            );
        }
        let chip = Rect::from_center_size(
            Pos2::new(rect.max.x - 16.0, rect.min.y + 16.0),
            Vec2::splat(18.0),
        );
        match state {
            TriageState::Accepted => paint_flag_chip(&painter, chip, true),
            TriageState::Rejected => paint_flag_chip(&painter, chip, false),
            TriageState::Pending => {}
        }
        let band = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - 22.0), rect.max);
        painter.rect_filled(band, CornerRadius::same(5), Color32::from_black_alpha(150));
        let stem = Path::new(&photo.name)
            .file_stem()
            .map_or_else(|| photo.name.clone(), |s| s.to_string_lossy().into_owned());
        painter.text(
            Pos2::new(rect.min.x + 7.0, rect.max.y - 11.0),
            Align2::LEFT_CENTER,
            if imported {
                format!("{stem}  imported")
            } else {
                stem
            },
            FontId::monospace(10.0),
            Color32::from_rgb(0xe7, 0xe7, 0xea),
        );
        for star in 0..rating {
            let center = Pos2::new(rect.max.x - 11.0 - star as f32 * 12.0, rect.max.y - 11.0);
            paint_star(&painter, center, 5.5, STAR);
        }
        let border = if selected {
            Some((ACCENT, 2.0))
        } else if focused {
            Some((ACCENT_2, 2.0))
        } else if state == TriageState::Accepted {
            Some((ACCEPT, 2.0))
        } else {
            None
        };
        if let Some((color, thickness)) = border {
            painter.rect_stroke(
                rect,
                CornerRadius::same(5),
                Stroke::new(thickness, color),
                StrokeKind::Inside,
            );
        }
        response
    }

    /// Compare: the chosen photos side by side, each with its own flag and rating.
    fn survey_view(&mut self, ui: &mut egui::Ui) {
        let positions = self.survey.clone();
        if positions.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(
                        "Select photos in the grid with Cmd-click or Shift-click, then press N.",
                    )
                    .color(INK_3),
                );
            });
            return;
        }
        let columns = (positions.len() as f32).sqrt().ceil() as usize;
        let rows = positions.len().div_ceil(columns);
        let area = ui.available_rect_before_wrap().shrink(6.0);
        let cell = Vec2::new(area.width() / columns as f32, area.height() / rows as f32);
        let mut focus_on = None;
        let mut flag = None;
        let mut rate = None;
        for (slot, position) in positions.iter().enumerate() {
            let Some(photo) = self
                .library
                .as_ref()
                .and_then(|library| library.visible.get(*position))
                .and_then(|index| self.library.as_ref().map(|l| l.photos[*index].clone()))
            else {
                continue;
            };
            let at = area.min
                + Vec2::new(
                    (slot % columns) as f32 * cell.x,
                    (slot / columns) as f32 * cell.y,
                );
            let rect = Rect::from_min_size(at, cell).shrink(4.0);
            let (rating, state, focused) = {
                let library = self.library.as_ref();
                let (rating, state) =
                    library.map_or((0, TriageState::Pending), |l| l.decisions(&photo.key));
                (rating, state, library.is_some_and(|l| l.focus == *position))
            };
            let texture = self.texture_for(&photo, true);
            let response = ui.interact(rect, ui.scope_id().with(("survey", slot)), Sense::click());
            if response.clicked() {
                focus_on = Some(*position);
            }
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::same(5), PANEL_2);
            let image = Rect::from_min_max(rect.min, rect.max - Vec2::new(0.0, 30.0));
            if let Some(texture) = texture {
                let tint = if state == TriageState::Rejected {
                    Color32::from_gray(100)
                } else {
                    Color32::WHITE
                };
                draw_fit(&painter, &texture, image.shrink(3.0), tint);
            }
            if state != TriageState::Pending {
                paint_flag_chip(
                    &painter,
                    Rect::from_min_size(image.min + Vec2::splat(7.0), Vec2::splat(18.0)),
                    state == TriageState::Accepted,
                );
            }
            if focused {
                painter.rect_stroke(
                    rect,
                    CornerRadius::same(5),
                    Stroke::new(2.0, ACCENT_2),
                    StrokeKind::Inside,
                );
            }
            let bar = Rect::from_min_max(Pos2::new(rect.min.x + 8.0, image.max.y), rect.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new(&photo.name)
                            .monospace()
                            .size(11.0)
                            .color(INK_2),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        let row =
                            ui.allocate_ui(Vec2::new(90.0, 22.0), |ui| star_row(ui, rating, 15.0));
                        if let Some(stars) = row.inner {
                            rate = Some((*position, if stars == rating { 0 } else { stars }));
                        }
                        if ui.small_button(RichText::new("P").color(ACCEPT)).clicked() {
                            flag = Some((*position, TriageState::Accepted));
                        }
                        if ui.small_button(RichText::new("X").color(REJECT)).clicked() {
                            flag = Some((*position, TriageState::Rejected));
                        }
                    });
                });
            });
        }
        if let Some(position) = focus_on
            && let Some(library) = &mut self.library
        {
            library.jump(position);
        }
        if let Some((position, flag)) = flag {
            if let Some(library) = &mut self.library {
                library.jump(position);
                library.selected.clear();
            }
            self.apply_flag(flag, false);
        }
        if let Some((position, stars)) = rate
            && let Some(library) = &mut self.library
        {
            library.jump(position);
            library.selected.clear();
            library.rate(stars, false);
        }
    }

    /// Groups of similar photos, with the sensitivity slider. Positions are the same
    /// as in the grid because groups run in capture order, so all the culling keys and
    /// the rest of the app behave identically here.
    fn clusters_view(&mut self, ui: &mut egui::Ui) {
        let Some(library) = &mut self.library else {
            return;
        };
        let mut sensitivity = library.sensitivity;
        let ranges = library.clusters().to_vec();
        let hashed = library.fingerprint_count();
        let total_photos = library.photos.len();
        let groups: Vec<std::ops::Range<usize>> = ranges
            .iter()
            .map(|range| {
                let start = library.visible.partition_point(|i| *i < range.start);
                let end = library.visible.partition_point(|i| *i < range.end);
                start..end
            })
            .filter(|group| !group.is_empty())
            .collect();
        let visible = library.visible.len();
        let focus = library.focus;

        egui::Frame::new()
            .fill(PANEL_2)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} groups", groups.len()))
                            .color(INK)
                            .strong(),
                    );
                    ui.label(RichText::new(format!("from {visible} photos")).color(INK_3));
                    if hashed < total_photos {
                        ui.label(
                            RichText::new(format!("{hashed} of {total_photos} analyzed"))
                                .color(INK_3)
                                .small(),
                        );
                    }
                    ui.separator();
                    ui.label(RichText::new("Sensitivity").color(INK_2));
                    ui.label(RichText::new("scene only").color(INK_3).small());
                    let response = ui.add(
                        egui::Slider::new(&mut sensitivity, 0.0..=100.0)
                            .show_value(false)
                            .trailing_fill(true),
                    );
                    ui.label(RichText::new("+ subject position").color(INK_3).small());
                    ui.label(RichText::new(format!("{sensitivity:.0}")).color(ACCENT));
                    if response.double_clicked() {
                        sensitivity = DEFAULT_SENSITIVITY;
                    }
                });
            });
        if let Some(library) = &mut self.library
            && library.sensitivity != sensitivity
        {
            library.set_sensitivity(sensitivity);
        }
        if groups.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No photos match this filter.").color(INK_3));
            });
            return;
        }

        let spacing = 12.0;
        let header = 30.0;
        let width = ui.available_width() - 28.0;
        let columns = (((width + spacing) / (self.cell + spacing)) as usize).max(1);
        self.columns = columns;
        let cell = (width - spacing * (columns - 1) as f32) / columns as f32;
        let row_height = cell * 2.0 / 3.0 + spacing;

        // Where each group starts, so only what is on screen gets drawn.
        let mut tops = Vec::with_capacity(groups.len());
        let mut total = 0.0;
        for group in &groups {
            tops.push(total);
            total += header + group.len().div_ceil(columns) as f32 * row_height + spacing;
        }

        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if self.scroll_to_focus {
            if let Some(index) = groups.iter().position(|group| group.contains(&focus)) {
                let row = (focus - groups[index].start) / columns;
                let y = tops[index] + header + row as f32 * row_height;
                area = area.vertical_scroll_offset(
                    (y + row_height / 2.0 - ui.available_height() / 2.0).max(0.0),
                );
            }
            self.scroll_to_focus = false;
        }

        let (mut clicked, mut opened) = (None, None);
        area.show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().min;
            ui.allocate_rect(
                Rect::from_min_size(origin, Vec2::new(width, total)),
                Sense::hover(),
            );
            ui.spacing_mut().item_spacing = Vec2::splat(spacing);
            for (number, group) in groups.iter().enumerate() {
                let top = tops[number];
                let height = header + group.len().div_ceil(columns) as f32 * row_height;
                if top > viewport.max.y || top + height < viewport.min.y {
                    continue;
                }
                self.cluster_header(ui, origin + Vec2::new(14.0, top), width, number, group);
                let rows = group.len().div_ceil(columns);
                for row in 0..rows {
                    let y = top + header + row as f32 * row_height;
                    if y > viewport.max.y || y + row_height < viewport.min.y {
                        continue;
                    }
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(14.0, y),
                        Vec2::new(width, row_height),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        ui.horizontal(|ui| {
                            for column in 0..columns {
                                let position = group.start + row * columns + column;
                                if position >= group.end {
                                    break;
                                }
                                let Some(index) = self
                                    .library
                                    .as_ref()
                                    .and_then(|library| library.visible.get(position).copied())
                                else {
                                    break;
                                };
                                let response =
                                    self.grid_cell(ui, index, position, cell, row_height - spacing);
                                if response.double_clicked() {
                                    opened = Some(position);
                                } else if response.clicked() {
                                    clicked = Some(position);
                                }
                            }
                        });
                    });
                }
            }
        });
        if let Some(library) = &mut self.library {
            if let Some(position) = clicked {
                let modifiers = ui.input(|input| input.modifiers);
                library.click(position, modifiers.command, modifiers.shift);
            }
            if let Some(position) = opened {
                library.jump(position);
                self.view = View::Loupe;
            }
        }
    }

    fn cluster_header(
        &self,
        ui: &egui::Ui,
        top_left: Pos2,
        width: f32,
        number: usize,
        group: &std::ops::Range<usize>,
    ) {
        let Some(library) = &self.library else {
            return;
        };
        let first = &library.photos[library.visible[group.start]];
        let picks = group
            .clone()
            .filter(|position| {
                let photo = &library.photos[library.visible[*position]];
                library.decisions(&photo.key).1 == TriageState::Accepted
            })
            .count();
        let when = first
            .captured
            .or(first.taken)
            .map(|taken| taken.format("%H:%M:%S").to_string())
            .unwrap_or_default();
        let painter = ui.painter();
        let y = top_left.y + 14.0;
        painter.circle_filled(
            Pos2::new(top_left.x + 6.0, y),
            5.0,
            CLUSTER_COLORS[number % CLUSTER_COLORS.len()],
        );
        painter.text(
            Pos2::new(top_left.x + 18.0, y),
            Align2::LEFT_CENTER,
            format!("{when}  {}", first.name),
            FontId::proportional(13.0),
            INK,
        );
        painter.text(
            Pos2::new(top_left.x + width - 8.0, y),
            Align2::RIGHT_CENTER,
            format!(
                "{} {}   {picks} {}",
                group.len(),
                if group.len() == 1 { "frame" } else { "frames" },
                if picks == 1 { "pick" } else { "picks" },
            ),
            FontId::proportional(11.0),
            INK_3,
        );
    }

    fn loupe(&mut self, ui: &mut egui::Ui) {
        let Some(photo) = self.library.as_ref().and_then(|l| l.focused().cloned()) else {
            return;
        };
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let full = self
            .full
            .as_ref()
            .filter(|(key, _)| self.zoomed && *key == photo.key)
            .map(|(_, texture)| texture.clone());
        if let Some(texture) = full {
            if response.dragged() {
                self.pan += response.drag_delta();
            }
            let size = texture.size_vec2() / ui.ctx().pixels_per_point();
            let max_pan = ((size - rect.size()) / 2.0).max(Vec2::ZERO);
            self.pan = self.pan.clamp(-max_pan, max_pan);
            let image = Rect::from_center_size(rect.center() + self.pan, size);
            painter.image(
                texture.id(),
                image,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            if let Some(texture) = self.texture_for(&photo, true) {
                draw_fit(&painter, &texture, rect.shrink(8.0), Color32::WHITE);
            }
            if self.zoomed {
                painter.text(
                    rect.center_top() + Vec2::new(0.0, 12.0),
                    Align2::CENTER_TOP,
                    "Loading 1:1…",
                    FontId::proportional(12.0),
                    INK_2,
                );
            }
        }
        if response.double_clicked() {
            self.zoomed = !self.zoomed;
            self.pan = Vec2::ZERO;
        }
        self.hud(&painter, rect, &photo);
        self.loupe_controls(ui, rect, &photo);
    }

    /// The design's loupe controls, over the image.
    fn loupe_controls(&mut self, ui: &mut egui::Ui, rect: Rect, photo: &Photo) {
        let Some(library) = &self.library else {
            return;
        };
        let (rating, state) = library.decisions(&photo.key);
        let copied = library.photo_copied(photo);
        let imported = library.already_imported.contains(&photo.key);
        let phase = self.session.as_ref().map(|session| session.phase);
        let mut flag = None;
        let mut rate = None;
        let mut step = 0;
        let nav = Vec2::new(30.0, 44.0);
        if ui
            .put(
                Rect::from_center_size(Pos2::new(rect.min.x + 26.0, rect.center().y), nav),
                egui::Button::new(RichText::new("<").size(16.0)),
            )
            .clicked()
        {
            step = -1;
        }
        if ui
            .put(
                Rect::from_center_size(Pos2::new(rect.max.x - 26.0, rect.center().y), nav),
                egui::Button::new(RichText::new(">").size(16.0)),
            )
            .clicked()
        {
            step = 1;
        }
        let bottom = rect.max.y - 26.0;
        let reject_text = RichText::new("Reject  X").color(if state == TriageState::Rejected {
            REJECT
        } else {
            INK_2
        });
        let pick_text = RichText::new("Pick  P").color(if state == TriageState::Accepted {
            ACCEPT
        } else {
            INK_2
        });
        let size = Vec2::new(90.0, 28.0);
        if ui
            .put(
                Rect::from_center_size(Pos2::new(rect.center().x - 50.0, bottom), size),
                egui::Button::new(reject_text),
            )
            .clicked()
        {
            flag = Some(TriageState::Rejected);
        }
        if ui
            .put(
                Rect::from_center_size(Pos2::new(rect.center().x + 50.0, bottom), size),
                egui::Button::new(pick_text),
            )
            .clicked()
        {
            flag = Some(TriageState::Accepted);
        }
        let stars = Rect::from_min_size(
            Pos2::new(rect.min.x + 14.0, bottom - 13.0),
            Vec2::new(150.0, 26.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(stars), |ui| {
            ui.horizontal(|ui| {
                rate = star_row(ui, rating, 20.0);
            });
        });
        let pill = if copied {
            ("copied", ACCEPT)
        } else if imported {
            ("already imported", INK_3)
        } else if state == TriageState::Rejected {
            ("held", INK_3)
        } else if phase == Some(Phase::Finalized) {
            ("not kept", INK_3)
        } else {
            ("queued", ACCENT_2)
        };
        ui.painter_at(rect).text(
            Pos2::new(rect.max.x - 14.0, bottom),
            Align2::RIGHT_CENTER,
            pill.0,
            FontId::proportional(11.0),
            pill.1,
        );
        if let Some(flag) = flag {
            self.apply_flag(flag, false);
        }
        if let Some(library) = &mut self.library {
            if let Some(stars) = rate {
                library.rate(if stars == rating { 0 } else { stars }, false);
            }
            if step != 0 {
                library.move_focus(step);
                self.scroll_to_focus = true;
            }
        }
    }

    fn hud(&self, painter: &egui::Painter, rect: Rect, photo: &Photo) {
        let Some(library) = &self.library else {
            return;
        };
        let (_, state) = library.decisions(&photo.key);
        painter.text(
            rect.left_top() + Vec2::new(12.0, 10.0),
            Align2::LEFT_TOP,
            format!(
                "{}   {}/{}{}",
                photo.name,
                library.focus + 1,
                library.visible.len(),
                if self.zoomed { "   1:1" } else { "" }
            ),
            FontId::proportional(12.0),
            INK_2,
        );
        let flag = match state {
            TriageState::Accepted => Some(("PICK", ACCEPT)),
            TriageState::Rejected => Some(("REJECT", REJECT)),
            TriageState::Pending => None,
        };
        if let Some((text, color)) = flag {
            painter.text(
                rect.right_top() + Vec2::new(-12.0, 10.0),
                Align2::RIGHT_TOP,
                text,
                FontId::proportional(13.0),
                color,
            );
        }
    }

    fn develop_stage(&mut self, ui: &mut egui::Ui) {
        let Some(photo) = self.library.as_ref().and_then(|l| l.focused().cloned()) else {
            return;
        };
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        let painter = ui.painter_at(rect);
        let rendered = self
            .rendered
            .as_ref()
            .filter(|(key, _)| *key == photo.key)
            .map(|(_, texture)| texture.clone());
        match rendered {
            Some(texture) => draw_fit(&painter, &texture, rect.shrink(8.0), Color32::WHITE),
            None => {
                if let Some(texture) = self.texture_for(&photo, true) {
                    draw_fit(
                        &painter,
                        &texture,
                        rect.shrink(8.0),
                        Color32::from_gray(150),
                    );
                }
            }
        }
        let error = self
            .develop_error
            .as_ref()
            .filter(|(key, _)| *key == photo.key)
            .map(|(_, error)| error.as_str());
        let note = match (error, self.base_loading.as_ref() == Some(&photo.key)) {
            (Some(error), _) => Some((error.to_string(), REJECT)),
            (None, true) => Some(("Reading the RAW…".to_string(), INK_2)),
            (None, false) if self.show_before => Some(("Before  \\".to_string(), INK_2)),
            _ => None,
        };
        if let Some((text, color)) = note {
            painter.text(
                rect.center_top() + Vec2::new(0.0, 12.0),
                Align2::CENTER_TOP,
                text,
                FontId::proportional(12.0),
                color,
            );
        }
        self.hud(&painter, rect, &photo);
    }

    fn filmstrip(&mut self, ui: &mut egui::Ui) {
        let Some(library) = &self.library else {
            return;
        };
        let count = library.visible.len();
        let focus = library.focus;
        let selected = library.selected.len();
        let scroll_focus = self.scroll_to_focus;
        let mut clicked = None;
        let mut opened = None;
        egui::Panel::bottom("film")
            .exact_size(136.0)
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Filmstrip").color(INK_3).size(11.0));
                    ui.label(
                        RichText::new(count.to_string())
                            .monospace()
                            .size(11.0)
                            .color(INK_2),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if selected > 1 {
                            ui.label(
                                RichText::new(format!("{selected} selected"))
                                    .monospace()
                                    .size(11.0)
                                    .color(ACCENT_2),
                            );
                        } else {
                            ui.label(RichText::new("by capture time").color(INK_3).size(11.0));
                        }
                    });
                });
                let cell = 130.0;
                let mut area = egui::ScrollArea::horizontal().auto_shrink(false);
                if scroll_focus {
                    let offset =
                        (focus as f32 * (cell + 6.0) - ui.available_width() / 2.0).max(0.0);
                    area = area.horizontal_scroll_offset(offset);
                }
                let step = cell + 6.0;
                let height = (ui.available_height() - 12.0).max(40.0);
                area.show_viewport(ui, |ui, viewport| {
                    // Only the cells on screen exist; spacers keep the scrollbar right.
                    let first = (viewport.min.x / step).max(0.0) as usize;
                    let last = ((viewport.max.x / step).ceil() as usize + 1).min(count);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.horizontal(|ui| {
                        ui.add_space(first as f32 * step);
                        for position in first..last {
                            let Some(index) = self
                                .library
                                .as_ref()
                                .and_then(|library| library.visible.get(position).copied())
                            else {
                                continue;
                            };
                            let response = self.grid_cell(ui, index, position, cell, height);
                            if response.double_clicked() {
                                opened = Some(position);
                            } else if response.clicked() {
                                clicked = Some(position);
                            }
                            ui.add_space(6.0);
                        }
                        ui.add_space(count.saturating_sub(last) as f32 * step);
                    });
                });
            });
        // The grid and clusters scroll to the focus too, and draw after this.
        self.scroll_to_focus = self.is_grid_view() && scroll_focus;
        if let Some(library) = &mut self.library {
            if let Some(position) = clicked {
                let modifiers = ui.input(|input| input.modifiers);
                library.click(position, modifiers.command, modifiers.shift);
            }
            if let Some(position) = opened {
                library.jump(position);
                self.view = View::Loupe;
            }
        }
    }

    /// The options chosen before anything is opened.
    fn import_form(&mut self, ui: &mut egui::Ui) {
        let credentials_exist = self.gdrive_available;
        let form = &mut self.form;
        ui.horizontal(|ui| {
            let label = ui.label("Import name");
            ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(220.0))
                .labelled_by(label.id);
        });
        folder_field(ui, "To", &mut form.destination);
        ui.horizontal(|ui| {
            let label = ui.label("Into subfolder");
            ui.add(egui::TextEdit::singleline(&mut form.subfolder).desired_width(220.0))
                .labelled_by(label.id);
        });
        ui.horizontal(|ui| {
            let label = ui.label("Organize");
            egui::ComboBox::from_id_salt("organize")
                .selected_text(["Into one folder", "By original folders", "By date"][form.organize])
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.organize, 0, "Into one folder");
                    ui.selectable_value(&mut form.organize, 1, "By original folders");
                    ui.selectable_value(&mut form.organize, 2, "By date");
                })
                .response
                .labelled_by(label.id);
        });
        if form.organize == 2 {
            ui.horizontal(|ui| {
                let label = ui.label("Date format");
                egui::ComboBox::from_id_salt("date_format")
                    .selected_text(form.date_format.clone())
                    .show_ui(ui, |ui| {
                        for format in [
                            DateFolders::YEAR_DATE,
                            DateFolders::YEAR_MONTH_DAY,
                            DateFolders::YEAR_MONTHDAY,
                            DateFolders::YEAR_MONTH_NAME_DAY,
                            DateFolders::DATE,
                            DateFolders::COMPACT,
                        ] {
                            ui.selectable_value(&mut form.date_format, format.to_string(), format);
                        }
                    })
                    .response
                    .labelled_by(label.id);
            });
        }
        ui.add_space(8.0);
        section(ui, "FILE RENAMING");
        ui.checkbox(&mut form.rename, "Rename files");
        if form.rename {
            ui.horizontal(|ui| {
                let label = ui.label("Template");
                ui.text_edit_singleline(&mut form.template)
                    .labelled_by(label.id);
            });
            ui.label(
                RichText::new("{name} {folder} {custom} {filenum} {seq:4} {date:%Y%m%d}")
                    .color(INK_3)
                    .small(),
            );
            ui.horizontal(|ui| {
                let label = ui.label("Custom text");
                ui.text_edit_singleline(&mut form.custom_text)
                    .labelled_by(label.id);
            });
            ui.horizontal(|ui| {
                let label = ui.label("Start number");
                ui.add(egui::DragValue::new(&mut form.start_number).range(0..=9_999_999))
                    .labelled_by(label.id);
            });
        }
        ui.horizontal(|ui| {
            ui.label("Extensions");
            for (label, case) in [
                ("As is", ExtensionCase::AsIs),
                ("lower", ExtensionCase::Lower),
                ("UPPER", ExtensionCase::Upper),
            ] {
                ui.selectable_value(&mut form.extensions, case, label);
            }
        });
        ui.add_space(8.0);
        section(ui, "FILE HANDLING");
        ui.checkbox(&mut form.only_new, "Don't copy suspected duplicates");
        folder_field(ui, "Second copy to", &mut form.second_copy);
        ui.add_enabled(
            credentials_exist || self.drive_override.is_some(),
            egui::Checkbox::new(
                &mut form.gdrive,
                format!("Back up to Google Drive ({DRIVE_ROOT_FOLDER})"),
            ),
        )
        .on_disabled_hover_text("Run `ingot gdrive-login` first.");
        if form.gdrive {
            ui.label(
                RichText::new(
                    "Uploads wait for Finalize and send only what you keep, because Ingot never deletes from Drive.",
                )
                .color(INK_3)
                .small(),
            );
        }
        if let Some(preview) = form_preview(form) {
            ui.add_space(6.0);
            ui.label(
                RichText::new(preview)
                    .monospace()
                    .size(10.5)
                    .color(ACCENT_2),
            );
        }
    }

    /// Confirms the choice and shows what will be removed. Reads nothing from disk.
    fn finalize_window(&mut self, ctx: &egui::Context) {
        let Some(mut keep_unflagged) = self.finalize.as_ref().map(|d| d.keep_unflagged) else {
            return;
        };
        let Some(library) = &self.library else {
            return;
        };
        let keep = library.files_to_keep(keep_unflagged);
        let plan = plan_removal(&library.replicas, &keep);
        let removals: Vec<String> = plan
            .paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let already_there = plan.already_there.len();
        let counts = library.import_counts();
        let kept_photos = library
            .photos
            .iter()
            .filter(|photo| photo.files.iter().any(|file| keep.contains(file)))
            .count();
        let unread = library
            .photos
            .iter()
            .flat_map(|photo| photo.files.iter())
            .filter(|file| !keep.contains(*file) && !library.is_copied(file))
            .count();
        let drive = self
            .session
            .as_ref()
            .is_some_and(|session| session.drive.is_some());
        let mut close = false;
        let mut confirm = false;
        egui::Modal::new(egui::Id::unique("finalize")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(RichText::new("Finalize import").size(17.0).strong().color(INK));
            ui.label(
                RichText::new("Keep your picks, take back the rest, and finish the copy.")
                    .color(INK_3)
                    .size(12.0),
            );
            ui.add_space(10.0);
            big_stats(
                ui,
                &[
                    (kept_photos.to_string(), "photos kept", INK),
                    (removals.len().to_string(), "copies to Trash", REJECT),
                    (unread.to_string(), "copies stopped", INK),
                ],
            );
            ui.add_space(8.0);
            ui.checkbox(&mut keep_unflagged, "Also keep unflagged photos");
            if kept_photos == 0 {
                ui.label(
                    RichText::new(
                        "You have no picks, so no photos would be kept. Pick some first, or keep the unflagged ones.",
                    )
                    .color(REJECT),
                );
            }
            ui.add_space(6.0);
            if removals.is_empty() {
                ui.label(RichText::new("No copies need to be removed.").color(INK_2));
            } else {
                ui.label(RichText::new("These go to the Trash:").color(INK_2));
                egui::ScrollArea::vertical()
                    .max_height(170.0)
                    .show(ui, |ui| {
                        for path in &removals {
                            ui.label(RichText::new(path).monospace().size(10.5).color(INK_2));
                        }
                    });
            }
            ui.add_space(6.0);
            let mut note = "Only copies this import wrote can go, and only if unchanged. Source files are never touched.".to_string();
            if already_there > 0 {
                note.push_str(&format!(
                    " {already_there} files that were already at the destination stay."
                ));
            }
            if drive {
                note.push_str(" Kept photos then upload to Google Drive.");
            }
            ui.label(RichText::new(note).color(INK_3).small());
            let _ = counts;
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if button(ui, "Keep choosing", ButtonKind::Normal).clicked() {
                    close = true;
                }
                if ui
                    .add_enabled_ui(kept_photos > 0 || removals.is_empty(), |ui| {
                        button(ui, "Finalize", ButtonKind::Primary)
                    })
                    .inner
                    .clicked()
                {
                    confirm = true;
                }
            });
        });
        if confirm {
            self.finalize = None;
            self.confirm_finalize(keep_unflagged);
        } else if close {
            self.finalize = None;
        } else if let Some(dialog) = &mut self.finalize {
            dialog.keep_unflagged = keep_unflagged;
        }
    }

    fn finalize_result_window(&mut self, ctx: &egui::Context) {
        let Some(result) = &self.finalize_result else {
            return;
        };
        let mut close = false;
        egui::Modal::new(egui::Id::unique("finalized")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(
                RichText::new("Import finalized")
                    .size(17.0)
                    .strong()
                    .color(INK),
            );
            ui.add_space(10.0);
            big_stats(
                ui,
                &[
                    (result.kept_photos.to_string(), "photos kept", ACCEPT),
                    (result.trashed.len().to_string(), "copies in Trash", INK),
                    (result.uploaded.to_string(), "uploaded", INK),
                ],
            );
            if result.already_there > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} files that were already there stayed.",
                        result.already_there
                    ))
                    .color(INK_2),
                );
            }
            if result.sidecars_trashed > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} sidecars went with their copies.",
                        result.sidecars_trashed
                    ))
                    .color(INK_3)
                    .small(),
                );
            }
            if !result.problems.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("{} problems:", result.problems.len())).color(REJECT),
                );
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .show(ui, |ui| {
                        for problem in &result.problems {
                            ui.label(RichText::new(problem).color(INK_2).small());
                        }
                    });
            }
            ui.add_space(10.0);
            if button(ui, "Done", ButtonKind::Primary).clicked() {
                close = true;
            }
        });
        if close {
            self.finalize_result = None;
        }
    }

    fn save_window(&mut self, ctx: &egui::Context) {
        if !self.save_open {
            return;
        }
        let counts = self
            .library
            .as_ref()
            .map(Library::import_counts)
            .unwrap_or_default();
        let targets = self.session.as_ref().map_or(0, |s| s.local_targets);
        let mut close = false;
        let mut confirm = false;
        egui::Modal::new(egui::Id::unique("save")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new("Save metadata").size(17.0).strong().color(INK));
            ui.label(
                RichText::new("Ratings and flags to XMP sidecars")
                    .color(INK_3)
                    .size(12.0),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Writes a .xmp sidecar next to each copy, on every local destination. Ingot also does this as you cull; this writes them all again. Source files and photo files are never changed.",
                )
                .color(INK_2),
            );
            ui.add_space(8.0);
            big_stats(
                ui,
                &[
                    (counts.rated.to_string(), "ratings", INK),
                    ((counts.picks + counts.rejects).to_string(), "flags", INK),
                    (targets.to_string(), "targets", INK),
                ],
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if button(ui, "Cancel", ButtonKind::Normal).clicked() {
                    close = true;
                }
                if button(ui, "Write metadata", ButtonKind::Primary).clicked() {
                    confirm = true;
                }
            });
        });
        if confirm {
            if let Some(library) = &mut self.library {
                library.save_all_sidecars();
            }
            self.toast = Some((
                format!("Metadata written: {} ratings", counts.rated),
                Instant::now(),
            ));
            close = true;
        }
        if close {
            self.save_open = false;
        }
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        let Some((message, since)) = &self.toast else {
            return;
        };
        if since.elapsed() > Duration::from_millis(2600) {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(200));
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -70.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(PANEL_3)
                    .stroke(Stroke::new(1.0, LINE_2))
                    .corner_radius(CornerRadius::same(5))
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.label(RichText::new(message).color(INK));
                    });
            });
    }

    fn sweep_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.sweep else {
            return;
        };
        let mut close = false;
        let mut confirm = false;
        egui::Window::new("Delete rejected photos")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                if dialog.planning {
                    ui.spinner();
                    ui.label(RichText::new("Checking the copies…").color(INK_2));
                    return;
                }
                big_stats(
                    ui,
                    &[(dialog.paths.len().to_string(), "copies to Trash", REJECT)],
                );
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for path in &dialog.paths {
                            ui.label(
                                RichText::new(path.display().to_string())
                                    .color(INK_2)
                                    .small(),
                            );
                        }
                    });
                ui.add_space(6.0);
                ui.label(
                    RichText::new("The card and other source files are not touched.")
                        .color(INK_3)
                        .small(),
                );
                if dialog.left_in_cloud > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} copies on Google Drive stay. Ingot never deletes from Drive.",
                            dialog.left_in_cloud
                        ))
                        .color(INK_3)
                        .small(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui
                        .add_enabled(
                            !dialog.paths.is_empty() && !dialog.running,
                            egui::Button::new(RichText::new("Move to Trash").color(Color32::WHITE))
                                .fill(REJECT),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if confirm {
            self.confirm_sweep();
        }
        if close {
            self.sweep = None;
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("keys").num_columns(2).show(ui, |ui| {
                    for (keys, what) in [
                        ("0 – 5", "Rating"),
                        ("P or Space / X / U", "Pick / reject / clear flag"),
                        ("Shift + any of those", "Also move to the next photo"),
                        ("A", "Move on after a flag, on or off"),
                        ("Arrow keys", "Move"),
                        ("Home / End", "First / last photo"),
                        ("G", "Grid"),
                        ("E or Enter", "Loupe"),
                        ("D", "Develop"),
                        ("L", "Clusters"),
                        ("N", "Compare the selection"),
                        ("Cmd-click, Shift-click", "Select several"),
                        ("Cmd+A", "Select all"),
                        ("Z", "Zoom to 1:1"),
                        ("\\", "Before and after"),
                        ("Cmd+Shift+C / V", "Copy / paste develop settings"),
                        ("Cmd+Shift+I", "New import"),
                        ("Tab", "Hide panels"),
                        ("Esc", "Back"),
                        ("/", "This list"),
                    ] {
                        ui.label(RichText::new(keys).color(INK));
                        ui.label(RichText::new(what).color(INK_2));
                        ui.end_row();
                    }
                });
            });
        if !open {
            self.show_help = false;
        }
    }
}

impl eframe::App for IngotApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// Watches for cards in the background: a stat on a slow or sleeping volume must not
/// hold up a frame. Reports only changes.
fn spawn_environment_thread(gdrive: PathBuf, tx: mpsc::Sender<Msg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            let now = (find_cards(Path::new("/Volumes")), gdrive.exists());
            if last.as_ref() != Some(&now) {
                let message = Msg::Environment {
                    cards: now.0.clone(),
                    gdrive: now.1,
                };
                if tx.send(message).is_err() {
                    return;
                }
                ctx.request_repaint();
                last = Some(now);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Fingerprints images already in memory, so clustering never adds a card read.
fn spawn_hash_thread(
    tx: mpsc::Sender<Msg>,
    ctx: egui::Context,
) -> mpsc::Sender<(AssetKey, Vec<u8>, bool)> {
    let (jobs, rx) = mpsc::channel::<(AssetKey, Vec<u8>, bool)>();
    std::thread::spawn(move || {
        for (key, bytes, from_thumb) in rx {
            let Ok(image) = preview::decompress(&bytes, 64) else {
                continue;
            };
            let histogram = luma_histogram(&image.pixels);
            let Some(print) = Fingerprint::from_rgb(&image.pixels, image.width, image.height)
            else {
                continue;
            };
            if tx
                .send(Msg::Hashed(key, print, histogram, from_thumb))
                .is_err()
            {
                return;
            }
            ctx.request_repaint();
        }
    });
    jobs
}

const HISTOGRAM_BINS: usize = 48;

fn luma_histogram(rgb: &[u8]) -> Vec<u32> {
    let mut bins = vec![0u32; HISTOGRAM_BINS];
    for pixel in rgb.chunks_exact(3) {
        let luma =
            (u32::from(pixel[0]) * 54 + u32::from(pixel[1]) * 183 + u32::from(pixel[2]) * 19) >> 8;
        bins[luma as usize * HISTOGRAM_BINS / 256] += 1;
    }
    bins
}

/// As the design names imports: `2026-05-12 · 14:02`.
fn default_import_name() -> String {
    chrono::Local::now().format("%Y-%m-%d · %H:%M").to_string()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

/// A short form of where the files go: `~/Photos/2026/IMG_1.CR2 … (879 files)`. Long
/// paths lose their middle, since the start and the end are what you recognise.
fn destination_summary(example: &Path, files: usize, home: Option<&Path>) -> String {
    const LONGEST: usize = 64;
    let mut path = match home.and_then(|home| example.strip_prefix(home).ok()) {
        Some(inside) => format!("~/{}", inside.display()),
        None => example.display().to_string(),
    };
    if path.chars().count() > LONGEST {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() > 4 {
            path = format!(
                "{}/{}/…/{}/{}",
                parts[0],
                parts[1],
                parts[parts.len() - 2],
                parts[parts.len() - 1]
            );
        }
    }
    match files {
        0 | 1 => path,
        _ => format!("{path} … ({files} files)"),
    }
}

/// Folders under `volumes` that look like camera cards.
fn find_cards(volumes: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(volumes) else {
        return vec![];
    };
    let mut cards: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("DCIM"))
        .filter(|path| path.is_dir())
        .collect();
    cards.sort();
    cards
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL_2;
    visuals.extreme_bg_color = BG;
    visuals.window_stroke = Stroke::new(1.0, LINE);
    visuals.selection.bg_fill = ACCENT;
    visuals.widgets.inactive.weak_bg_fill = PANEL_3;
    visuals.widgets.hovered.weak_bg_fill = LINE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).color(INK_3).size(10.0).strong());
    ui.add_space(2.0);
}

fn slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized(
            [66.0, 16.0],
            egui::Label::new(RichText::new(label).color(INK_2).small()),
        );
        let response = ui.add(
            egui::Slider::new(value, range)
                .max_decimals(2)
                .trailing_fill(true),
        );
        if response.double_clicked() {
            *value = default;
            return true;
        }
        response.changed()
    })
    .inner
}

fn folder_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        let label = ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(180.0))
            .labelled_by(label.id);
        if ui.button("Choose…").clicked()
            && let Some(folder) = rfd::FileDialog::new().pick_folder()
        {
            *value = folder.display().to_string();
        }
    });
}

/// Shrinks an image that the graphics card can't hold as one texture, by the smallest
/// whole factor that fits. Most cards take 16384 px, so a photo normally stays 1:1.
fn fit_texture(image: egui::ColorImage, max_side: usize) -> egui::ColorImage {
    let [width, height] = image.size;
    let factor = width.max(height).div_ceil(max_side.max(1));
    if factor <= 1 {
        return image;
    }
    let (new_w, new_h) = (width / factor, height / factor);
    let mut pixels = Vec::with_capacity(new_w * new_h * 3);
    for y in 0..new_h {
        for x in 0..new_w {
            let pixel = image.pixels[(y * factor) * width + x * factor];
            pixels.extend([pixel.r(), pixel.g(), pixel.b()]);
        }
    }
    egui::ColorImage::from_rgb([new_w, new_h], &pixels)
}

/// A five-pointed star built from five convex kites, since the default font has no
/// star glyph.
fn paint_star(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let point = |angle_deg: f32, length: f32| {
        let angle = angle_deg.to_radians();
        center + Vec2::new(angle.cos(), angle.sin()) * length
    };
    let inner = radius * 0.42;
    for tip in 0..5 {
        let angle = -90.0 + tip as f32 * 72.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                center,
                point(angle - 36.0, inner),
                point(angle, radius),
                point(angle + 36.0, inner),
            ],
            color,
            Stroke::NONE,
        ));
    }
}

#[derive(Clone, Copy)]
enum ButtonKind {
    Primary,
    Normal,
    Danger,
}

fn button(ui: &mut egui::Ui, text: &str, kind: ButtonKind) -> egui::Response {
    let (fill, ink, line) = match kind {
        ButtonKind::Primary => (ACCENT, Color32::from_rgb(0x06, 0x12, 0x2b), ACCENT),
        ButtonKind::Normal => (PANEL_2, INK, LINE),
        ButtonKind::Danger => (
            Color32::from_rgb(0x20, 0x15, 0x1a),
            REJECT,
            Color32::from_rgb(0x4a, 0x20, 0x26),
        ),
    };
    ui.add(
        egui::Button::new(RichText::new(text).color(ink).size(12.0).strong())
            .fill(fill)
            .stroke(Stroke::new(1.0, line))
            .corner_radius(CornerRadius::same(5)),
    )
}

fn psec_h(ui: &mut egui::Ui, title: &str) {
    ui.horizontal(|ui| {
        ui.add_space(13.0);
        ui.label(RichText::new(title).color(INK_3).size(10.5).strong());
    });
}

/// A row in the left panel: optional dot, label, count on the right.
fn panel_row(
    ui: &mut egui::Ui,
    on: bool,
    label: &str,
    dot: Option<Color32>,
    count: String,
) -> egui::Response {
    // Allocated rather than given an id from the label: two imports can share a name.
    let width = ui.available_width() - 18.0;
    let (outer, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 27.0), Sense::click());
    let rect = Rect::from_center_size(outer.center(), Vec2::new(width, 26.0));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let painter = ui.painter();
    if on {
        painter.rect_filled(rect, CornerRadius::same(5), ACCENT_DIM);
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(5), PANEL_2);
    }
    let mut x = rect.min.x + 9.0;
    if let Some(color) = dot {
        painter.circle_filled(Pos2::new(x + 4.0, rect.center().y), 4.5, color);
        x += 18.0;
    }
    painter.text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.5),
        if on { INK } else { INK_2 },
    );
    painter.text(
        Pos2::new(rect.max.x - 9.0, rect.center().y),
        Align2::RIGHT_CENTER,
        count,
        FontId::monospace(11.0),
        INK_3,
    );
    response
}

/// The design's clickable star threshold row. Returns the star clicked.
fn star_filter_row(ui: &mut egui::Ui, active: u8, label: &str) -> Option<u8> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.add_space(18.0);
        clicked = star_row(ui, active, 14.0);
        ui.label(RichText::new(label).color(INK_3).size(12.0));
    });
    clicked
}

/// Five clickable stars, lit up to `value`. Returns the star clicked.
fn star_row(ui: &mut egui::Ui, value: u8, size: f32) -> Option<u8> {
    let mut clicked = None;
    ui.spacing_mut().item_spacing.x = 2.0;
    for star in 1..=5u8 {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        let response = response.on_hover_text(star.to_string());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{star} stars"))
        });
        let color = if star <= value { STAR } else { STAR_OFF };
        paint_star(ui.painter(), rect.center(), size * 0.45, color);
        if response.clicked() {
            clicked = Some(star);
        }
    }
    clicked
}

fn paint_inline_star(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
    paint_star(ui.painter(), rect.center(), 5.5, INK_3);
}

fn paint_inline_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn flag_button(ui: &mut egui::Ui, label: &str, on: bool, color: Color32) -> bool {
    let (fill, line, ink) = if !on {
        (PANEL_2, LINE, INK_2)
    } else if color == ACCEPT {
        (
            Color32::from_rgb(0x0f, 0x2c, 0x22),
            Color32::from_rgb(0x1f, 0x6b, 0x4e),
            ACCEPT,
        )
    } else {
        (
            Color32::from_rgb(0x2a, 0x14, 0x19),
            Color32::from_rgb(0x6b, 0x25, 0x33),
            REJECT,
        )
    };
    ui.add_sized(
        [ui.available_width(), 34.0],
        egui::Button::new(RichText::new(label).color(ink).size(11.5).strong())
            .fill(fill)
            .stroke(Stroke::new(1.0, line)),
    )
    .clicked()
}

fn pair_tag(ui: &mut egui::Ui, extension: &str, size: u64) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, LINE_2))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{extension} · {}", format_size(size)))
                    .monospace()
                    .size(10.0)
                    .color(INK_2),
            );
        });
}

fn paint_flag_chip(painter: &egui::Painter, rect: Rect, accept: bool) {
    let (fill, ink) = if accept {
        (ACCEPT, Color32::from_rgb(0x06, 0x25, 0x1a))
    } else {
        (REJECT, Color32::from_rgb(0x2a, 0x0a, 0x0f))
    };
    painter.rect_filled(rect, CornerRadius::same(5), fill);
    let stroke = Stroke::new(2.0, ink);
    let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * rect.width();
    if accept {
        painter.line_segment([at(0.24, 0.52), at(0.42, 0.72)], stroke);
        painter.line_segment([at(0.42, 0.72), at(0.76, 0.30)], stroke);
    } else {
        painter.line_segment([at(0.28, 0.28), at(0.72, 0.72)], stroke);
        painter.line_segment([at(0.72, 0.28), at(0.28, 0.72)], stroke);
    }
}

fn paint_histogram(painter: &egui::Painter, rect: Rect, bins: &[u32]) {
    let peak = bins.iter().copied().max().unwrap_or(0).max(1) as f32;
    let step = rect.width() / bins.len().max(1) as f32;
    for (index, value) in bins.iter().enumerate() {
        let height = rect.height() * (*value as f32 / peak).sqrt();
        let x = rect.min.x + index as f32 * step;
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x, rect.max.y - height),
                Pos2::new(x + step, rect.max.y),
            ),
            CornerRadius::ZERO,
            Color32::from_white_alpha(36),
        );
    }
}

fn big_stats(ui: &mut egui::Ui, stats: &[(String, &str, Color32)]) {
    ui.columns(stats.len().max(1), |columns| {
        for (column, (value, label, color)) in columns.iter_mut().zip(stats) {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(8)
                .show(column, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(value).size(20.0).strong().color(*color));
                    ui.label(RichText::new(*label).color(INK_3).size(11.0));
                });
        }
    });
}

fn source_row(ui: &mut egui::Ui, source: &Path) -> egui::Response {
    let name = source
        .parent()
        .and_then(Path::file_name)
        .or_else(|| source.file_name())
        .map_or_else(
            || source.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    ui.add_sized(
        [ui.available_width(), 40.0],
        egui::Button::new(RichText::new(format!("{name}\n{}", source.display())).size(11.5))
            .fill(PANEL_2)
            .stroke(Stroke::new(1.0, LINE)),
    )
}

/// What a sample photo's path would be, as the design shows under the hierarchy.
fn form_preview(form: &ImportForm) -> Option<String> {
    let options = form.options().ok()?;
    let relative = planned_relative(
        Path::new("IMG_0421.CR2"),
        Path::new(""),
        &options,
        options.start_number,
        chrono::Local::now().naive_local(),
    )
    .ok()?;
    let root = form.destination.trim();
    let root = if root.is_empty() { "…" } else { root };
    let home = dirs::home_dir();
    Some(destination_summary(
        &Path::new(root).join(relative),
        0,
        home.as_deref(),
    ))
}

/// Draws a texture as large as fits, centered, keeping its shape.
fn draw_fit(painter: &egui::Painter, texture: &TextureHandle, area: Rect, tint: Color32) {
    let size = texture.size_vec2();
    let scale = (area.width() / size.x).min(area.height() / size.y);
    painter.image(
        texture.id(),
        Rect::from_center_size(area.center(), size * scale),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        tint,
    );
}

fn spawn_render_thread(slot: Arc<RenderSlot>, tx: mpsc::Sender<Msg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        loop {
            let (key, base, params) = {
                let mut job = slot.job.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(next) = job.take() {
                        break next;
                    }
                    job = slot.wake.wait(job).unwrap_or_else(|e| e.into_inner());
                }
            };
            let result = base.render(&params).map_err(io_err);
            if tx.send(Msg::Rendered(key, result)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use egui_kittest::{Harness, kittest::Queryable};
    use tempfile::TempDir;

    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/ingot_core/testdata/test_exif_read")
    }

    fn paths(tmp: &TempDir) -> AppPaths {
        AppPaths {
            triage: tmp.path().join("triage.jsonl"),
            history: tmp.path().join("history.jsonl"),
            imports: tmp.path().join("imports.json"),
            gdrive: tmp.path().join("missing-gdrive.json"),
        }
    }

    /// Runs frames until `done`, since background threads need real time.
    fn wait_until(
        harness: &mut Harness<'_, IngotApp>,
        what: &str,
        done: impl Fn(&IngotApp) -> bool,
    ) {
        for _ in 0..600 {
            harness.step();
            if done(harness.state()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    fn harness(tmp: &TempDir, folder: Option<PathBuf>) -> Harness<'static, IngotApp> {
        let ctx = egui::Context::default();
        let app = IngotApp::new(&ctx, paths(tmp), folder);
        Harness::builder()
            .with_size(Vec2::new(1280.0, 800.0))
            .build_ui_state(|ui, app: &mut IngotApp| app.show(ui), app)
    }

    /// Fake removal: the real one fills the user's Trash. A real removal is covered by
    /// the ignored test in `sweep.rs`.
    fn delete_for_test(plan: &RemovalPlan<'_>) -> SweepReport {
        let mut report = SweepReport::default();
        for path in plan.paths() {
            std::fs::remove_file(path).unwrap();
            report.trashed.push(path.to_path_buf());
        }
        report
    }

    /// A harness with the destination chosen and the source picked: the import runs.
    fn imported(tmp: &TempDir, dest: &Path) -> Harness<'static, IngotApp> {
        let mut harness = harness(tmp, None);
        harness.state_mut().remover = delete_for_test;
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "the import to start", |app| {
            app.session.is_some()
        });
        harness
    }

    fn all_copied(app: &IngotApp) -> bool {
        app.library
            .as_ref()
            .is_some_and(|library| library.import_counts().pending() == 0)
    }

    fn phase(harness: &Harness<'static, IngotApp>) -> Phase {
        harness.state().session.as_ref().unwrap().phase
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.ends_with(".tmp") && !name.ends_with(".xmp"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn focus_on(harness: &mut Harness<'static, IngotApp>, name: &str) {
        let library = harness.state_mut().library.as_mut().unwrap();
        let position = library
            .visible
            .iter()
            .position(|index| library.photos[*index].name == name)
            .unwrap();
        library.jump(position);
        library.selected.clear();
    }

    fn pick(harness: &mut Harness<'static, IngotApp>, name: &str) {
        focus_on(harness, name);
        harness.state_mut().apply_flag(TriageState::Accepted, false);
    }

    fn finalize(harness: &mut Harness<'static, IngotApp>, keep_unflagged: bool) {
        wait_until(harness, "the copy", all_copied);
        harness.state_mut().confirm_finalize(keep_unflagged);
        wait_until(harness, "finalize", |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.phase == Phase::Finalized)
        });
    }

    #[test]
    fn the_first_screen_asks_for_a_destination_then_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, None);
        harness.step();
        harness.step();
        harness.get_by_label_contains("Choose where the photos go");
        harness.get_by_label("Choose a destination first.");
        assert!(!harness.state().form.name.is_empty());
    }

    #[test]
    fn choosing_a_source_without_a_destination_starts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, None);
        harness.state_mut().choose_source(fixture());
        assert!(harness.state().form.error.is_some());
        for _ in 0..20 {
            harness.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(harness.state().library.is_none());
        assert!(harness.state().session.is_none());
    }

    #[test]
    fn a_folder_from_the_command_line_waits_for_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, Some(fixture()));
        for _ in 0..10 {
            harness.step();
        }
        assert!(harness.state().session.is_none());
        assert_eq!(harness.state().suggested_source, Some(fixture()));
    }

    #[test]
    fn choosing_the_source_starts_copying_everything_and_opens_the_grid() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        assert_eq!(harness.state().view, View::Grid);
        assert_eq!(phase(&harness), Phase::Copying);
        wait_until(&mut harness, "the copy", all_copied);
        assert_eq!(files_in(&dest).len(), 4);
        assert_eq!(harness.state().copy.failed, 0);
        // The session waits for Finalize, and the import is on the list.
        assert_eq!(phase(&harness), Phase::Copying);
        assert_eq!(harness.state().imports.entries.len(), 1);
    }

    #[test]
    fn finalize_keeps_picks_and_takes_back_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        pick(&mut harness, "IMG_1800.CR2");
        finalize(&mut harness, false);

        assert_eq!(files_in(&dest), ["IMG_1800.CR2", "IMG_1800.JPG"]);
        let result = harness.state().finalize_result.clone().unwrap();
        assert_eq!(result.kept_photos, 1);
        assert_eq!(result.trashed.len(), 2);
        assert!(result.problems.is_empty(), "{:?}", result.problems);
        assert!(harness.state().imports.entries[0].finalized);
        assert_eq!(harness.state().imports.entries[0].photos, 1);
        // The source is untouched.
        assert_eq!(files_in(&fixture()).len(), 4);
    }

    #[test]
    fn finalize_can_keep_unflagged_photos() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        focus_on(&mut harness, "IMG_1868.JPG");
        harness.state_mut().apply_flag(TriageState::Rejected, false);
        // A rejected photo may already be copied; Finalize takes it back either way.
        harness.state_mut().confirm_finalize(true);
        wait_until(&mut harness, "finalize", |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.phase == Phase::Finalized)
        });
        assert_eq!(
            files_in(&dest),
            ["IMG_1800.CR2", "IMG_1800.JPG", "IMG_1939.CR2"]
        );
    }

    #[test]
    fn finalize_never_takes_a_file_that_was_already_there() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::copy(fixture().join("IMG_1868.JPG"), dest.join("IMG_1868.JPG")).unwrap();
        let mut harness = imported(&tmp, &dest);
        pick(&mut harness, "IMG_1800.CR2");
        finalize(&mut harness, false);

        assert!(dest.join("IMG_1868.JPG").exists());
        assert!(!dest.join("IMG_1939.CR2").exists());
        assert_eq!(
            harness
                .state()
                .finalize_result
                .as_ref()
                .unwrap()
                .already_there,
            1
        );
    }

    #[test]
    fn a_rejected_photo_is_held_back_and_returns_when_unflagged() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = harness(&tmp, None);
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "start", |app| app.session.is_some());
        harness.state_mut().library.as_mut().unwrap().jump(2);
        harness.state_mut().apply_flag(TriageState::Rejected, false);
        let held_or_done = {
            let counts = harness.state().library.as_ref().unwrap().import_counts();
            counts.rejects == 1 && counts.pending() <= 3
        };
        assert!(held_or_done);
        // Clearing the flag brings it back, so everything ends up copied.
        harness.state_mut().apply_flag(TriageState::Rejected, false);
        wait_until(&mut harness, "the copy", all_copied);
        wait_until(&mut harness, "all files", |_| files_in(&dest).len() == 4);
    }

    #[test]
    fn drive_gets_nothing_until_finalize_and_then_only_what_was_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let drive_root = tmp.path().join("drive");
        let mut harness = harness(&tmp, None);
        harness.state_mut().remover = delete_for_test;
        harness.state_mut().drive_override = Some(Arc::new(LocalTarget::new(
            TargetId(DRIVE_TARGET_ID),
            Target {
                root: drive_root.clone(),
                kind: TargetKind::Network,
                write_permits: 2,
            },
        )));
        harness.state_mut().form.gdrive = true;
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "start", |app| app.session.is_some());

        wait_until(&mut harness, "the copy", all_copied);
        assert_eq!(files_in(&dest).len(), 4);
        assert!(files_in(&drive_root).is_empty());

        pick(&mut harness, "IMG_1939.CR2");
        finalize(&mut harness, false);
        assert_eq!(files_in(&drive_root), ["IMG_1939.CR2"]);
        assert_eq!(files_in(&dest), ["IMG_1939.CR2"]);
        assert_eq!(
            harness.state().finalize_result.as_ref().unwrap().uploaded,
            1
        );
    }

    #[test]
    fn a_second_copy_follows_the_same_removal_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, second) = (tmp.path().join("photos"), tmp.path().join("backup"));
        let mut harness = harness(&tmp, None);
        harness.state_mut().remover = delete_for_test;
        harness.state_mut().form.second_copy = second.display().to_string();
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "start", |app| app.session.is_some());
        pick(&mut harness, "IMG_1939.CR2");
        finalize(&mut harness, false);
        assert_eq!(files_in(&dest), ["IMG_1939.CR2"]);
        assert_eq!(files_in(&second), ["IMG_1939.CR2"]);
    }

    #[test]
    fn the_finalize_dialog_shows_what_will_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        wait_until(&mut harness, "the copy", all_copied);
        pick(&mut harness, "IMG_1800.CR2");
        harness.state_mut().finalize = Some(FinalizeDialog {
            keep_unflagged: false,
        });
        for _ in 0..5 {
            harness.step();
        }
        assert!(harness.get_all_by_label("Finalize import").count() >= 1);
        harness.get_by_label("These go to the Trash:");
        harness.get_by_label_contains("IMG_1939.CR2");
        assert!(dest.join("IMG_1939.CR2").exists());
        // Nothing has happened yet.
        assert_eq!(phase(&harness), Phase::Copying);
    }

    #[test]
    fn only_kept_photos_count_as_imported_next_time() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        pick(&mut harness, "IMG_1800.CR2");
        finalize(&mut harness, false);
        harness.state_mut().finalize_result = None;

        harness.state_mut().form.destination = tmp.path().join("again").display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "the second import", |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.phase == Phase::Copying)
        });
        let library = harness.state().library.as_ref().unwrap();
        assert_eq!(library.already_imported.len(), 1);
    }

    #[test]
    fn a_new_import_is_refused_while_files_are_still_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        wait_until(&mut harness, "the copy", all_copied);
        // Asking for a target no file has reached leaves every file on its way.
        harness.state_mut().library.as_mut().unwrap().target_count = 99;
        assert!(!harness.state_mut().leave_session());
        assert!(harness.state().session.is_some());

        harness.state_mut().library.as_mut().unwrap().target_count = 1;
        harness.state_mut().session.as_mut().unwrap().phase = Phase::Finalizing;
        assert!(!harness.state_mut().leave_session());
        harness.state_mut().session.as_mut().unwrap().phase = Phase::Copying;
        assert!(harness.state_mut().leave_session());
    }

    #[test]
    fn imports_are_named_and_renamed_and_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = harness(&tmp, None);
        harness.state_mut().form.name = "Wedding".to_string();
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "start", |app| app.session.is_some());
        harness.step();
        assert!(harness.get_all_by_label("Wedding").count() >= 1);
        assert_eq!(harness.state().session.as_ref().unwrap().name, "Wedding");

        let id = harness.state().session.as_ref().unwrap().id;
        harness
            .state_mut()
            .imports
            .rename(id, "Anna and Tom")
            .unwrap();
        let log = ImportLog::open(paths(&tmp).imports);
        assert_eq!(log.entries[0].name, "Anna and Tom");
    }

    #[test]
    fn culling_works_while_previews_arrive_and_develop_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);

        // Rate before previews or copies have finished.
        harness.key_press(Key::Num4);
        harness.step();
        harness.key_press(Key::X);
        harness.step();
        {
            let library = harness.state().library.as_ref().unwrap();
            assert_eq!(library.import_counts().rejects, 1);
            let first = library.photos[library.visible[0]].key.clone();
            assert_eq!(library.decisions(&first).0, 4);
            // Rejecting moved on to the next photo.
            assert_eq!(library.focus, 1);
        }

        wait_until(&mut harness, "previews", |app| app.previews_done);

        harness.key_press(Key::E);
        harness.step();
        assert_eq!(harness.state().view, View::Loupe);
        harness.key_press(Key::Z);
        wait_until(&mut harness, "1:1 image", |app| app.full.is_some());
        harness.key_press(Key::Escape);
        harness.step();

        let raw_position = {
            let library = harness.state().library.as_ref().unwrap();
            library
                .visible
                .iter()
                .position(|i| library.photos[*i].raw.is_some())
                .unwrap()
        };
        harness
            .state_mut()
            .library
            .as_mut()
            .unwrap()
            .jump(raw_position);
        harness.key_press(Key::D);
        wait_until(&mut harness, "develop render", |app| app.rendered.is_some());
        harness.get_by_label("Exposure");

        harness.state_mut().params.exposure = 1.0;
        harness.state_mut().commit_params();
        harness.step();
        harness.step();
        let photo = {
            let library = harness.state().library.as_ref().unwrap();
            library.focused().unwrap().clone()
        };
        let saved = harness
            .state()
            .library
            .as_ref()
            .unwrap()
            .develop_params(&photo);
        assert_eq!(saved.exposure, 1.0);
    }

    #[test]
    fn compare_opens_on_the_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        {
            let library = harness.state_mut().library.as_mut().unwrap();
            library.click(0, false, false);
            library.click(1, true, false);
        }
        harness.key_press(Key::N);
        harness.step();
        assert_eq!(harness.state().view, View::Survey);
        assert_eq!(harness.state().survey, [0, 1]);
        harness.step();
    }

    #[test]
    fn clusters_regroup_on_the_slider_and_keys_still_cull() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        wait_until(&mut harness, "every fingerprint", |app| {
            app.library.as_ref().unwrap().fingerprint_count() == 3
        });

        harness.key_press(Key::L);
        harness.step();
        assert_eq!(harness.state().view, View::Clusters);
        harness.get_by_label("Sensitivity");

        let groups_at = |harness: &mut Harness<'static, IngotApp>, sensitivity: f32| {
            let library = harness.state_mut().library.as_mut().unwrap();
            library.set_sensitivity(sensitivity);
            library.clusters().len()
        };
        let low = groups_at(&mut harness, 0.0);
        let high = groups_at(&mut harness, 100.0);
        assert!(low <= high, "{low} {high}");
        assert_eq!(high, 3);
        assert_eq!(
            harness
                .state()
                .library
                .as_ref()
                .unwrap()
                .fingerprint_count(),
            3
        );
        harness.step();

        harness.key_press(Key::Num3);
        harness.step();
        let library = harness.state().library.as_ref().unwrap();
        let key = library.focused().unwrap().key.clone();
        assert_eq!(library.decisions(&key).0, 3);
    }

    #[test]
    fn ratings_reach_a_sidecar_next_to_the_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = imported(&tmp, &dest);
        wait_until(&mut harness, "the copy", all_copied);

        harness.state_mut().library.as_mut().unwrap().jump(0);
        harness.state_mut().library.as_mut().unwrap().rate(5, false);
        // Written off the UI thread.
        wait_until(&mut harness, "sidecar", |_| {
            std::fs::read_dir(&dest)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|x| x == "xmp"))
        });

        // Nothing is rejected, so the sweep offers nothing.
        harness.state_mut().open_sweep();
        wait_until(&mut harness, "sweep plan", |app| {
            app.sweep.as_ref().is_some_and(|sweep| !sweep.planning)
        });
        assert!(harness.state().sweep.as_ref().unwrap().paths.is_empty());
    }

    #[test]
    fn destination_summary_abbreviates_home_and_long_paths() {
        let home = Path::new("/Users/anna");
        let path = Path::new("/Users/anna/Photos/Wedding/2026/2026-05-22/IMG_0001.CR2");
        assert_eq!(
            destination_summary(path, 879, Some(home)),
            "~/Photos/Wedding/2026/2026-05-22/IMG_0001.CR2 … (879 files)"
        );
        assert_eq!(
            destination_summary(path, 1, Some(home)),
            "~/Photos/Wedding/2026/2026-05-22/IMG_0001.CR2"
        );
        let long = Path::new(
            "/Volumes/Backup/Clients/2026/A very long client name/Wedding/Second day/IMG_0001.CR2",
        );
        assert_eq!(
            destination_summary(long, 2, None),
            "/Volumes/…/Second day/IMG_0001.CR2 … (2 files)"
        );
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(format_size(500_000_000), "500 MB");
        assert_eq!(format_size(30_500_000_000), "30.5 GB");
    }

    #[test]
    fn oversized_images_are_shrunk_to_fit_the_graphics_card() {
        let image = egui::ColorImage::from_rgb([100, 40], &vec![9u8; 100 * 40 * 3]);
        assert_eq!(fit_texture(image.clone(), 200).size, [100, 40]);
        assert_eq!(fit_texture(image, 30).size, [25, 10]);
    }

    /// Renders each view to PNGs for a person to look at:
    /// `INGOT_SHOTS=<dir> cargo test --bin ingot render_views -- --ignored`
    #[test]
    #[ignore = "writes images; needs a GPU"]
    fn render_views() {
        let dir = PathBuf::from(std::env::var("INGOT_SHOTS").unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = harness(&tmp, Some(fixture()));
        harness.state_mut().remover = delete_for_test;
        let shoot = |harness: &mut Harness<'_, IngotApp>, name: &str| {
            for _ in 0..30 {
                harness.step();
                std::thread::sleep(Duration::from_millis(30));
            }
            let image = harness.render().unwrap();
            image.save(dir.join(format!("{name}.png"))).unwrap();
        };
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().form.subfolder = "Wedding".to_string();
        harness.state_mut().form.organize = 2;
        shoot(&mut harness, "launchpad");

        harness.state_mut().choose_source(fixture());
        wait_until(&mut harness, "start", |app| app.session.is_some());
        wait_until(&mut harness, "previews", |app| app.previews_done);
        harness.key_press(Key::Num4);
        harness.key_press(Key::P);
        shoot(&mut harness, "grid");
        harness.key_press(Key::L);
        shoot(&mut harness, "clusters");
        harness.state_mut().library.as_mut().unwrap().jump(0);
        harness.key_press(Key::E);
        shoot(&mut harness, "loupe");
        {
            let library = harness.state_mut().library.as_mut().unwrap();
            library.click(0, false, false);
            library.click(2, false, true);
        }
        harness.key_press(Key::N);
        shoot(&mut harness, "survey");
        harness.key_press(Key::Escape);
        harness
            .state_mut()
            .library
            .as_mut()
            .unwrap()
            .clear_selection();
        let raw = {
            let library = harness.state().library.as_ref().unwrap();
            library
                .visible
                .iter()
                .position(|i| library.photos[*i].raw.is_some())
                .unwrap()
        };
        harness.state_mut().library.as_mut().unwrap().jump(raw);
        harness.key_press(Key::D);
        wait_until(&mut harness, "develop", |app| app.rendered.is_some());
        shoot(&mut harness, "develop");
        harness.key_press(Key::G);
        harness.step();
        harness.state_mut().finalize = Some(FinalizeDialog {
            keep_unflagged: false,
        });
        shoot(&mut harness, "finalize");
        harness.state_mut().finalize = None;
        harness.state_mut().confirm_finalize(false);
        wait_until(&mut harness, "finalized", |app| {
            app.finalize_result.is_some()
        });
        shoot(&mut harness, "finalized");
    }

    #[test]
    fn cards_are_found_by_their_dcim_folder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("EOS_DIGITAL/DCIM")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Backup")).unwrap();
        assert_eq!(
            find_cards(tmp.path()),
            [tmp.path().join("EOS_DIGITAL/DCIM")]
        );
        assert!(find_cards(&tmp.path().join("nope")).is_empty());
    }
}
