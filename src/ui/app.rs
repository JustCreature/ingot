//! The window: everything drawn, and the background work it starts.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::Instant,
};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use ingot_core::{
    AssetKey, DRIVE_ROOT_FOLDER, DateFolders, DriveConfig, DriveCredentials, DriveTarget, Engine,
    EngineConfig, ExtensionCase, ImportHistory, ImportOptions, LocalTarget, NameTemplate, Organize,
    ReplicationEvent, ReplicationReport, ReplicationTarget, ScanOptions, Target, TargetId,
    TargetKind, TriageState, execute_sweep, plan_import, plan_sweep,
};
use ingot_develop::{DevelopParams, DevelopedImage, PreviewBase, WhiteBalance};

use super::{
    AppPaths,
    images::{ImageCache, Tier, decode_jpeg},
    library::{Command, Filter, Library, Photo, command_for},
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

const PREVIEW_EDGE: usize = 1600;
const DRIVE_TARGET_ID: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Grid,
    Loupe,
    Develop,
}

enum Msg {
    Ready(Arc<Engine>),
    Preview,
    PreviewsDone,
    ScanInfo { ignored: usize, errors: usize },
    Full(AssetKey, Result<egui::ColorImage, String>),
    Base(AssetKey, Result<Arc<PreviewBase>, String>),
    Rendered(AssetKey, Result<DevelopedImage, String>),
    Copy(ReplicationEvent),
    CopyDone(ReplicationReport),
    Exported(Result<String, String>),
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
    previewed: Vec<String>,
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
            previewed: vec![],
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
            skip_suspected_duplicates: self.only_new,
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct CopyProgress {
    running: bool,
    total: usize,
    done: usize,
    failed: usize,
    bytes: u64,
    started: Option<Instant>,
    last_errors: Vec<String>,
    summary: Option<String>,
}

struct SweepDialog {
    paths: Vec<PathBuf>,
    left_in_cloud: usize,
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

