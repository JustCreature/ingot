//! The ingest engine: a configurable, instantiable handle the app constructs
//! once and drives. It owns the cross-cutting policy (preview/thumb resolution,
//! JPEG quality) and — once step 4 lands — the shared concurrency resources
//! (a source-read semaphore sized to `card_read_permits`, a CPU pool sized to
//! `cpu_threads`). Those resources are shared across all in-flight assets, so
//! they must live on an instance, not in free functions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::asset::{AssetKey, ExifAssetData, ImageProcessingError};
use crate::import::{ImportHistory, ImportPlan};
use crate::metadata::{EXIF_HEADER_PREFIX_LEN, read_exif_data};
use crate::preview::{compress, make_preview_from_jpeg_bytes, make_thumbnail_from_jpeg_bytes};
use crate::replicate::{
    CardGate, CopyEngine, DEFAULT_READ_CHUNK, ImportQueue, ReplicationEvent, ReplicationFailure,
    ReplicationReport, ReplicationTarget, TargetId, UploadFile, upload_copies,
};
use crate::scan::{ScanOptions, ScanResponse, scan_source};
use crate::{FileKind, PreviewStrip, enrich_assets, read_embedded_preview};

/// Tunable engine policy. Construct via `..Default::default()` and override what
/// you need; the defaults are the values measured/locked in Phase 2.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Long-edge (px) of the generated grid/loupe preview.
    pub preview_long_edge: usize,
    /// Long-edge (px) of the generated grid thumbnail.
    pub thumb_long_edge: usize,
    /// JPEG encode quality (1..=100) for generated outputs.
    pub jpeg_quality: i32,
    /// Width of the **dedicated reader tier** — source-card concurrent reads,
    /// sized *independently* of the CPU tier. A *device property*: measured ~4
    /// for a camera card over a fast reader (queue depth saturates ~4, >4
    /// regresses); a serial SD reader wants 1-2; SSD/NVMe wants more. Decoupled
    /// from `cpu_threads` precisely so reads and decode scale separately.
    pub card_read_permits: usize,
    /// Width of the **CPU tier** — decode/resize/encode workers, fed by the
    /// reader tier over a bounded channel. Defaults to the machine's available
    /// parallelism (CPU-bound work scales with cores).
    pub cpu_threads: usize,
    /// How many files may wait in RAM for the primary target during an import.
    pub read_channel_depth: usize,
    /// For measuring: bytes per second of a simulated slow card.
    pub simulated_card_speed: Option<u64>,
    /// Bytes the copy reads before letting a culling read go first.
    pub copy_read_chunk: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            preview_long_edge: 1920,
            thumb_long_edge: 512,
            jpeg_quality: 85,
            card_read_permits: 2,
            cpu_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            read_channel_depth: 4,
            simulated_card_speed: None,
            copy_read_chunk: DEFAULT_READ_CHUNK,
        }
    }
}

#[derive(Debug)]
pub struct IngestMessage {
    pub key: AssetKey,
    pub error: bool,
}

/// What `ingest` streams back to the consumer, one per processed asset.
/// (Single-message-per-asset for v1; per-tier splitting is a later refinement.)
#[derive(Debug)]
pub enum IngestEvent {
    Preview(IngestMessage),
}

/// What `start_loading` reports, in display order within each stage.
#[derive(Debug)]
pub enum LoadEvent {
    /// Stage 1: the photo's tiny thumbnail and details are in the store.
    Header {
        key: AssetKey,
        orientation: Option<u16>,
        captured_at: Option<chrono::NaiveDateTime>,
    },
    HeadersDone,
    /// Stage 2: the photo's grid thumbnail and preview are in the store.
    Preview(IngestMessage),
    PreviewsDone,
}

/// The engine instance. Cheap to construct; holds config now, concurrency
/// resources after step 4.
pub struct Engine {
    config: EngineConfig,
    pub scan_response: Arc<RwLock<ScanResponse>>,
    card: Arc<CardGate>,
}

/// A running import. It carries on in the background whether or not anyone reads
/// its progress.
pub struct ImportHandle {
    /// Add or take back files while the import runs. Close it to let the import end.
    pub queue: Arc<ImportQueue>,
    pub progress: crossbeam_channel::Receiver<ReplicationEvent>,
    finished: std::thread::JoinHandle<ReplicationReport>,
}

