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
    DriveTarget, Engine, EngineConfig, ExtensionCase, Fingerprint, ImportHistory, ImportOptions,
    ImportQueue, LocalTarget, NameTemplate, Organize, PlannedFile, ReplicationEvent,
    ReplicationReport, ReplicationTarget, ScanOptions, Selection, SweepReport, Target, TargetId,
    TargetKind, TriageState, VerifiedReplica, execute_sweep, plan_import, plan_sweep,
    planned_relative, preview,
};
use ingot_develop::{DevelopParams, DevelopedImage, PreviewBase, WhiteBalance};

use super::{
    AppPaths,
    images::{ImageCache, Tier, decode_jpeg},
    library::{Command, Filter, ImportCounts, Library, Photo, command_for},
};

// Colors from docs/design/ingot_v2/styles.css.
const BG: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0f);
const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x19);
const PANEL_2: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1f);
const PANEL_3: Color32 = Color32::from_rgb(0x23, 0x23, 0x28);
const STAGE: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0b);
const LINE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x30);
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
    Hashed(AssetKey, Fingerprint, bool),
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
}

/// Slider renders: only the newest request matters, so a fast drag never queues up.
#[derive(Default)]
struct RenderSlot {
    job: Mutex<Option<(AssetKey, Arc<PreviewBase>, DevelopParams)>>,
    wake: Condvar,
}

struct ImportForm {
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
            // The ticks already say what comes in, and nothing has been opened yet to
            // date a file by, so names and folders use the file's own date.
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
    summary: Option<String>,
}

