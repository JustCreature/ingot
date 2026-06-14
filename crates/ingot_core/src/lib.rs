//! ingot_core — the RAW+JPEG ingestion & triage engine.
//!
//! The app drives this crate through [`Engine`] / [`EngineConfig`]; the lower
//! modules (`scan`, `metadata`, `preview`, `route`) are the engine's internals,
//! re-exported here only where they form part of the public surface.

mod asset;
mod engine;
mod metadata;
pub mod preview;
mod route;
mod scan;

#[cfg(test)]
mod test_support;

pub use asset::{
    AssetFiles, AssetKey, ExifAssetData, FileKind, PhotoAsset, PreviewStrip, TriageState,
};
pub use engine::{Engine, EngineConfig, ProcessedPreview};
pub use metadata::{enrich_assets, get_embedded_preview_location, read_embedded_preview};
pub use route::{Target, TargetKind, build_destination_path};
pub use scan::{Collision, ScanResponse, scan_source_dir};