impl ImportHandle {
    /// Waits for the import to end and returns its report.
    pub fn wait(self) -> ReplicationReport {
        match self.finished.join() {
            Ok(report) => report,
            Err(_) => {
                let mut report = ReplicationReport::default();
                report.failures.push(ReplicationFailure {
                    source: None,
                    target: None,
                    error: "import worker panicked".into(),
                });
                report
            }
        }
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        let card =
            CardGate::with_simulated_speed(config.card_read_permits, config.simulated_card_speed);
        Engine {
            card,
            config,
            scan_response: Arc::new(RwLock::new(ScanResponse {
                assets: HashMap::new(),
                collisions: vec![],
                errors: vec![],
                ignored: vec![],
                file_meta: HashMap::new(),
            })),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Fast enumeration: walk + pair, no file-content reads. Gives the UI its
    /// grid skeleton immediately.
    pub fn scan(&mut self, source: &Path) -> Arc<RwLock<ScanResponse>> {
        self.scan_with(source, &ScanOptions::default())
    }

    /// Builds the new skeleton with no lock held, then swaps it in, so readers of
    /// the store are never kept waiting on card I/O.
    pub fn scan_with(&mut self, source: &Path, options: &ScanOptions) -> Arc<RwLock<ScanResponse>> {
        let mut response = scan_source(source, options);
        enrich_assets(&mut response.assets);
        *self.scan_response.write().unwrap() = response;
        Arc::clone(&self.scan_response)
    }

    /// Reads the largest JPEG of an asset for 1:1 viewing, ahead of everything else on
    /// the card. Blocks, so call it off the UI thread.
    pub fn read_full_jpeg(&self, key: &AssetKey) -> Result<Vec<u8>, ImageProcessingError> {
        let (kind, path, strip) = {
            let scan = self
                .scan_response
                .read()
                .map_err(|err| format!("scan store lock poisoned: {err}"))?;
            let asset = scan
                .assets
                .get(key)
                .ok_or("photo is no longer in the library")?;
            let (kind, path) = asset.files.get_prioritized();
            (
                kind,
                path.to_path_buf(),
                asset.exif_data.embedded_preview_file_location,
            )
        };
        let _permit = self.card.acquire_on_demand();
        let bytes = read_source_bytes(kind, &path, strip)?;
        self.card.simulate_read(bytes.len() as u64);
        Ok(bytes)
    }

    /// A card slot for reading a whole RAW for develop, ahead of everything else.
    pub fn card_permit(&self) -> crate::CardPermit {
        self.card.acquire_on_demand()
    }

    /// Walks a folder for names, sizes and dates. Opens no photo, so this can happen
    /// before the photographer has chosen where the copies go.
    pub fn scan_names(&mut self, source: &Path, options: &ScanOptions) {
        *self.scan_response.write().unwrap() = scan_source(source, options);
    }

    /// Starts copying `plan` in the background and returns at once. Holds no lock
    /// on the store, and previews go first on the card, so triage is never waiting
    /// on the copy. `primary` paces the card and should be the fastest local disk.
    /// Files that reached every target are added to `history`.
    pub fn start_import(
        &self,
        plan: ImportPlan,
        primary: Arc<dyn ReplicationTarget>,
        secondaries: Vec<Arc<dyn ReplicationTarget>>,
        history: Option<ImportHistory>,
    ) -> ImportHandle {
        let handle = self.start_session(primary, secondaries, history, plan.failures);
        for file in plan.files {
            handle.queue.push(file);
        }
        handle.queue.close();
        handle
    }

    /// Starts an import whose files come through `queue` on the returned handle, so
    /// photos can be ticked and unticked while it runs. It runs until the queue is
    /// closed. A file goes into `history` as soon as it is on every target, so a
    /// crash in a long session loses nothing already copied.
    pub fn start_session(
        &self,
        primary: Arc<dyn ReplicationTarget>,
        secondaries: Vec<Arc<dyn ReplicationTarget>>,
        history: Option<ImportHistory>,
        failures: Vec<ReplicationFailure>,
    ) -> ImportHandle {
        let (raw_progress_tx, raw_progress) = crossbeam_channel::unbounded();
        let (progress_tx, progress) = crossbeam_channel::unbounded();
        let target_count = secondaries.len() + 1;
        let queue = ImportQueue::new();
        let mut copy = CopyEngine::new(
            Arc::clone(&self.card),
            self.config.read_channel_depth,
            primary,
        )
        .with_progress(raw_progress_tx)
        .with_read_chunk(self.config.copy_read_chunk);
        for secondary in secondaries {
            copy = copy.with_secondary(secondary);
        }

        // Watches every event on its way to the consumer, and keeps the history current.
        let recorder = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let mut history = history;
                let mut reached: HashMap<PathBuf, HashSet<TargetId>> = HashMap::new();
                let mut errors: Vec<ReplicationFailure> = vec![];
                for event in raw_progress.iter() {
                    if let ReplicationEvent::Verified { source, target, .. } = &event {
                        let targets = reached.entry(source.clone()).or_default();
                        targets.insert(*target);
                        if targets.len() == target_count
                            && let Some(history) = &mut history
                            && let Some(key) = queue.duplicate_key(source)
                            && let Err(error) = history.record([key])
                        {
                            errors.push(ReplicationFailure {
                                source: Some(source.clone()),
                                target: None,
                                error: format!("could not update the import history: {error}")
                                    .into(),
                            });
                        }
                    }
                    let _ = progress_tx.send(event);
                }
                errors
            })
        };

