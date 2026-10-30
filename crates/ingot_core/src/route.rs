//! Storage target configuration. Where files land below a target's root is decided
//! by the import plan.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum TargetKind {
    LocalNvme,
    LocalSpinning,
    Network,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub root: PathBuf,
    pub kind: TargetKind,
    pub write_permits: usize,
}
