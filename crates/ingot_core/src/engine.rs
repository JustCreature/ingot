//! The ingest engine: a configurable, instantiable handle the app constructs
//! once and drives. It owns the cross-cutting policy (preview/thumb resolution,
//! JPEG quality) and — once step 4 lands — the shared concurrency resources
//! (a source-read semaphore sized to `card_read_permits`, a CPU pool sized to
//! `cpu_threads`). Those resources are shared across all in-flight assets, so
//! they must live on an instance, not in free functions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rayon::iter::{IntoParallelIterator, ParallelIterator};

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
    /// Source-card concurrent reads. A *device property*: 1-2 for a camera
    /// card (serial), higher for SSD/NVMe (queue depth). See Phase 2 step 1.
    pub card_read_permits: usize,
    /// CPU worker threads for the preview pipeline. Defaults to the machine's
    /// available parallelism.
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

        std::thread::spawn(move || Engine::run_processing_worker(cloned_scan_response, tx));

        rx
    }

    fn run_processing_worker(
        scan_response: Arc<RwLock<ScanResponse>>,
        tx: crossbeam_channel::Sender<IngestEvent>,
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

        work_items
            .into_par_iter()
            .for_each(|(key, kind, path, strip)| {
                // 1. Off-lock: read the source bytes and generate both outputs. A
                //    missing output stays `None` (its desired failed state) and is
                //    recorded as an error; a missing *source* fails both.
                let (thumb, preview, errors): (
                    Option<Vec<u8>>,
                    Option<Vec<u8>>,
                    Vec<ImageProcessingError>,
                ) = match read_source_bytes(kind, &path, strip) {
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

                // 2. Brief write lock: store the results into the engine-owned asset.
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

                // 3. Notify (lock already released). A dropped receiver means the
                //    consumer is gone — stop quietly rather than panic.
                let _ = tx.send(IngestEvent::Preview(IngestMessage { key, error: failed }));
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
