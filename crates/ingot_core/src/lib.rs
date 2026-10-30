//! ingot_core — the RAW+JPEG ingestion & triage engine.
//!
//! The app drives this crate through [`Engine`] / [`EngineConfig`]; the lower
//! modules (`scan`, `metadata`, `preview`, `route`) are the engine's internals,
//! re-exported here only where they form part of the public surface.

mod asset;
mod cluster;
mod engine;
mod import;
mod metadata;
pub mod preview;
mod replicate;
mod route;
mod scan;
mod sweep;
mod triage;

#[cfg(test)]
mod test_support;

pub use asset::{
    AssetFiles, AssetKey, ExifAssetData, FileKind, PhotoAsset, PreviewStrip, TriageState,
};
pub use cluster::{
    ClusterItem, DEFAULT_SENSITIVITY, Fingerprint, MAX_GROUP_GAP_SECONDS, cluster_any_order,
    cluster_ranges, looks_alike,
};
pub use engine::{Engine, EngineConfig, ImportHandle, IngestEvent};
pub use import::{
    DateFolders, DuplicateKey, ExtensionCase, ImportHistory, ImportOptions, ImportPlan,
    NameTemplate, Organize, PlannedFile, Selection, plan_import, planned_relative,
};
pub use metadata::{enrich_assets, get_embedded_preview_location, read_embedded_preview};
pub use replicate::{
    CardGate, CardPermit, CopyEngine, DRIVE_ROOT_FOLDER, DriveConfig, DriveCredentials,
    DriveTarget, FileInfo, ImportQueue, LocalTarget, ReplicaLocation, ReplicationEvent,
    ReplicationFailure, ReplicationReport, ReplicationTarget, TargetId, VerifiedReplica,
    drive_login,
};
pub use route::{Target, TargetKind};
pub use scan::{Collision, FileMeta, ScanOptions, ScanResponse, scan_source, scan_source_dir};
pub use sweep::{SweepPlan, SweepReport, execute_sweep, plan_sweep};
pub use triage::{
    SidecarOutcome, TriageKey, TriageRecord, TriageStore, sidecar_path, write_sidecar,
};
