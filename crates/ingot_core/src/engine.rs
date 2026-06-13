//! The ingest engine: a configurable, instantiable handle the app constructs
//! once and drives. It owns the cross-cutting policy (preview/thumb resolution,
//! JPEG quality) and — once step 4 lands — the shared concurrency resources
//! (a source-read semaphore sized to `card_read_permits`, a CPU pool sized to
//! `cpu_threads`). Those resources are shared across all in-flight assets, so
//! they must live on an instance, not in free functions.

use std::path::Path;

use crate::asset::AssetKey;
use crate::scan::{ScanResponse, scan_source_dir};

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

/// What `ingest` streams back to the consumer, one per processed asset.
/// (Single-message-per-asset for v1; per-tier splitting is a later refinement.)
#[derive(Debug)]
pub struct ProcessedPreview {
    pub key: AssetKey,
    pub thumb_jpeg: Vec<u8>,
    pub preview_jpeg: Vec<u8>,
}

/// The engine instance. Cheap to construct; holds config now, concurrency
/// resources after step 4.
pub struct Engine {
    config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Engine { config }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Fast enumeration: walk + pair, no file-content reads. Gives the UI its
    /// grid skeleton immediately.
    pub fn scan(&self, source: &Path) -> ScanResponse {
        scan_source_dir(source)
    }

    // Step 4 (guided): `ingest(&self, source: &Path) -> IngestHandle`.
    // The per-asset processing unit (open -> parse EXIF -> preview-source
    // branch -> make_thumbnail + make_preview -> emit ProcessedPreview), the
    // two-tier concurrency (card_read_permits feeding cpu_threads), and the
    // channel land here.
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
