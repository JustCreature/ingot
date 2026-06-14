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

        // let mut writable_scan_response = scan_response.write().unwrap();
        work_items
            .into_par_iter()
            .for_each(|(key, kind, path, preview_location_strip)| {
                let src_jpeg_bytes: Result<Vec<u8>, ImageProcessingError> = match (kind, path) {
                    (FileKind::Raw, path) => match preview_location_strip {
                        Some(strip) => read_embedded_preview(&path, strip),
                        None => Err(ImageProcessingError::from(
                            "no embedded preview strip with jpeg bytes offset, limit was found",
                        )),
                    },
                    (FileKind::Jpeg, path) => {
                        std::fs::read(path).map_err(ImageProcessingError::from)
                    }
                };

                match src_jpeg_bytes {
                    Err(e) => {
                        let mut writable_scan_response = scan_response.write().unwrap();
                        let Some(writable_asset) = writable_scan_response.assets.get_mut(&key)
                        else {
                            return;
                        };
                        writable_asset
                            .image_data
                            .errors
                            .get_or_insert_with(Vec::new)
                            .push(e);
                        drop(writable_scan_response);
                        tx.send(IngestEvent::Preview(IngestMessage { key, error: true }))
                            .expect("error sending message to the channel");
                    }
                    Ok(jpeg) => {
                        let mut error_thumb: bool = false;
                        let mut error_preview: bool = false;
                        let thumb = make_thumbnail_from_jpeg_bytes(&jpeg);
                        let preview = make_preview_from_jpeg_bytes(&jpeg);

                        let mut writable_scan_response = scan_response.write().unwrap();
                        let Some(writable_asset) = writable_scan_response.assets.get_mut(&key)
                        else {
                            return;
                        };
                        if thumb.is_none() {
                            writable_asset
                                .image_data
                                .errors
                                .get_or_insert_with(Vec::new)
                                .push(ImageProcessingError::from("error generating thumbnail"));
                            error_thumb = true;
                        } else {
                            writable_asset.image_data.thumb = thumb
                        }

                        if preview.is_none() {
                            writable_asset
                                .image_data
                                .errors
                                .get_or_insert_with(Vec::new)
                                .push(ImageProcessingError::from("error generating preview"));
                            error_preview = true;
                        } else {
                            writable_asset.image_data.preview = preview
                        }
                        drop(writable_scan_response);

                        tx.send(IngestEvent::Preview(IngestMessage {
                            key,
                            error: error_thumb && error_preview,
                        }))
                        .expect("error sending message to the channel");
                    }
                };
            });
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
