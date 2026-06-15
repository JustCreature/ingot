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
use crate::preview::{make_preview_from_jpeg_bytes, make_thumbnail_from_jpeg_bytes};
use crate::scan::{ScanResponse, scan_source_dir};
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
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Engine {
            config,
            scan_response: Arc::new(RwLock::new(ScanResponse {
                assets: HashMap::new(),
                collisions: vec![],
                errors: vec![],
            })),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Fast enumeration: walk + pair, no file-content reads. Gives the UI its
    /// grid skeleton immediately.
    pub fn scan(&mut self, source: &Path) -> Arc<RwLock<ScanResponse>> {
        let response = scan_source_dir(source);
        let mut writable_scan_response = self.scan_response.write().unwrap();
        *writable_scan_response = response;
        enrich_assets(&mut writable_scan_response.assets);
        Arc::clone(&self.scan_response)
    }

    pub fn ingest(&mut self) -> crossbeam_channel::Receiver<IngestEvent> {
        let (tx, rx): (
            crossbeam_channel::Sender<IngestEvent>,
            crossbeam_channel::Receiver<IngestEvent>,
        ) = crossbeam_channel::bounded(64);

        let cloned_scan_response = Arc::clone(&self.scan_response);
        let read_threads = self.config.card_read_permits.max(1);
        let cpu_threads = self.config.cpu_threads.max(1);

        std::thread::spawn(move || {
            Engine::run_processing_worker(cloned_scan_response, tx, read_threads, cpu_threads)
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
        )>(cpu_threads);

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
                            let bytes = read_source_bytes(kind, &path, strip);
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

    #[test]
    fn default_config_has_phase2_locked_values() {
        let c = EngineConfig::default();
        assert_eq!(c.preview_long_edge, 1920);
        assert_eq!(c.thumb_long_edge, 512);
        assert_eq!(c.jpeg_quality, 85);
        assert!(c.cpu_threads >= 1);
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