        let finished = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let mut report = ReplicationReport::default();
                        report.failures.push(ReplicationFailure {
                            source: None,
                            target: None,
                            error: format!("could not start the import runtime: {err}").into(),
                        });
                        return report;
                    }
                };
                let mut report = runtime.block_on(copy.replicate_queue(queue));
                report.failures.extend(failures);
                // Dropping the engine closes the progress channel, which ends the recorder.
                drop(copy);
                if let Ok(errors) = recorder.join() {
                    report.failures.extend(errors);
                }
                report
            })
        };

        ImportHandle {
            queue,
            progress,
            finished,
        }
    }

    /// Stage 2 alone, for callers that already read the headers: previews in key order.
    pub fn ingest(&self) -> crossbeam_channel::Receiver<IngestEvent> {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut order: Vec<AssetKey> = self
            .scan_response
            .read()
            .unwrap()
            .assets
            .keys()
            .cloned()
            .collect();
        order.sort_by(|a, b| (&a.dir, &a.stem).cmp(&(&b.dir, &b.stem)));
        let loader = self.loader(order, Arc::new(AtomicUsize::new(0)));
        std::thread::spawn(move || {
            loader.previews(|message| {
                let _ = tx.send(IngestEvent::Preview(message));
            });
        });
        rx
    }

    /// Loads everything for the photographer, strictly in `order`, the display order:
    /// first the header thumbnail of every photo, then the preview of every photo. If
    /// the photographer jumps ahead, `focus` (a position in `order`) pulls the photos
    /// around it forward, and the rest carry on in order afterwards.
    pub fn start_loading(
        &self,
        order: Vec<AssetKey>,
        focus: Arc<AtomicUsize>,
    ) -> crossbeam_channel::Receiver<LoadEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let loader = self.loader(order, focus);
        std::thread::spawn(move || {
            loader.headers(|key, data| {
                let _ = tx.send(LoadEvent::Header {
                    key,
                    orientation: data.orientation,
                    captured_at: data.captured_at,
                });
            });
            let _ = tx.send(LoadEvent::HeadersDone);
            loader.previews(|message| {
                let _ = tx.send(LoadEvent::Preview(message));
            });
            let _ = tx.send(LoadEvent::PreviewsDone);
        });
        rx
    }

    fn loader(&self, order: Vec<AssetKey>, focus: Arc<AtomicUsize>) -> Loader {
        Loader {
            store: Arc::clone(&self.scan_response),
            card: Arc::clone(&self.card),
            order,
            focus,
            read_threads: self.config.card_read_permits.max(1),
            cpu_threads: self.config.cpu_threads.max(1),
        }
    }
}

/// One stage's place in the display order. Hands out the next photo to work on: the
/// first not yet taken, except that photos just after the focus go first.
struct OrderedWork {
    taken: Vec<bool>,
    frontier: usize,
    next_sequence: usize,
}

