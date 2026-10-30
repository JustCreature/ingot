//! The ingest engine: a configurable, instantiable handle the app constructs
//! once and drives. It owns the cross-cutting policy (preview/thumb resolution,
//! JPEG quality) and — once step 4 lands — the shared concurrency resources
//! (a source-read semaphore sized to `card_read_permits`, a CPU pool sized to
//! `cpu_threads`). Those resources are shared across all in-flight assets, so
//! they must live on an instance, not in free functions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rayon::iter::{IntoParallelIterator, ParallelBridge, ParallelIterator};

use crate::asset::{AssetKey, ImageProcessingError};
use crate::import::{DuplicateKey, ImportHistory, ImportPlan};
use crate::preview::{make_preview_from_jpeg_bytes, make_thumbnail_from_jpeg_bytes};
use crate::replicate::{
    CardGate, CopyEngine, ReplicationEvent, ReplicationFailure, ReplicationReport,
    ReplicationTarget,
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
        let card = CardGate::new(config.card_read_permits);
        Engine {
            card,
            config,
            scan_response: Arc::new(RwLock::new(ScanResponse {
                assets: HashMap::new(),
                collisions: vec![],
                errors: vec![],
                ignored: vec![],
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
        let (progress_tx, progress) = crossbeam_channel::unbounded();
        let target_count = secondaries.len() + 1;
        let mut copy = CopyEngine::new(
            Arc::clone(&self.card),
            self.config.read_channel_depth,
            primary,
        )
        .with_progress(progress_tx);
        for secondary in secondaries {
            copy = copy.with_secondary(secondary);
        }

        let finished = std::thread::spawn(move || {
            let keys: HashMap<PathBuf, DuplicateKey> = plan
                .files
                .iter()
                .map(|file| (file.source.clone(), file.duplicate_key.clone()))
                .collect();
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
            let mut report = runtime.block_on(copy.replicate(plan));

            if let Some(mut history) = history {
                let mut copies: HashMap<&Path, usize> = HashMap::new();
                for replica in &report.verified {
                    *copies.entry(replica.source()).or_default() += 1;
                }
                let done: Vec<DuplicateKey> = copies
                    .into_iter()
                    .filter(|(_, count)| *count == target_count)
                    .filter_map(|(source, _)| keys.get(source).cloned())
                    .collect();
                if let Err(error) = history.record(done) {
                    report.failures.push(ReplicationFailure {
                        source: None,
                        target: None,
                        error: format!("could not update the import history: {error}").into(),
                    });
                }
            }
            report
        });

        ImportHandle { progress, finished }
    }

    pub fn ingest(&mut self) -> crossbeam_channel::Receiver<IngestEvent> {
        let (tx, rx): (
            crossbeam_channel::Sender<IngestEvent>,
            crossbeam_channel::Receiver<IngestEvent>,
        ) = crossbeam_channel::bounded(64);

        let cloned_scan_response = Arc::clone(&self.scan_response);
        let read_threads = self.config.card_read_permits.max(1);
        let cpu_threads = self.config.cpu_threads.max(1);

        let card = Arc::clone(&self.card);

        std::thread::spawn(move || {
            Engine::run_processing_worker(cloned_scan_response, tx, card, read_threads, cpu_threads)
        });

        rx
    }

    /// Two decoupled tiers connected by a bounded channel: a **reader tier**
    /// (`read_threads`) that does nothing but pull source bytes off the card, and
    /// a **CPU tier** (`cpu_threads`) that decodes/encodes. Sizing them
    /// separately is the whole point — the card's optimal read concurrency
    /// (measured ~4, device-dependent) is unrelated to the core count the encode
    /// wants. The bounded channel between them is the backpressure / memory
    /// governor (only ~`cpu_threads` raw buffers alive at once).
    fn run_processing_worker(
        scan_response: Arc<RwLock<ScanResponse>>,
        tx: crossbeam_channel::Sender<IngestEvent>,
        card: Arc<CardGate>,
        read_threads: usize,
        cpu_threads: usize,
    ) {
        // Snapshot the per-asset inputs under a brief read lock, then release it.
        // Everything heavy below — the file read and both encodes — runs lock-free.
        let work_items: Vec<(AssetKey, FileKind, PathBuf, Option<PreviewStrip>)> = {
            let readable = scan_response.read().unwrap();
            readable
                .assets
                .iter()
                .map(|(key, asset)| {
                    let (kind, path) = asset.files.get_prioritized();
                    (
                        key.clone(),
                        kind,
                        path.to_path_buf(),
                        asset.exif_data.embedded_preview_file_location,
                    )
                })
                .collect()
        };

        // Raw bytes flow reader -> CPU over this bounded channel. Cap ≈ cpu_threads
        // keeps every CPU worker fed without holding more than ~that many multi-MB
        // buffers resident.
        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<(
            AssetKey,
            Result<Vec<u8>, ImageProcessingError>,
        )>(cpu_threads * 10);

        let read_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(read_threads)
            .build()
            .expect("failed to build reader pool");
        let cpu_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(cpu_threads)
            .build()
            .expect("failed to build cpu pool");

        std::thread::scope(|s| {
            // READER TIER: `read_threads`-wide, does only I/O. Dropping `raw_tx`
            // when the pool drains closes the channel and ends the CPU stage.
            s.spawn(move || {
                read_pool.install(|| {
                    work_items
                        .into_par_iter()
                        .for_each(|(key, kind, path, strip)| {
                            // The slot covers only the read, not the wait on the channel.
                            let permit = card.acquire_priority();
                            let bytes = read_source_bytes(kind, &path, strip);
                            drop(permit);
                            let _ = raw_tx.send((key, bytes));
                        });
                });
            });

            // CPU TIER: `cpu_threads`-wide, decode/encode lock-free, then a brief
            // per-asset write lock to store, then notify. `par_bridge` adapts the
            // blocking channel receiver to the cpu pool.
            cpu_pool.install(|| {
                raw_rx.into_iter().par_bridge().for_each(|(key, bytes)| {
                    let (thumb, preview, errors): (
                        Option<Vec<u8>>,
                        Option<Vec<u8>>,
                        Vec<ImageProcessingError>,
                    ) = match bytes {
                        Err(e) => (None, None, vec![e]),
                        Ok(jpeg) => {
                            let mut errors = Vec::new();
                            let thumb = match make_thumbnail_from_jpeg_bytes(&jpeg) {
                                Ok(t) => Some(t),
                                Err(e) => {
                                    errors.push(format!("thumbnail generation failed: {e}").into());
                                    None
                                }
                            };
                            let preview = match make_preview_from_jpeg_bytes(&jpeg) {
                                Ok(p) => Some(p),
                                Err(e) => {
                                    errors.push(format!("preview generation failed: {e}").into());
                                    None
                                }
                            };
                            (thumb, preview, errors)
                        }
                    };
                    let failed = thumb.is_none() && preview.is_none();

                    {
                        let mut writable = scan_response.write().unwrap();
                        let Some(asset) = writable.assets.get_mut(&key) else {
                            return;
                        };
                        asset.image_data.thumb = thumb;
                        asset.image_data.preview = preview;
                        if !errors.is_empty() {
                            asset
                                .image_data
                                .errors
                                .get_or_insert_with(Vec::new)
                                .extend(errors);
                        }
                    }

                    let _ = tx.send(IngestEvent::Preview(IngestMessage { key, error: failed }));
                });
            });
        });
    }
}

/// Acquire the JPEG bytes that feed the preview pipeline for one asset, per its
/// read stage: a RAW seek-reads its embedded-preview strip; a JPEG reads the
/// file. Lock-free and self-contained, so it is unit-testable without threads.
fn read_source_bytes(
    kind: FileKind,
    path: &Path,
    strip: Option<PreviewStrip>,
) -> Result<Vec<u8>, ImageProcessingError> {
    match kind {
        FileKind::Raw => strip
            .ok_or_else(|| {
                ImageProcessingError::from("no embedded preview location found for RAW asset")
            })
            .and_then(|strip| read_embedded_preview(path, strip)),
        FileKind::Jpeg => std::fs::read(path).map_err(ImageProcessingError::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileInfo, ImportOptions, LocalTarget, Target, TargetId, TargetKind, TriageState,
        VerifiedReplica, plan_import,
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
    fn config_override_keeps_other_defaults() {
        let c = EngineConfig {
            preview_long_edge: 2560,
            ..Default::default()
        };
        assert_eq!(c.preview_long_edge, 2560);
        assert_eq!(c.thumb_long_edge, 512);
    }
}