/// A running import. It starts when the destination is confirmed and stays open for
/// photos ticked later, until a new import starts.
struct Session {
    queue: Arc<ImportQueue>,
    options: ImportOptions,
    destination: PathBuf,
    next_number: u64,
    /// Each file keeps the name and number it was first given, even if it is unticked
    /// and ticked again.
    planned: HashMap<PathBuf, PlannedFile>,
}

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
    pending_tick: Option<usize>,

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
    gdrive_available: bool,
    session: Option<Session>,
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
            pending_tick: None,
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
            gdrive_available: false,
            session: None,
            hashes,
            form: ImportForm::default(),
            copy: CopyProgress::default(),
            sweep: None,
        };
        if let Some(folder) = folder {
            app.open_folder(folder);
        }
        app
    }

    /// Lists a card or folder for the import screen. Walks names, sizes and dates only.
    /// Opening a photo has to wait for a destination, since copying starts with it.
    fn open_folder(&mut self, folder: PathBuf) {
        if !self.leave_session() {
            return;
        }
        self.flush_params();
        self.source = Some(folder.clone());
        self.engine = None;
        self.library = None;
        self.scanning = true;
        self.previews_done = false;
        self.previews_ready = 0;
        self.view = View::Grid;
        self.status = None;
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

    /// Ends the running import if everything has finished. Says no otherwise, so a
    /// copy can't be walked away from by accident.
    fn leave_session(&mut self) -> bool {
        let pending = self
            .library
            .as_ref()
            .map_or(0, |library| library.import_counts().pending());
        if self.session.is_some() && pending > 0 {
            self.status = Some("Wait for the import to finish before starting a new one.".into());
            return false;
        }
        if let Some(session) = self.session.take() {
            session.queue.close();
        }
        true
    }

    fn new_import(&mut self) {
        if self.leave_session() {
            self.library = None;
            self.engine = None;
            self.source = None;
            self.copy = CopyProgress::default();
            self.status = None;
            self.view = View::Grid;
        }
    }

    /// Confirms the destination. This is the moment the card is first touched, and the
    /// copy, the headers and the previews all start together.
    fn start_session(&mut self) -> Result<(), String> {
        if self.session.is_some() {
            return Err("An import is already running.".to_string());
        }
        let destination = self.form.destination.trim().to_string();
        if destination.is_empty() {
            return Err("Choose a destination folder.".to_string());
        }
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
        let primary = local(1, &destination);
        let mut secondaries = vec![];
        if !self.form.second_copy.trim().is_empty() {
            secondaries.push(local(2, &self.form.second_copy));
        }
        if self.form.gdrive {
            let credentials = DriveCredentials::load(&self.paths.gdrive).map_err(io_err)?;
            secondaries.push(Arc::new(
                DriveTarget::new(
                    TargetId(DRIVE_TARGET_ID),
                    DriveConfig::default(),
                    credentials,
                )
                .map_err(io_err)?,
            ) as Arc<dyn ReplicationTarget>);
        }
        let history = ImportHistory::open(self.paths.history.clone()).map_err(io_err)?;

        let target_count = secondaries.len() + 1;
        let handle = engine.start_session(primary, secondaries, Some(history), vec![]);
        self.session = Some(Session {
            queue: Arc::clone(&handle.queue),
            next_number: options.start_number,
            options,
            destination: PathBuf::from(destination),
            planned: HashMap::new(),
        });
        self.copy = CopyProgress {
            started: Some(Instant::now()),
            ..Default::default()
        };
        if let Some(library) = &mut self.library {
            library.target_count = target_count;
        }
        let ticked: Vec<usize> = self.library.as_ref().map_or(vec![], |library| {
            (0..library.photos.len())
                .filter(|index| library.ticked.contains(&library.photos[*index].key))
                .collect()
        });
        self.queue_photos(&ticked);

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

    /// Ticks or unticks photos. During an import, a file whose read hasn't started
    /// leaves the queue, and one already copied keeps its copy.
    fn set_ticks(&mut self, indexes: &[usize], ticked: bool) {
        let Some(library) = &mut self.library else {
            return;
        };
        let keys: Vec<AssetKey> = indexes
            .iter()
            .map(|index| library.photos[*index].key.clone())
            .collect();
        for key in &keys {
            library.set_ticked(key, ticked);
        }
        if ticked {
            self.queue_photos(indexes);
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let mut started = 0;
        for index in indexes {
            let photo = &library.photos[*index];
            let mut any_started = false;
            for file in &photo.files {
                if !session.queue.remove(file) {
                    any_started = true;
                }
            }
            started += usize::from(any_started);
        }
        if started > 0 {
            self.status = Some(if indexes.len() == 1 {
                "Already copied or being copied, so that copy stays.".to_string()
            } else {
                format!("{started} photos already copied or being copied. Those copies stay.")
            });
        }
    }

    fn toggle_tick(&mut self, index: usize) {
        let ticked = self
            .library
            .as_ref()
            .is_some_and(|library| library.ticked.contains(&library.photos[index].key));
        self.set_ticks(&[index], !ticked);
    }

    fn tick_all(&mut self) {
        let skip = self.form.only_new && self.session.is_none();
        let Some(library) = &mut self.library else {
            return;
        };
        library.tick_all(skip);
        let indexes: Vec<usize> = (0..library.photos.len())
            .filter(|index| library.ticked.contains(&library.photos[*index].key))
            .collect();
        self.queue_photos(&indexes);
    }

    fn tick_none(&mut self) {
        let count = self.library.as_ref().map_or(0, |l| l.photos.len());
        let indexes: Vec<usize> = (0..count).collect();
        self.set_ticks(&indexes, false);
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
                    library.tick_all(self.form.only_new);
                    self.library = Some(library);
                    self.engine = Some(engine);
                    self.scanning = false;
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
                Msg::Hashed(key, print, from_thumb) => {
                    if let Some(library) = &mut self.library {
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
                    self.copy.summary = Some(format!(
                        "Last import: {} copies verified, {} failed",
                        report.verified.len(),
                        report.failures.len(),
                    ));
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
                                library.replicas.retain(|replica| match replica.location() {
                                    ingot_core::ReplicaLocation::Local(path) => {
                                        !report.trashed.contains(path)
                                    }
                                    ingot_core::ReplicaLocation::Remote(_) => true,
                                });
                            }
                        }
                        Err(err) => self.status = Some(err),
                    }
                }
            }
        }
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
            Command::Pick => library.flag(TriageState::Accepted, advance),
            Command::Reject => library.flag(TriageState::Rejected, advance),
            Command::Unflag => library.flag(TriageState::Pending, shift),
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
            Command::Tick => {
                if let Some(index) = library.visible.get(library.focus).copied() {
                    self.toggle_tick(index);
                }
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
            Command::Import => self.new_import(),
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
        if ctx.egui_wants_keyboard_input() || self.session.is_none() || self.sweep.is_some() {
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
        self.bottom_bar(ui);
        if self.library.is_none() {
            self.launchpad(ui);
        } else if self.session.is_none() {
            self.import_screen(ui);
        } else {
            if self.show_panels {
                self.left_panel(ui);
                self.right_panel(ui);
            }
            if !self.is_grid_view() {
                self.filmstrip(ui);
            }
            self.stage(ui);
        }
        if let Some(index) = self.pending_tick.take() {
            self.toggle_tick(index);
        }
        self.sweep_window(&ctx);
        self.help_window(&ctx);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .exact_size(46.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("INGOT").strong().color(INK).size(15.0));
                    ui.separator();
                    if ui
                        .button("New import")
                        .on_hover_text("Cmd+Shift+I")
                        .clicked()
                    {
                        self.new_import();
                    }
                    ui.separator();
                    let open = self.session.is_some();
                    for (label, view, hint) in [
                        ("Grid", View::Grid, "G"),
                        ("Clusters", View::Clusters, "L"),
                        ("Loupe", View::Loupe, "E"),
                        ("Develop", View::Develop, "D"),
                    ] {
                        if ui
                            .add_enabled(open, egui::Button::selectable(self.view == view, label))
                            .on_hover_text(hint)
                            .clicked()
                        {
                            self.view = view;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("?")
                            .on_hover_text("Keyboard shortcuts ( / )")
                            .clicked()
                        {
                            self.show_help = true;
                        }
                        if open && let Some(library) = &self.library {
                            let (total, picks, rejects) = library.counts();
                            ui.label(RichText::new(format!("{rejects} rejected")).color(REJECT));
                            ui.label(RichText::new(format!("{picks} picked")).color(ACCEPT));
                            ui.label(RichText::new(format!("{total} photos")).color(INK_2));
                        }
                    });
                });
            });
    }

    /// The count and destination, on screen at all times from the moment a folder is
    /// listed. Costs a little arithmetic per frame and never touches a disk.
    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let counts = self.library.as_ref().map(Library::import_counts);
        let destination = self.destination_line(counts.unwrap_or_default());
        egui::Panel::bottom("ingest")
            .exact_size(46.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.scanning {
                        ui.spinner();
                        ui.label(RichText::new("Listing the folder…").color(INK_2));
                    }
                    if let Some(counts) = counts {
                        ui.label(
                            RichText::new(format!(
                                "{} photos, {} files, {} to import",
                                counts.photos,
                                counts.files,
                                format_size(counts.bytes)
                            ))
                            .color(INK)
                            .strong(),
                        );
                        if self.session.is_some() {
                            let finished = counts.copied + counts.failed;
                            ui.add(
                                egui::ProgressBar::new(if counts.files == 0 {
                                    1.0
                                } else {
                                    finished as f32 / counts.files as f32
                                })
                                .desired_width(160.0)
                                .text(format!("{finished} / {}", counts.files)),
                            );
                            if counts.pending() > 0 {
                                let seconds = self
                                    .copy
                                    .started
                                    .map_or(1.0, |started| started.elapsed().as_secs_f64())
                                    .max(0.001);
                                ui.label(
                                    RichText::new(format!(
                                        "{:.0} MB/s",
                                        self.copy.bytes as f64 / 1e6 / seconds
                                    ))
                                    .color(INK_2),
                                );
                            } else if counts.files > 0 {
                                ui.label(RichText::new("All copied").color(ACCEPT));
                            }
                            if counts.kept > 0 {
                                ui.label(
                                    RichText::new(format!("{} unticked, copies kept", counts.kept))
                                        .color(INK_2),
                                );
                            }
                        }
                    }
                    if let Some(line) = &destination {
                        ui.separator();
                        ui.label(RichText::new(line).color(INK_2).small());
                    }
                    if self.copy.failed > 0 {
                        let errors = self.copy.last_errors.join("\n");
                        ui.label(
                            RichText::new(format!("{} failed", self.copy.failed)).color(REJECT),
                        )
                        .on_hover_text(errors);
                    }
                    if let Some(status) = &self.status {
                        ui.separator();
                        ui.label(RichText::new(status).color(INK_2));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.session.is_some() && !self.previews_done {
                            ui.spinner();
                            ui.label(
                                RichText::new(format!("Previews {}", self.previews_ready))
                                    .color(INK_3),
                            );
                        }
                        if self.ignored > 0 {
                            ui.label(
                                RichText::new(format!("{} files not imported", self.ignored))
                                    .color(INK_3),
                            )
                            .on_hover_text("Videos and other unsupported file types are skipped.");
                        }
                        if self.scan_errors > 0 {
                            ui.label(
                                RichText::new(format!("{} scan errors", self.scan_errors))
                                    .color(REJECT),
                            );
                        }
                        if let Some(warning) = self.library.as_ref().and_then(Library::last_warning)
                        {
                            ui.label(RichText::new("Warning").color(STAR))
                                .on_hover_text(warning);
                        }
                    });
                });
            });
    }

    /// One example path and how many more. Instant, and works before any file is open.
    fn destination_line(&self, counts: ImportCounts) -> Option<String> {
        let library = self.library.as_ref()?;
        let source = self.source.as_ref()?;
        let (options, root) = match &self.session {
            Some(session) => (session.options.clone(), session.destination.clone()),
            None => {
                let destination = self.form.destination.trim();
                if destination.is_empty() {
                    return Some("Choose a destination".to_string());
                }
                match self.form.options() {
                    Ok(options) => (options, PathBuf::from(destination)),
                    Err(err) => return Some(err),
                }
            }
        };
        let photo = library
            .photos
            .iter()
            .find(|photo| library.ticked.contains(&photo.key))?;
        let taken = photo.taken?;
        let file = photo.files.first()?;
        let relative =
            planned_relative(file, source, &options, options.start_number, taken).ok()?;
        let home = dirs::home_dir();
        Some(destination_summary(
            &root.join(relative),
            counts.files,
            home.as_deref(),
        ))
    }

    fn launchpad(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.28);
                    if self.scanning {
                        ui.spinner();
                        ui.label(RichText::new("Listing the folder…").color(INK_2));
                        return;
                    }
                    ui.label(RichText::new("New import").size(26.0).color(INK));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Pick a card or folder. Nothing is opened until you choose where it goes.",
                        )
                        .color(INK_3),
                    );
                    ui.add_space(18.0);
                    for card in self.cards.clone() {
                        if ui.button(format!("Card: {}", card.display())).clicked() {
                            self.open_folder(card);
                        }
                    }
                    if ui.button("Choose a folder…").clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        self.open_folder(folder);
                    }
                });
            });
    }

    /// Import mode: every photo by name with a tick, and where they will go. No photo
    /// has been opened yet, so there are no thumbnails here. The photos appear as soon
    /// as Start is pressed, and can still be ticked and unticked there.
    fn import_screen(&mut self, ui: &mut egui::Ui) {
        let mut start = false;
        let counts = self
            .library
            .as_ref()
            .map(Library::import_counts)
            .unwrap_or_default();
        egui::Panel::right("import_form")
            .exact_size(400.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(14))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.import_form(ui);
                    ui.add_space(12.0);
                    if let Some(error) = &self.form.error {
                        ui.label(RichText::new(error).color(REJECT));
                    }
                    let ready = !self.form.destination.trim().is_empty() && counts.files > 0;
                    let label = format!("Start: copy {} photos and cull", counts.photos);
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(label).color(Color32::WHITE))
                                .fill(ACCENT),
                        )
                        .on_disabled_hover_text("Choose a destination and tick at least one photo.")
                        .clicked()
                    {
                        start = true;
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Copying starts the moment the card is first read. You can cull, and change the ticks, while it runs.",
                        )
                        .color(INK_3)
                        .small(),
                    );
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(STAGE).inner_margin(10))
            .show(ui, |ui| {
                // Truncated: a long path must not widen this panel over the form.
                if let Some(source) = &self.source {
                    ui.add(
                        egui::Label::new(RichText::new(source.display().to_string()).color(INK_2))
                            .truncate(),
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Select all").clicked() {
                        self.tick_all();
                    }
                    if ui.button("Select none").clicked() {
                        self.tick_none();
                    }
                });
                ui.add_space(6.0);
                self.name_list(ui);
            });
        if start {
            match self.start_session() {
                Ok(()) => self.form.error = None,
                Err(err) => self.form.error = Some(err),
            }
        }
    }

    fn name_list(&mut self, ui: &mut egui::Ui) {
        let Some(library) = &self.library else {
            return;
        };
        let count = library.photos.len();
        if count == 0 {
            ui.label(RichText::new("No photos in this folder.").color(INK_3));
            return;
        }
        let cell = 240.0;
        let columns = ((ui.available_width() / cell) as usize).max(1);
        let rows = count.div_ceil(columns);
        let mut toggled = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_rows(ui, 22.0, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        for column in 0..columns {
                            let index = row * columns + column;
                            let Some(photo) = library.photos.get(index) else {
                                break;
                            };
                            let mut ticked = library.ticked.contains(&photo.key);
                            let already = library.already_imported.contains(&photo.key);
                            ui.allocate_ui(Vec2::new(cell - 8.0, 20.0), |ui| {
                                ui.set_width(cell - 8.0);
                                let label = RichText::new(format!(
                                    "{}{}  {}",
                                    photo.name,
                                    if photo.raw.is_some() && photo.has_jpeg {
                                        " + JPG"
                                    } else {
                                        ""
                                    },
                                    format_size(photo.size)
                                ))
                                .color(if already { INK_3 } else { INK_2 })
                                .small();
                                let response = ui.checkbox(&mut ticked, label);
                                if already {
                                    response.clone().on_hover_text("Already imported");
                                }
                                if response.changed() {
                                    toggled = Some(index);
                                }
                            });
                        }
                    });
                }
            });
        if let Some(index) = toggled {
            self.toggle_tick(index);
        }
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        let mut sweep = false;
        let (mut all, mut none) = (false, false);
        let Some(library) = &mut self.library else {
            return;
        };
        egui::Panel::left("left")
            .exact_size(220.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(12))
            .show(ui, |ui| {
                section(ui, "SOURCE");
                if let Some(source) = &self.source {
                    ui.label(
                        RichText::new(source.display().to_string())
                            .color(INK_2)
                            .small(),
                    );
                }
                ui.add_space(12.0);
                section(ui, "IMPORT");
                ui.horizontal(|ui| {
                    all = ui.button("All").clicked();
                    none = ui.button("None").clicked();
                });
                ui.label(
                    RichText::new("T ticks the current photo")
                        .color(INK_3)
                        .small(),
                );
                ui.add_space(12.0);
                section(ui, "SHOW");
                let current = library.filter;
                for (label, filter) in [
                    ("All photos", Filter::All),
                    ("Ticked", Filter::Ticked),
                    ("Not ticked", Filter::NotTicked),
                    ("Picked", Filter::Picked),
                    ("Rejected", Filter::Rejected),
                    ("Unflagged", Filter::Unflagged),
                    ("1 star and up", Filter::Stars(1)),
                    ("3 stars and up", Filter::Stars(3)),
                    ("5 stars", Filter::Stars(5)),
                ] {
                    if ui.selectable_label(current == filter, label).clicked() {
                        library.set_filter(filter);
                    }
                }
                ui.add_space(12.0);
                section(ui, "CULLING");
                ui.checkbox(&mut library.auto_advance, "Move on after a flag")
                    .on_hover_text("A");
                if matches!(self.view, View::Grid | View::Clusters) {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Thumbnail size").color(INK_3).small());
                    ui.add(egui::Slider::new(&mut self.cell, 110.0..=420.0).show_value(false));
                }
                ui.add_space(12.0);
                section(ui, "CLEAN UP");
                sweep = ui
                    .add_enabled(
                        !library.replicas.is_empty(),
                        egui::Button::new("Trash rejected copies…"),
                    )
                    .on_disabled_hover_text("Available once something has been copied.")
                    .clicked();
            });
        if all {
            self.tick_all();
        }
        if none {
            self.tick_none();
        }
        if sweep {
            self.open_sweep();
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
            .frame(egui::Frame::new().fill(PANEL).inner_margin(12))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.photo_details(ui);
                    if self.view == View::Develop {
                        ui.add_space(12.0);
                        changed = self.develop_controls(ui, &mut export);
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
        let Some(library) = &mut self.library else {
            return;
        };
        let Some(photo) = library.focused().cloned() else {
            return;
        };
        let (rating, state) = library.decisions(&photo.key);
        section(ui, "PHOTO");
        ui.label(RichText::new(&photo.name).color(INK));
        if photo.raw.is_some() && photo.has_jpeg {
            ui.label(RichText::new("RAW + JPEG").color(INK_3).small());
        }
        if let Some(taken) = photo.captured.or(photo.taken) {
            ui.label(
                RichText::new(taken.format("%Y-%m-%d %H:%M:%S").to_string())
                    .color(INK_2)
                    .small(),
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for star in 1..=5u8 {
                let (rect, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                let color = if star <= rating { STAR } else { INK_3 };
                paint_star(ui.painter(), rect.center(), 9.0, color);
                if response.on_hover_text(star.to_string()).clicked() {
                    library.rate(if rating == star { 0 } else { star }, false);
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let pick = state == TriageState::Accepted;
            let reject = state == TriageState::Rejected;
            if ui
                .selectable_label(pick, RichText::new("Pick  P").color(ACCEPT))
                .clicked()
            {
                library.flag(
                    if pick {
                        TriageState::Pending
                    } else {
                        TriageState::Accepted
                    },
                    false,
                );
            }
            if ui
                .selectable_label(reject, RichText::new("Reject  X").color(REJECT))
                .clicked()
            {
                library.flag(
                    if reject {
                        TriageState::Pending
                    } else {
                        TriageState::Rejected
                    },
                    false,
                );
            }
        });
        let copies = library
            .replicas
            .iter()
            .filter(|replica| photo.files.iter().any(|file| file == replica.source()))
            .count();
        if copies > 0 {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("{copies} verified copies"))
                    .color(ACCEPT)
                    .small(),
            );
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
        let spacing = 8.0;
        let width = ui.available_width() - 16.0;
        let columns = (((width + spacing) / (self.cell + spacing)) as usize).max(1);
        self.columns = columns;
        let cell = (width - spacing * (columns - 1) as f32) / columns as f32;
        let row_height = cell * 0.78 + spacing;
        let rows = count.div_ceil(columns);

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
                    ui.add_space(8.0);
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
                library.jump(position);
            }
            if let Some(position) = opened {
                library.jump(position);
                self.view = View::Loupe;
            }
        }
    }

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
        let (rating, state) = library.decisions(&photo.key);
        let ticked = library.ticked.contains(&photo.key);
        let copied = library.photo_copied(&photo);
        let texture = self.texture_for(&photo, false);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(5), PANEL_2);
        let label_height = 18.0;
        let image_area = Rect::from_min_max(
            rect.min + Vec2::splat(4.0),
            rect.max - Vec2::new(4.0, 4.0 + label_height),
        );
        if let Some(texture) = texture {
            let tint = if state == TriageState::Rejected || !ticked {
                Color32::from_gray(90)
            } else {
                Color32::WHITE
            };
            draw_fit(&painter, &texture, image_area, tint);
        }
        if copied {
            painter.text(
                image_area.left_bottom() + Vec2::new(4.0, -4.0),
                Align2::LEFT_BOTTOM,
                if ticked { "copied" } else { "copy kept" },
                FontId::proportional(10.0),
                ACCEPT,
            );
        }
        // On top of the cell, so a click here ticks rather than selects.
        let tick_rect = Rect::from_center_size(
            Pos2::new(rect.max.x - 12.0, rect.min.y + 12.0),
            Vec2::splat(16.0),
        );
        paint_tick(&painter, tick_rect, ticked);
        let tick = ui.interact(
            tick_rect.expand(3.0),
            ui.scope_id().with(("tick", index)),
            Sense::click(),
        );
        if tick.on_hover_text("Import this photo (T)").clicked() {
            self.pending_tick = Some(index);
        }
        painter.text(
            Pos2::new(rect.min.x + 6.0, rect.max.y - 5.0),
            Align2::LEFT_BOTTOM,
            &photo.name,
            FontId::proportional(11.0),
            INK_3,
        );
        if rating > 0 {
            // In a narrow cell the name needs the bottom row, so stars go on top.
            let y = if width < 150.0 {
                rect.min.y + 11.0
            } else {
                rect.max.y - 11.0
            };
            // Stars on top sit to the left of the tick box.
            let right = if width < 150.0 { 26.0 } else { 0.0 };
            for star in 0..rating {
                let center = Pos2::new(rect.max.x - 10.0 - right - star as f32 * 12.0, y);
                paint_star(&painter, center, 5.5, STAR);
            }
        }
        let flag = match state {
            TriageState::Accepted => Some(ACCEPT),
            TriageState::Rejected => Some(REJECT),
            TriageState::Pending => None,
        };
        if let Some(color) = flag {
            painter.circle_filled(rect.min + Vec2::splat(11.0), 5.0, color);
        }
        if focused {
            painter.rect_stroke(
                rect,
                CornerRadius::same(5),
                Stroke::new(2.0, ACCENT),
                StrokeKind::Inside,
            );
        }
        response
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

        let spacing = 8.0;
        let header = 30.0;
        let width = ui.available_width() - 16.0;
        let columns = (((width + spacing) / (self.cell + spacing)) as usize).max(1);
        self.columns = columns;
        let cell = (width - spacing * (columns - 1) as f32) / columns as f32;
        let row_height = cell * 0.78 + spacing;

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
                self.cluster_header(ui, origin + Vec2::new(8.0, top), width, number, group);
                let rows = group.len().div_ceil(columns);
                for row in 0..rows {
                    let y = top + header + row as f32 * row_height;
                    if y > viewport.max.y || y + row_height < viewport.min.y {
                        continue;
                    }
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(8.0, y),
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
                library.jump(position);
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
        let scroll_focus = self.scroll_to_focus;
        let mut clicked = None;
        egui::Panel::bottom("film")
            .exact_size(100.0)
            .frame(egui::Frame::new().fill(BG).inner_margin(6))
            .show(ui, |ui| {
                let cell = 110.0;
                let mut area = egui::ScrollArea::horizontal().auto_shrink(false);
                if scroll_focus {
                    let offset =
                        (focus as f32 * (cell + 6.0) - ui.available_width() / 2.0).max(0.0);
                    area = area.horizontal_scroll_offset(offset);
                }
                ui.spacing_mut().item_spacing.x = 6.0;
                let step = cell + 6.0;
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
                            let response = self.grid_cell(ui, index, position, cell, 84.0);
                            if response.clicked() {
                                clicked = Some(position);
                            }
                            ui.add_space(6.0);
                        }
                        ui.add_space(count.saturating_sub(last) as f32 * step);
                    });
                });
            });
        self.scroll_to_focus = false;
        if let (Some(position), Some(library)) = (clicked, &mut self.library) {
            library.jump(position);
        }
    }

    /// The options chosen before anything is opened.
    fn import_form(&mut self, ui: &mut egui::Ui) {
        let credentials_exist = self.gdrive_available;
        let form = &mut self.form;
        section(ui, "DESTINATION");
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
        let only_new_changed = ui
            .checkbox(&mut form.only_new, "Don't import suspected duplicates")
            .changed();
        folder_field(ui, "Second copy to", &mut form.second_copy);
        ui.add_enabled(
            credentials_exist,
            egui::Checkbox::new(
                &mut form.gdrive,
                format!("Back up to Google Drive ({DRIVE_ROOT_FOLDER})"),
            ),
        )
        .on_disabled_hover_text("Run `ingot gdrive-login` first.");
        if only_new_changed {
            let only_new = form.only_new;
            if let Some(library) = &mut self.library {
                library.tick_all(only_new);
            }
        }
    }

    fn sweep_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.sweep else {
            return;
        };
        let mut close = false;
        let mut confirm = false;
        egui::Window::new("Trash rejected copies")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                if dialog.planning {
                    ui.spinner();
                    ui.label(RichText::new("Checking the copies…").color(INK_2));
                    return;
                }
                ui.label(format!("{} copies go to the Trash:", dialog.paths.len()));
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
                        ("P / X / U", "Pick / reject / clear flag"),
                        ("Shift + any of those", "Also move to the next photo"),
                        ("A", "Move on after a flag, on or off"),
                        ("Arrow keys", "Move"),
                        ("Home / End", "First / last photo"),
                        ("G", "Grid"),
                        ("E or Enter", "Loupe"),
                        ("D", "Develop"),
                        ("L", "Clusters"),
                        ("T", "Tick or untick for import"),
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
            let Some(print) = Fingerprint::from_rgb(&image.pixels, image.width, image.height)
            else {
                continue;
            };
            if tx.send(Msg::Hashed(key, print, from_thumb)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    jobs
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

fn paint_tick(painter: &egui::Painter, rect: Rect, ticked: bool) {
    let radius = CornerRadius::same(3);
    if ticked {
        painter.rect_filled(rect, radius, ACCENT);
        let stroke = Stroke::new(2.0, Color32::WHITE);
        let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * rect.width();
        painter.line_segment([at(0.22, 0.52), at(0.42, 0.72)], stroke);
        painter.line_segment([at(0.42, 0.72), at(0.78, 0.30)], stroke);
    } else {
        painter.rect_filled(rect, radius, Color32::from_black_alpha(140));
        painter.rect_stroke(rect, radius, Stroke::new(1.5, INK_2), StrokeKind::Inside);
    }
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

    /// A harness on the import screen, with the folder listed.
    fn listed(tmp: &TempDir) -> Harness<'static, IngotApp> {
        let mut harness = harness(tmp, Some(fixture()));
        wait_until(&mut harness, "listing", |app| app.library.is_some());
        harness
    }

    fn start(harness: &mut Harness<'static, IngotApp>, dest: &Path) {
        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().start_session().unwrap();
        harness.step();
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.ends_with(".tmp"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn index_named(harness: &Harness<'static, IngotApp>, name: &str) -> usize {
        let library = harness.state().library.as_ref().unwrap();
        library.photos.iter().position(|p| p.name == name).unwrap()
    }

    #[test]
    fn the_first_screen_asks_for_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, None);
        harness.step();
        harness.get_by_label_contains("Pick a card or folder");
    }

    #[test]
    fn no_photo_is_opened_until_a_destination_is_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = listed(&tmp);
        for _ in 0..20 {
            harness.step();
            std::thread::sleep(Duration::from_millis(10));
        }

        {
            let app = harness.state();
            let library = app.library.as_ref().unwrap();
            assert_eq!(library.photos.len(), 3);
            // Names and sizes are known, and not one byte of a photo has been read.
            assert_eq!(library.import_counts().files, 4);
            assert!(library.import_counts().bytes > 1_000_000);
            let scan = library.store.read().unwrap();
            assert!(scan.assets.values().all(|asset| {
                asset.exif_data.captured_at.is_none()
                    && asset.exif_data.thumbnail.is_none()
                    && asset.image_data.thumb.is_none()
            }));
            assert_eq!(library.fingerprint_count(), 0);
        }

        // Keys do nothing yet, and a missing destination stops the start.
        harness.key_press(Key::E);
        harness.step();
        assert_eq!(harness.state().view, View::Grid);
        assert!(harness.state_mut().start_session().is_err());
        assert!(harness.state().session.is_none());
    }

    #[test]
    fn the_count_and_destination_are_on_screen_before_starting() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = listed(&tmp);
        harness.state_mut().form.destination = "/Volumes/Photos".to_string();
        harness.state_mut().form.subfolder = "Wedding".to_string();
        harness.step();
        harness.step();

        let line = {
            let app = harness.state();
            let counts = app.library.as_ref().unwrap().import_counts();
            app.destination_line(counts).unwrap()
        };
        assert!(line.starts_with("/Volumes/Photos/Wedding/"), "{line}");
        assert!(line.ends_with("… (4 files)"), "{line}");
        harness.get_by_label_contains("3 photos, 4 files");
        harness.get_by_label_contains(&line);
    }

    #[test]
    fn only_ticked_photos_are_copied_and_ticks_can_change_during_the_import() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);

        // Unticked in import mode, before anything has been read.
        let cows = index_named(&harness, "IMG_1868.JPG");
        harness.state_mut().toggle_tick(cows);
        assert_eq!(
            harness
                .state()
                .library
                .as_ref()
                .unwrap()
                .import_counts()
                .photos,
            2
        );

        start(&mut harness, &dest);
        wait_until(&mut harness, "the ticked files", |app| {
            let counts = app.library.as_ref().unwrap().import_counts();
            counts.files > 0 && counts.pending() == 0
        });
        assert_eq!(
            files_in(&dest),
            ["IMG_1800.CR2", "IMG_1800.JPG", "IMG_1939.CR2"]
        );
        assert_eq!(harness.state().copy.failed, 0);

        // Ticked after the queue has drained, and it still comes in.
        harness.state_mut().toggle_tick(cows);
        wait_until(&mut harness, "the file ticked later", |app| {
            app.library.as_ref().unwrap().import_counts().pending() == 0
        });
        assert_eq!(files_in(&dest).len(), 4);

        // Unticked after copying: the copy stays, and the app says so.
        let person = index_named(&harness, "IMG_1939.CR2");
        harness.state_mut().toggle_tick(person);
        let counts = harness.state().library.as_ref().unwrap().import_counts();
        assert_eq!(counts.kept, 1);
        assert_eq!(counts.photos, 2);
        assert!(harness.state().status.as_ref().unwrap().contains("stays"));
        assert_eq!(files_in(&dest).len(), 4);

        // Ticked again: nothing is copied twice.
        harness.state_mut().toggle_tick(person);
        assert_eq!(files_in(&dest).len(), 4);
    }

    #[test]
    fn select_none_then_all_works_during_the_import_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = listed(&tmp);
        harness.state_mut().tick_none();
        assert_eq!(
            harness
                .state()
                .library
                .as_ref()
                .unwrap()
                .import_counts()
                .files,
            0
        );
        harness.state_mut().tick_all();
        assert_eq!(
            harness
                .state()
                .library
                .as_ref()
                .unwrap()
                .import_counts()
                .files,
            4
        );
    }

    #[test]
    fn a_second_import_leaves_what_was_imported_unticked() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);
        start(&mut harness, &dest);
        wait_until(&mut harness, "first import", |app| {
            app.library.as_ref().unwrap().import_counts().pending() == 0
        });
        // The history is written just after the proof reaches the app.
        std::thread::sleep(Duration::from_millis(200));

        harness.state_mut().new_import();
        assert!(harness.state().library.is_none());
        harness.state_mut().open_folder(fixture());
        wait_until(&mut harness, "second listing", |app| app.library.is_some());

        let library = harness.state().library.as_ref().unwrap();
        assert_eq!(library.already_imported.len(), 3);
        assert_eq!(library.import_counts().photos, 0);
    }

    #[test]
    fn a_new_import_is_refused_while_files_are_still_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);
        start(&mut harness, &dest);
        // Files have been queued this very frame, so they can't all be verified yet.
        harness.state_mut().new_import();
        assert!(harness.state().session.is_some());
        assert!(harness.state().status.is_some());
    }

    #[test]
    fn culling_works_while_previews_arrive_and_develop_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);
        start(&mut harness, &dest);

        // Rate before previews or copies have finished.
        harness.key_press(Key::Num4);
        harness.step();
        harness.key_press(Key::X);
        harness.step();
        {
            let library = harness.state().library.as_ref().unwrap();
            assert_eq!(library.counts().2, 1);
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
    fn clusters_regroup_on_the_slider_and_keys_still_cull() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);
        start(&mut harness, &dest);
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
    fn ratings_after_the_import_reach_a_sidecar_next_to_the_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = listed(&tmp);
        harness.state_mut().form.subfolder = "Wedding".to_string();
        start(&mut harness, &dest);
        wait_until(&mut harness, "import finished", |app| {
            app.library.as_ref().unwrap().import_counts().pending() == 0
        });

        assert_eq!(harness.state().copy.failed, 0);
        assert_eq!(files_in(&dest.join("Wedding")).len(), 4);

        harness.state_mut().library.as_mut().unwrap().jump(0);
        harness.state_mut().library.as_mut().unwrap().rate(5, false);
        // Written off the UI thread.
        let wedding = dest.join("Wedding");
        wait_until(&mut harness, "sidecar", |_| {
            files_in(&wedding).iter().any(|name| name.ends_with(".xmp"))
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
        let mut harness = listed(&tmp);
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
        let cows = index_named(&harness, "IMG_1868.JPG");
        harness.state_mut().toggle_tick(cows);
        shoot(&mut harness, "import");

        start(&mut harness, &dest);
        wait_until(&mut harness, "previews", |app| app.previews_done);
        harness.key_press(Key::Num4);
        harness.key_press(Key::P);
        shoot(&mut harness, "grid");
        harness.key_press(Key::L);
        shoot(&mut harness, "clusters");
        harness.state_mut().library.as_mut().unwrap().jump(0);
        harness.key_press(Key::E);
        shoot(&mut harness, "loupe");
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