/// Photos after the focus to pull forward: about two rows of the grid.
const FOCUS_WINDOW: usize = 12;

impl OrderedWork {
    fn new(len: usize) -> Self {
        OrderedWork {
            taken: vec![false; len],
            frontier: 0,
            next_sequence: 0,
        }
    }

    /// The next (sequence number, position in the order).
    fn next(&mut self, focus: usize) -> Option<(usize, usize)> {
        let len = self.taken.len();
        let near_focus = (focus..focus.saturating_add(FOCUS_WINDOW).min(len))
            .find(|position| !self.taken[*position]);
        while self.frontier < len && self.taken[self.frontier] {
            self.frontier += 1;
        }
        let position = near_focus.or((self.frontier < len).then_some(self.frontier))?;
        self.taken[position] = true;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        Some((sequence, position))
    }
}

/// A photo's bytes on their way from the reader to the CPU tier.
struct RawPreview {
    sequence: usize,
    key: AssetKey,
    bytes: Result<Vec<u8>, ImageProcessingError>,
}

struct FinishedPreview {
    sequence: usize,
    key: AssetKey,
    thumb: Option<Vec<u8>>,
    preview: Option<Vec<u8>>,
    errors: Vec<ImageProcessingError>,
}

struct Loader {
    store: Arc<RwLock<ScanResponse>>,
    card: Arc<CardGate>,
    order: Vec<AssetKey>,
    focus: Arc<AtomicUsize>,
    read_threads: usize,
    cpu_threads: usize,
}

impl Loader {
    /// Stage 1: one small read per photo, so one reader in order is all it needs.
    fn headers(&self, mut on_each: impl FnMut(AssetKey, &ExifAssetData)) {
        let mut work = OrderedWork::new(self.order.len());
        while let Some((_, position)) = work.next(self.focus.load(Ordering::Relaxed)) {
            let key = &self.order[position];
            let path = {
                let scan = self.store.read().unwrap();
                let Some(asset) = scan.assets.get(key) else {
                    continue;
                };
                asset.files.get_prioritized().1.to_path_buf()
            };
            let data = {
                let _permit = self.card.acquire_priority();
                self.card.simulate_read(EXIF_HEADER_PREFIX_LEN);
                read_exif_data(&path)
            };
            let Some(data) = data else { continue };
            if let Some(asset) = self.store.write().unwrap().assets.get_mut(key) {
                asset.exif_data = data.clone();
            }
            on_each(key.clone(), &data);
        }
    }