    import_open: bool,
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
            import_open: false,
            form: ImportForm::default(),
            copy: CopyProgress::default(),
            sweep: None,
        };
        if let Some(folder) = folder {
            app.open_folder(folder);
        }
        app
    }

    fn open_folder(&mut self, folder: PathBuf) {
        if self.copy.running {
            self.status =
                Some("Wait for the import to finish before opening a folder.".to_string());
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
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let mut engine = Engine::new(EngineConfig::default());
            engine.scan_with(&folder, &ScanOptions::default());
            {
                let scan = engine
                    .scan_response
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                let _ = tx.send(Msg::ScanInfo {
                    ignored: scan.ignored.len(),
                    errors: scan.errors.len(),
                });
            }
            let previews = engine.ingest();
            let _ = tx.send(Msg::Ready(Arc::new(engine)));
            ctx.request_repaint();
            for event in previews.iter() {
                let ingot_core::IngestEvent::Preview(_) = event;
                let _ = tx.send(Msg::Preview);
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::PreviewsDone);
            ctx.request_repaint();
        });
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::ScanInfo { ignored, errors } => {
                    self.ignored = ignored;
                    self.scan_errors = errors;
                }
                Msg::Ready(engine) => {
                    self.library = Some(Library::new(
                        Arc::clone(&engine.scan_response),
                        &self.paths.triage,
                    ));
                    self.engine = Some(engine);
                    self.scanning = false;
                }
                Msg::Preview => self.previews_ready += 1,
                Msg::PreviewsDone => self.previews_done = true,
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
                        ..
                    } => {
                        self.copy.done += 1;
                        if newly_written {
                            self.copy.bytes += size;
                        }
                    }
                    ReplicationEvent::Failed { source, error, .. } => {
                        self.copy.failed += 1;
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
                    self.copy.running = false;
                    let seconds = self
                        .copy
                        .started
                        .map_or(0.0, |started| started.elapsed().as_secs_f64());
                    self.copy.summary = Some(format!(
                        "{} copies verified, {} failed, in {:.0} s (card done after {:.0} s)",
                        report.verified.len(),
                        report.failures.len(),
                        seconds,
                        report.primary_done_after.as_secs_f64(),
                    ));
                    if let Some(library) = &mut self.library {
                        library.add_replicas(report.verified);
                    }
                }
                Msg::Exported(result) => {
                    self.status = Some(match result {
                        Ok(message) => message,
                        Err(err) => format!("Export failed: {err}"),
                    });
                }
            }
        }
    }

    fn run_command(&mut self, command: Command, shift: bool) {
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
            Command::RowDown => library.move_focus(if self.view == View::Grid {
                self.columns as isize
            } else {
                1
            }),
            Command::RowUp => library.move_focus(if self.view == View::Grid {
                -(self.columns as isize)
            } else {
                -1
            }),
            Command::First => library.jump(0),
            Command::Last => library.jump(usize::MAX),
            Command::Grid => self.view = View::Grid,
            Command::Loupe => self.view = View::Loupe,
            Command::Develop => self.view = View::Develop,
            Command::Zoom => {
                if self.view == View::Grid {
                    self.view = View::Loupe;
                }
                self.zoomed = !self.zoomed;
                self.pan = Vec2::ZERO;
            }
            Command::BeforeAfter => self.show_before = !self.show_before,
            Command::ToggleAutoAdvance => library.auto_advance = !library.auto_advance,
            Command::TogglePanels => self.show_panels = !self.show_panels,
            Command::Import => self.import_open = true,
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
        if ctx.egui_wants_keyboard_input() || self.import_open || self.sweep.is_some() {
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

    fn import_plan(&self) -> Result<(ingot_core::ImportPlan, Option<ImportHistory>), String> {
        let engine = self.engine.as_ref().ok_or("The folder is still loading.")?;
        let source = self.source.as_ref().ok_or("No folder is open.")?;
        let options = self.form.options()?;
        let history = ImportHistory::open(self.paths.history.clone()).map_err(io_err)?;
        let plan =
            plan_import(&engine.scan_response, source, &options, Some(&history)).map_err(io_err)?;
        Ok((plan, Some(history)))
    }

    fn start_import(&mut self) -> Result<(), String> {
        if self.form.destination.trim().is_empty() {
            return Err("Choose a destination folder.".to_string());
        }
        if self.copy.running {
            return Err("An import is already running.".to_string());
        }
        let (plan, history) = self.import_plan()?;
        let engine = self.engine.clone().ok_or("The folder is still loading.")?;

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
        let primary = local(1, &self.form.destination);
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

        self.copy = CopyProgress {
            running: true,
            total: plan.files.len() * (secondaries.len() + 1),
            started: Some(Instant::now()),
            ..Default::default()
        };
        let handle = engine.start_import(plan, primary, secondaries, history);
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            for event in handle.progress.iter() {
                let _ = tx.send(Msg::Copy(event));
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::CopyDone(handle.wait()));
            ctx.request_repaint();
        });
        Ok(())
    }

    fn open_sweep(&mut self) {
        let Some(library) = &self.library else {
            return;
        };
        match plan_sweep(&library.store, &library.replicas) {
            Ok(plan) => {
                self.sweep = Some(SweepDialog {
                    paths: plan.paths().iter().map(|p| p.to_path_buf()).collect(),
                    left_in_cloud: plan.left_in_cloud,
                });
            }
            Err(err) => self.status = Some(err.to_string()),
        }
    }

    /// Draws one frame. Separate from `eframe::App` so tests can run it headless.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.ctx = ctx.clone();
        self.drain_messages();
        self.images.collect(&ctx);
        self.handle_keys(&ctx);
        self.sync_develop();
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
        } else {
            if self.show_panels {
                self.left_panel(ui);
                self.right_panel(ui);
            }
            if self.view != View::Grid {
                self.filmstrip(ui);
            }
            self.stage(ui);
        }
        self.import_window(&ctx);
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
                    if ui.button("Open folder…").clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        self.open_folder(folder);
                    }
                    let can_import = self.library.is_some();
                    if ui
                        .add_enabled(can_import, egui::Button::new("Import…"))
                        .on_hover_text("Cmd+Shift+I")
                        .clicked()
                    {
                        self.import_open = true;
                    }
                    ui.separator();
                    for (label, view, hint) in [
                        ("Grid", View::Grid, "G"),
                        ("Loupe", View::Loupe, "E"),
                        ("Develop", View::Develop, "D"),
                    ] {
                        if ui
                            .add_enabled(
                                can_import,
                                egui::Button::selectable(self.view == view, label),
                            )
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
                        if let Some(library) = &self.library {
                            let (total, picks, rejects) = library.counts();
                            ui.label(RichText::new(format!("{rejects} rejected")).color(REJECT));
                            ui.label(RichText::new(format!("{picks} picked")).color(ACCEPT));
                            ui.label(RichText::new(format!("{total} photos")).color(INK_2));
                        }
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
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
                        ui.label(RichText::new("Reading the folder…").color(INK_2));
                    } else if self.library.is_some() && !self.previews_done {
                        ui.spinner();
                        ui.label(
                            RichText::new(format!("Previews: {}", self.previews_ready))
                                .color(INK_2),
                        );
                        ui.separator();
                    }
                    if self.copy.running {
                        let total = self.copy.total.max(1);
                        let finished = self.copy.done + self.copy.failed;
                        ui.add(
                            egui::ProgressBar::new(finished as f32 / total as f32)
                                .desired_width(220.0)
                                .text(format!("{finished} / {total}")),
                        );
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
                    } else if let Some(summary) = &self.copy.summary {
                        ui.label(RichText::new(summary).color(if self.copy.failed > 0 {
                            REJECT
                        } else {
                            ACCEPT
                        }));
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
                        if let Some(library) = &self.library
                            && let Some(warning) = library.warnings.last()
                        {
                            ui.label(RichText::new("Warning").color(STAR))
                                .on_hover_text(warning);
                        }
                    });
                });
            });
    }

    fn launchpad(&mut self, ui: &mut egui::Ui) {
        let cards = find_cards(Path::new("/Volumes"));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.28);
                    if self.scanning {
                        ui.spinner();
                        ui.label(RichText::new("Reading the folder…").color(INK_2));
                        return;
                    }
                    ui.label(RichText::new("Start culling").size(26.0).color(INK));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Open a card or folder. Rate photos right away while they copy.",
                        )
                        .color(INK_3),
                    );
                    ui.add_space(18.0);
                    for card in cards {
                        if ui
                            .button(format!("Open card: {}", card.display()))
                            .clicked()
                        {
                            self.open_folder(card);
                        }
                    }
                    if ui.button("Open folder…").clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        self.open_folder(folder);
                    }
                });
            });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
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
                section(ui, "SHOW");
                let current = library.filter;
                for (label, filter) in [
                    ("All photos", Filter::All),
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
                if self.view == View::Grid {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Thumbnail size").color(INK_3).small());
                    ui.add(egui::Slider::new(&mut self.cell, 110.0..=420.0).show_value(false));
                }
                ui.add_space(12.0);
                section(ui, "CLEAN UP");
                if ui
                    .add_enabled(
                        !library.replicas.is_empty(),
                        egui::Button::new("Trash rejected copies…"),
                    )
                    .on_disabled_hover_text("Available after an import in this session.")
                    .clicked()
                {
                    self.sweep = Some(SweepDialog {
                        paths: vec![],
                        left_in_cloud: usize::MAX,
                    });
                }
            });
        if self
            .sweep
            .as_ref()
            .is_some_and(|s| s.left_in_cloud == usize::MAX)
        {
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
        if let Some(taken) = photo.taken {
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
        let texture = self.texture_for(&photo, false);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(5), PANEL_2);
        let label_height = 18.0;
        let image_area = Rect::from_min_max(
            rect.min + Vec2::splat(4.0),
            rect.max - Vec2::new(4.0, 4.0 + label_height),
        );
        if let Some(texture) = texture {
            let tint = if state == TriageState::Rejected {
                Color32::from_gray(90)
            } else {
                Color32::WHITE
            };
            draw_fit(&painter, &texture, image_area, tint);
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
            for star in 0..rating {
                let center = Pos2::new(rect.max.x - 10.0 - star as f32 * 12.0, y);
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

    fn import_window(&mut self, ctx: &egui::Context) {
        if !self.import_open {
            return;
        }
        let mut open = true;
        let mut start = false;
        let mut preview = false;
        let credentials_exist = self.paths.gdrive.exists();
        egui::Window::new("Import")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let form = &mut self.form;
                if let Some(source) = &self.source {
                    ui.label(RichText::new(format!("From  {}", source.display())).color(INK_2));
                }
                ui.add_space(6.0);
                section(ui, "DESTINATION");
                folder_field(ui, "To", &mut form.destination);
                ui.horizontal(|ui| {
                    ui.label("Into subfolder");
                    ui.text_edit_singleline(&mut form.subfolder);
                });
                ui.horizontal(|ui| {
                    ui.label("Organize");
                    egui::ComboBox::from_id_salt("organize")
                        .selected_text(
                            ["Into one folder", "By original folders", "By date"][form.organize],
                        )
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.organize, 0, "Into one folder");
                            ui.selectable_value(&mut form.organize, 1, "By original folders");
                            ui.selectable_value(&mut form.organize, 2, "By date");
                        });
                });
                if form.organize == 2 {
                    ui.horizontal(|ui| {
                        ui.label("Date format");
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
                                    ui.selectable_value(
                                        &mut form.date_format,
                                        format.to_string(),
                                        format,
                                    );
                                }
                            });
                    });
                }
                ui.add_space(6.0);
                section(ui, "FILE RENAMING");
                ui.checkbox(&mut form.rename, "Rename files");
                if form.rename {
                    ui.horizontal(|ui| {
                        ui.label("Template");
                        ui.text_edit_singleline(&mut form.template);
                    });
                    ui.label(
                        RichText::new("{name} {folder} {custom} {filenum} {seq:4} {date:%Y%m%d}")
                            .color(INK_3)
                            .small(),
                    );
                    ui.horizontal(|ui| {
                        ui.label("Custom text");
                        ui.text_edit_singleline(&mut form.custom_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Start number");
                        ui.add(egui::DragValue::new(&mut form.start_number).range(0..=9_999_999));
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
                ui.add_space(6.0);
                section(ui, "FILE HANDLING");
                ui.checkbox(&mut form.only_new, "Don't import suspected duplicates");
                folder_field(ui, "Second copy to", &mut form.second_copy);
                ui.add_enabled(
                    credentials_exist,
                    egui::Checkbox::new(
                        &mut form.gdrive,
                        format!("Back up to Google Drive ({DRIVE_ROOT_FOLDER})"),
                    ),
                )
                .on_disabled_hover_text("Run `ingot gdrive-login` first.");
                ui.add_space(8.0);
                if let Some(error) = &form.error {
                    ui.label(RichText::new(error).color(REJECT));
                }
                for line in &form.previewed {
                    ui.label(RichText::new(line).color(INK_2).small());
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Show where files go").clicked() {
                        preview = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Import").color(Color32::WHITE))
                                .fill(ACCENT),
                        )
                        .clicked()
                    {
                        start = true;
                    }
                });
            });
        if preview {
            match self.import_plan() {
                Ok((plan, _)) => {
                    let mut lines: Vec<String> = plan
                        .files
                        .iter()
                        .take(6)
                        .map(|file| file.relative.display().to_string())
                        .collect();
                    lines.push(format!(
                        "{} files, {} already imported, {} can't be planned",
                        plan.files.len(),
                        plan.skipped_duplicates.len(),
                        plan.failures.len()
                    ));
                    self.form.previewed = lines;
                    self.form.error = None;
                }
                Err(err) => self.form.error = Some(err),
            }
        }
        if start {
            match self.start_import() {
                Ok(()) => {
                    self.form.error = None;
                    self.import_open = false;
                }
                Err(err) => self.form.error = Some(err),
            }
        }
        if !open {
            self.import_open = false;
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
                            !dialog.paths.is_empty(),
                            egui::Button::new(RichText::new("Move to Trash").color(Color32::WHITE))
                                .fill(REJECT),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if confirm && let Some(library) = &mut self.library {
            let outcome = match plan_sweep(&library.store, &library.replicas) {
                Ok(plan) => Ok(execute_sweep(&plan)),
                Err(err) => Err(err.to_string()),
            };
            match outcome {
                Ok(report) => {
                    self.status = Some(format!(
                        "{} copies moved to the Trash, {} kept",
                        report.trashed.len(),
                        report.failures.len()
                    ));
                    library.warnings.extend(
                        report
                            .failures
                            .iter()
                            .map(|(path, why)| format!("{}: {why}", path.display())),
                    );
                    library.replicas.retain(|replica| match replica.location() {
                        ingot_core::ReplicaLocation::Local(path) => !report.trashed.contains(path),
                        ingot_core::ReplicaLocation::Remote(_) => true,
                    });
                }
                Err(err) => self.status = Some(err),
            }
            close = true;
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
                        ("Z", "Zoom to 1:1"),
                        ("\\", "Before and after"),
                        ("Cmd+Shift+C / V", "Copy / paste develop settings"),
                        ("Cmd+Shift+I", "Import"),
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
        ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(300.0));
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

    #[test]
    fn launchpad_appears_with_no_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, None);
        harness.step();
        harness.get_by_label("Start culling");
    }

    #[test]
    fn culling_works_while_previews_arrive_and_develop_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, Some(fixture()));
        wait_until(&mut harness, "library", |app| app.library.is_some());

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

        // Move to a RAW and develop it.
        harness
            .state_mut()
            .library
            .as_mut()
            .unwrap()
            .set_filter(Filter::All);
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
    fn import_dialog_copies_and_sweep_trashes_nothing_unrejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("photos");
        let mut harness = harness(&tmp, Some(fixture()));
        wait_until(&mut harness, "library", |app| app.library.is_some());

        harness.state_mut().form.destination = dest.display().to_string();
        harness.state_mut().form.subfolder = "Wedding".to_string();
        harness.state_mut().import_open = true;
        for _ in 0..5 {
            harness.step();
        }
        // The window's title has the same text, and the button comes after it.
        harness.get_all_by_label("Import").last().unwrap().click();
        wait_until(&mut harness, "import finished", |app| {
            !app.copy.running && app.copy.summary.is_some()
        });

        assert_eq!(harness.state().copy.failed, 0);
        let copied = std::fs::read_dir(dest.join("Wedding")).unwrap().count();
        assert_eq!(copied, 4);

        // Ratings after the import reach a sidecar next to the copies.
        harness.state_mut().library.as_mut().unwrap().jump(0);
        harness.state_mut().library.as_mut().unwrap().rate(5, false);
        let sidecars = std::fs::read_dir(dest.join("Wedding"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "xmp"))
            .count();
        assert_eq!(sidecars, 1);

        // Nothing is rejected, so the sweep offers nothing.
        harness.state_mut().open_sweep();
        assert!(harness.state().sweep.as_ref().unwrap().paths.is_empty());
    }

    #[test]
    fn import_needs_a_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(&tmp, Some(fixture()));
        wait_until(&mut harness, "library", |app| app.library.is_some());
        assert!(harness.state_mut().start_import().is_err());
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
        let mut harness = harness(&tmp, Some(fixture()));
        wait_until(&mut harness, "previews", |app| app.previews_done);
        let shoot = |harness: &mut Harness<'_, IngotApp>, name: &str| {
            for _ in 0..30 {
                harness.step();
                std::thread::sleep(Duration::from_millis(30));
            }
            let image = harness.render().unwrap();
            image.save(dir.join(format!("{name}.png"))).unwrap();
        };
        harness.key_press(Key::Num4);
        harness.key_press(Key::P);
        harness.key_press(Key::Num2);
        harness.key_press(Key::X);
        shoot(&mut harness, "grid");
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
        harness.key_press(Key::G);
        harness.state_mut().form.rename = true;
        harness.state_mut().form.organize = 2;
        harness.state_mut().import_open = true;
        shoot(&mut harness, "import");
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
