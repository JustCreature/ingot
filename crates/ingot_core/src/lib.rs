//! ingot_core — the RAW+JPEG ingestion & triage engine.
//!
//! The app drives this crate through [`Engine`] / [`EngineConfig`]; the lower
//! modules (`scan`, `metadata`, `preview`, `route`) are the engine's internals,
//! re-exported here only where they form part of the public surface.

mod asset;
mod engine;
mod import;
mod metadata;
pub mod preview;
mod replicate;
mod route;
mod scan;

#[cfg(test)]
mod test_support;

pub use asset::{
    AssetFiles, AssetKey, ExifAssetData, FileKind, PhotoAsset, PreviewStrip, TriageState,
};
pub use engine::{Engine, EngineConfig, ImportHandle, IngestEvent};
pub use import::{
    DateFolders, DuplicateKey, ExtensionCase, ImportHistory, ImportOptions, ImportPlan,
    NameTemplate, Organize, PlannedFile, Selection, plan_import,
};
pub use metadata::{enrich_assets, get_embedded_preview_location, read_embedded_preview};
pub use replicate::{
    CardGate, CardPermit, CopyEngine, FileInfo, LocalTarget, ReplicaLocation, ReplicationEvent,
    ReplicationFailure, ReplicationReport, ReplicationTarget, TargetId, VerifiedReplica,
};
pub use route::{Target, TargetKind};
pub use scan::{Collision, ScanOptions, ScanResponse, scan_source, scan_source_dir};