    /// Stage 2: readers take photos in order, the CPU tier decodes them in parallel,
    /// and a final step puts the results back in order before anyone sees them. The
    /// bounded channel between the tiers limits how many buffers are in RAM.
    fn previews(&self, mut on_each: impl FnMut(IngestMessage)) {
        let work = Mutex::new(OrderedWork::new(self.order.len()));
        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<RawPreview>(self.cpu_threads * 2);
        let (done_tx, done_rx) = crossbeam_channel::unbounded::<FinishedPreview>();

        std::thread::scope(|scope| {
            for _ in 0..self.read_threads {
                let raw_tx = raw_tx.clone();
                let work = &work;
                scope.spawn(move || {
                    loop {
                        let focus = self.focus.load(Ordering::Relaxed);
                        let Some((sequence, position)) =
                            work.lock().unwrap_or_else(|e| e.into_inner()).next(focus)
                        else {
                            return;
                        };
                        let key = self.order[position].clone();
                        let source = {
                            let scan = self.store.read().unwrap();
                            scan.assets.get(&key).map(|asset| {
                                let (kind, path) = asset.files.get_prioritized();
                                (
                                    kind,
                                    path.to_path_buf(),
                                    asset.exif_data.embedded_preview_file_location,
                                )
                            })
                        };
                        let bytes = match source {
                            Some((kind, path, strip)) => {
                                // The slot covers only the read, not the wait on the channel.
                                let _permit = self.card.acquire_priority();
                                let bytes = read_source_bytes(kind, &path, strip);
                                if let Ok(bytes) = &bytes {
                                    self.card.simulate_read(bytes.len() as u64);
                                }
                                bytes
                            }
                            None => Err("photo is no longer in the library".into()),
                        };
                        if raw_tx
                            .send(RawPreview {
                                sequence,
                                key,
                                bytes,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
            drop(raw_tx);

            for _ in 0..self.cpu_threads {
                let (raw_rx, done_tx) = (raw_rx.clone(), done_tx.clone());
                scope.spawn(move || {
                    for raw in raw_rx {
                        let _ = done_tx.send(process_preview(raw));
                    }
                });
            }
            drop(done_tx);

            // Whatever order the CPU tier finishes in, photos appear in read order.
            let mut waiting: BTreeMap<usize, FinishedPreview> = BTreeMap::new();
            let mut next = 0;
            for finished in done_rx {
                waiting.insert(finished.sequence, finished);
                while let Some(finished) = waiting.remove(&next) {
                    next += 1;
                    on_each(self.publish(finished));
                }
            }
        });
    }

    /// Stores a finished preview with a brief lock, and says so.
    fn publish(&self, finished: FinishedPreview) -> IngestMessage {
        let failed = finished.thumb.is_none() && finished.preview.is_none();
        {
            let mut scan = self.store.write().unwrap();
            if let Some(asset) = scan.assets.get_mut(&finished.key) {
                asset.image_data.thumb = finished.thumb;
                asset.image_data.preview = finished.preview;
                if !finished.errors.is_empty() {
                    asset
                        .image_data
                        .errors
                        .get_or_insert_with(Vec::new)
                        .extend(finished.errors);
                }
            }
        }
        IngestMessage {
            key: finished.key,
            error: failed,
        }
    }
}

fn process_preview(raw: RawPreview) -> FinishedPreview {
    let mut errors: Vec<ImageProcessingError> = vec![];
    let (thumb, preview) = match raw.bytes {
        Err(error) => {
            errors.push(error);
            (None, None)
        }
        Ok(jpeg) => {
            let thumb = make_thumbnail_from_jpeg_bytes(&jpeg)
                .map_err(|e| errors.push(format!("thumbnail generation failed: {e}").into()))
                .ok();
            let preview = make_preview_from_jpeg_bytes(&jpeg)
                .map_err(|e| errors.push(format!("preview generation failed: {e}").into()))
                .ok();
            (thumb, preview)
        }
    };
    FinishedPreview {
        sequence: raw.sequence,
        key: raw.key,
        thumb,
        preview,
        errors,
    }
}

/// Runs [`upload_copies`] on its own runtime and blocks until it is done. Call it off
/// the UI thread.
pub fn upload_blocking(
    target: Arc<dyn ReplicationTarget>,
    files: Vec<UploadFile>,
    progress: Option<crossbeam_channel::Sender<ReplicationEvent>>,
) -> ReplicationReport {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(upload_copies(target, files, progress)),
        Err(err) => {
            let mut report = ReplicationReport::default();
            report.failures.push(ReplicationFailure {
                source: None,
                target: None,
                error: format!("could not start the upload runtime: {err}").into(),
            });
            report
        }
    }
}

/// Acquire the JPEG bytes that feed the preview pipeline for one asset, per its
/// read stage: a RAW seek-reads its embedded-preview strip; a JPEG reads the
/// file. A RAW whose format hides its preview from that fast path, such as CR3 or
/// RAF, goes through the RAW decoder's own preview extraction, which reads the
/// whole file. Lock-free and self-contained, so it is unit-testable without threads.
fn read_source_bytes(
    kind: FileKind,
    path: &Path,
    strip: Option<PreviewStrip>,
) -> Result<Vec<u8>, ImageProcessingError> {
    match kind {
        FileKind::Raw => {
            if let Some(strip) = strip {
                let bytes = read_embedded_preview(path, strip)?;
                if bytes.starts_with(&[0xFF, 0xD8]) {
                    return Ok(bytes);
                }
            }
            decoder_preview_jpeg(path)
        }
        FileKind::Jpeg => std::fs::read(path).map_err(ImageProcessingError::from),
    }
}

/// The largest image embedded in a RAW, as found by the decoder, encoded as a JPEG.
/// Never decodes the sensor data.
fn decoder_preview_jpeg(path: &Path) -> Result<Vec<u8>, ImageProcessingError> {
    let source = rawler::rawsource::RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source)?;
    let params = rawler::decoders::RawDecodeParams::default();
    let image = match decoder.full_image(&source, &params)? {
        Some(image) => Some(image),
        None => match decoder.preview_image(&source, &params)? {
            Some(image) => Some(image),
            None => decoder.thumbnail_image(&source, &params)?,
        },
    }
    .ok_or_else(|| format!("{} has no embedded preview", path.display()))?;

    let rgb = image.to_rgb8();
    let (width, height) = (rgb.width() as usize, rgb.height() as usize);
    compress(turbojpeg::Image {
        pixels: rgb.into_raw(),
        width,
        pitch: width * 3,
        height,
        format: turbojpeg::PixelFormat::RGB,
    })
    .map_err(|err| format!("could not encode preview: {err}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileInfo, ImportOptions, LocalTarget, Target, TargetKind, TriageState, VerifiedReplica,
        plan_import,
    };

    #[test]
    fn default_config_has_phase2_locked_values() {
        let c = EngineConfig::default();
        assert_eq!(c.preview_long_edge, 1920);
        assert_eq!(c.thumb_long_edge, 512);
        assert_eq!(c.jpeg_quality, 85);
        assert!(c.cpu_threads >= 1);
    }

    /// Wraps a real target and holds every store until `gate` opens.
    struct StalledTarget {
        inner: LocalTarget,
        gate: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl ReplicationTarget for StalledTarget {
        fn id(&self) -> TargetId {
            self.inner.id()
        }

        fn max_concurrent_writes(&self) -> usize {
            1
        }

        async fn store(
            &self,
            file: &FileInfo,
            bytes: Arc<[u8]>,
        ) -> Result<VerifiedReplica, Box<dyn std::error::Error + Send + Sync>> {
            let _open = self.gate.acquire().await?;
            self.inner.store(file, bytes).await
        }
    }

    #[test]
    fn triage_and_previews_finish_while_the_copy_is_stalled() {
        let tmp = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/test_exif_read");
        let mut engine = Engine::new(EngineConfig {
            card_read_permits: 1,
            ..Default::default()
        });
        engine.scan(&source);
        let total_assets = engine.scan_response.read().unwrap().assets.len();
        assert!(total_assets > 0);

        let plan = plan_import(
            &engine.scan_response,
            &source,
            &ImportOptions::default(),
            None,
        )
        .unwrap();
        let file_count = plan.files.len();
        let target = Arc::new(StalledTarget {
            inner: LocalTarget::new(
                TargetId(1),
                Target {
                    root: tmp.path().to_path_buf(),
                    kind: TargetKind::LocalNvme,
                    write_permits: 1,
                },
            ),
            gate: tokio::sync::Semaphore::new(0),
        });
        let import = engine.start_import(plan, target.clone(), vec![], None);

        // No file can reach the target, yet every preview still arrives.
        let previews = engine.ingest();
        let mut arrived = 0;
        while previews
            .recv_timeout(std::time::Duration::from_secs(60))
            .is_ok()
        {
            arrived += 1;
        }
        assert_eq!(arrived, total_assets);
        assert_eq!(import.progress.try_iter().count(), 0);

        // The store is free for a rating while files are still in flight.
        {
            let mut store = engine.scan_response.write().unwrap();
            let asset = store.assets.values_mut().next().unwrap();
            asset.rating = 5;
            asset.state = TriageState::Accepted;
        }

        target.gate.add_permits(file_count);
        let report = import.wait();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), file_count);
    }

    #[test]
    fn history_only_records_files_on_every_target() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("IMG_1.JPG"), b"not really a jpeg").unwrap();

        let mut engine = Engine::new(EngineConfig::default());
        engine.scan(&card);
        let options = ImportOptions::default();
        let history_path = tmp.path().join("history.jsonl");
        let target_at = |id, root: PathBuf| -> Arc<dyn ReplicationTarget> {
            Arc::new(LocalTarget::new(
                TargetId(id),
                Target {
                    root,
                    kind: TargetKind::LocalNvme,
                    write_permits: 1,
                },
            ))
        };

        // A file where the secondary root should be makes that target fail.
        let broken = tmp.path().join("broken");
        std::fs::write(&broken, b"").unwrap();
        let history = ImportHistory::open(history_path.clone()).unwrap();
        let plan = plan_import(&engine.scan_response, &card, &options, Some(&history)).unwrap();
        let report = engine
            .start_import(
                plan,
                target_at(1, tmp.path().join("a")),
                vec![target_at(2, broken)],
                Some(history),
            )
            .wait();
        assert_eq!(report.failures.len(), 1);

        let history = ImportHistory::open(history_path.clone()).unwrap();
        let plan = plan_import(&engine.scan_response, &card, &options, Some(&history)).unwrap();
        assert_eq!(plan.files.len(), 1, "a retry must still see the file");
        let report = engine
            .start_import(
                plan,
                target_at(1, tmp.path().join("a")),
                vec![target_at(2, tmp.path().join("b"))],
                Some(history),
            )
            .wait();
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        let history = ImportHistory::open(history_path).unwrap();
        let plan = plan_import(&engine.scan_response, &card, &options, Some(&history)).unwrap();
        assert!(plan.files.is_empty());
        assert_eq!(plan.skipped_duplicates.len(), 1);
    }

    #[test]
    fn decoder_fallback_produces_a_usable_preview() {
        let raw =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/test_exif_read/IMG_1939.CR2");

        let jpeg = decoder_preview_jpeg(&raw).unwrap();

        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(make_thumbnail_from_jpeg_bytes(&jpeg).is_ok());
    }

    #[test]
    fn raw_that_is_not_an_image_records_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("IMG_1.NEF");
        std::fs::write(&fake, b"not a raw file at all").unwrap();

        assert!(read_source_bytes(FileKind::Raw, &fake, None).is_err());
    }

    fn keys_of(engine: &Engine) -> Vec<AssetKey> {
        let mut keys: Vec<AssetKey> = engine
            .scan_response
            .read()
            .unwrap()
            .assets
            .keys()
            .cloned()
            .collect();
        keys.sort_by(|a, b| a.stem.cmp(&b.stem));
        keys
    }

    /// A card of `count` real RAW files: hard links, so it costs no disk space.
    fn linked_card(dir: &Path, count: usize) {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/test_exif_read/IMG_1939.CR2");
        std::fs::create_dir_all(dir).unwrap();
        for i in 0..count {
            let link = dir.join(format!("IMG_{i:04}.CR2"));
            std::fs::hard_link(&fixture, &link)
                .or_else(|_| std::fs::copy(&fixture, &link).map(|_| ()))
                .unwrap();
        }
    }

    #[test]
    fn scanning_names_opens_no_photo() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/test_exif_read");
        let mut engine = Engine::new(EngineConfig::default());
        engine.scan_names(&source, &ScanOptions::default());
        let scan = engine.scan_response.read().unwrap();
        assert_eq!(scan.assets.len(), 3);
        assert!(
            scan.assets
                .values()
                .all(|a| a.exif_data.captured_at.is_none())
        );
        assert_eq!(scan.file_meta.len(), 4);
    }

    #[test]
    fn every_thumbnail_arrives_before_any_preview_each_in_display_order() {
        let tmp = tempfile::tempdir().unwrap();
        linked_card(tmp.path(), 12);
        let mut engine = Engine::new(EngineConfig::default());
        engine.scan_names(tmp.path(), &ScanOptions::default());
        let order = keys_of(&engine);

        let events: Vec<LoadEvent> = engine
            .start_loading(order.clone(), Arc::new(AtomicUsize::new(0)))
            .iter()
            .collect();

        let headers: Vec<&AssetKey> = events
            .iter()
            .filter_map(|event| match event {
                LoadEvent::Header { key, .. } => Some(key),
                _ => None,
            })
            .collect();
        let previews: Vec<&AssetKey> = events
            .iter()
            .filter_map(|event| match event {
                LoadEvent::Preview(message) => Some(&message.key),
                _ => None,
            })
            .collect();
        assert_eq!(headers, order.iter().collect::<Vec<_>>());
        assert_eq!(previews, order.iter().collect::<Vec<_>>());
        let last_header = events
            .iter()
            .rposition(|event| matches!(event, LoadEvent::Header { .. }))
            .unwrap();
        let first_preview = events
            .iter()
            .position(|event| matches!(event, LoadEvent::Preview(_)))
            .unwrap();
        assert!(last_header < first_preview);
        assert!(matches!(events.last(), Some(LoadEvent::PreviewsDone)));
    }

    #[test]
    fn jumping_ahead_pulls_those_photos_forward_and_loses_none() {
        let mut work = OrderedWork::new(30);
        assert_eq!(work.next(0), Some((0, 0)));
        assert_eq!(work.next(1), Some((1, 1)));
        // The photographer jumps to 20: the window goes first.
        let jumped: Vec<usize> = (0..FOCUS_WINDOW.min(10))
            .map(|_| work.next(20).unwrap().1)
            .collect();
        assert_eq!(jumped, (20..30).collect::<Vec<_>>());
        // With the window done, the rest continue in order, each exactly once.
        let rest: Vec<usize> = std::iter::from_fn(|| work.next(20).map(|(_, p)| p)).collect();
        assert_eq!(rest, (2..20).collect::<Vec<_>>());
        assert_eq!(work.next(0), None);
    }

    #[test]
    fn thumbnails_reach_the_first_screen_quickly_on_a_slow_card_while_copying() {
        // 20 MB/s, one channel: a real SD card. This took about a minute before the
        // loader went in display order and the copy gave way to it.
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        linked_card(&card, 60);
        let mut engine = Engine::new(EngineConfig {
            simulated_card_speed: Some(20_000_000),
            ..Default::default()
        });
        engine.scan_names(&card, &ScanOptions::default());
        let options = ImportOptions {
            use_file_dates: true,
            ..Default::default()
        };
        let plan = plan_import(&engine.scan_response, &card, &options, None).unwrap();
        let target = Arc::new(LocalTarget::new(
            TargetId(1),
            Target {
                root: tmp.path().join("photos"),
                kind: TargetKind::LocalNvme,
                write_permits: 4,
            },
        ));
        let session = engine.start_session(target, vec![], None, vec![]);
        for file in plan.files {
            session.queue.push(file);
        }
        let order = keys_of(&engine);
        let started = std::time::Instant::now();
        let events = engine.start_loading(order.clone(), Arc::new(AtomicUsize::new(0)));

        let mut seen = vec![];
        while seen.len() < 24 {
            match events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
            {
                LoadEvent::Header { key, .. } => seen.push(key),
                other => panic!("{other:?}"),
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(seen, order[..24]);
        assert!(elapsed.as_secs_f64() < 2.0, "{elapsed:?}");
        session.queue.stop();
        session.wait();
    }

    #[test]
    fn a_session_records_history_file_by_file_while_still_open() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("IMG_1.JPG"), b"first").unwrap();
        let mut engine = Engine::new(EngineConfig::default());
        engine.scan_names(&card, &ScanOptions::default());
        let options = ImportOptions {
            use_file_dates: true,
            ..Default::default()
        };
        let history_path = tmp.path().join("history.jsonl");
        let plan = plan_import(&engine.scan_response, &card, &options, None).unwrap();
        let target = Arc::new(LocalTarget::new(
            TargetId(1),
            Target {
                root: tmp.path().join("photos"),
                kind: TargetKind::LocalNvme,
                write_permits: 1,
            },
        ));
        let session = engine.start_session(
            target,
            vec![],
            Some(ImportHistory::open(history_path.clone()).unwrap()),
            vec![],
        );
        session.queue.push(plan.files[0].clone());

        let event = session
            .progress
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(matches!(event, ReplicationEvent::Verified { .. }));

        // Not closed yet, and a crash now wouldn't lose the record.
        let on_disk = ImportHistory::open(history_path).unwrap();
        assert!(on_disk.contains(&plan.files[0].duplicate_key));

        session.queue.close();
        let report = session.wait();
        assert_eq!(report.verified.len(), 1);
    }

    #[test]
    fn config_override_keeps_other_defaults() {
        let c = EngineConfig {
            preview_long_edge: 2560,
            ..Default::default()
        };
        assert_eq!(c.preview_long_edge, 2560);
        assert_eq!(c.thumb_long_edge, 512);
    }
}
