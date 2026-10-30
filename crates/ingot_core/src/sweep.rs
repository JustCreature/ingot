//! The deletion sweep: moves rejected photos' copies to the Trash.
//!
//! Each step needs a [`VerifiedReplica`], so a path that wasn't proven to be a copy
//! can't get in. Source files are out of scope and are refused by comparison with
//! the proof. Cloud copies are left alone: Ingot has no delete call for Drive.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::{ReplicaLocation, ScanResponse, TriageState, VerifiedReplica};

type SweepError = Box<dyn std::error::Error + Send + Sync>;

/// What a sweep would do. Showing this to the user first is the dry run.
#[derive(Debug, Default)]
pub struct SweepPlan<'a> {
    /// Copies that will go to the Trash.
    pub to_trash: Vec<&'a VerifiedReplica>,
    /// Copies on Google Drive, which stay.
    pub left_in_cloud: usize,
}

impl SweepPlan<'_> {
    pub fn paths(&self) -> Vec<&Path> {
        self.to_trash
            .iter()
            .filter_map(|replica| match replica.location() {
                ReplicaLocation::Local(path) => Some(path.as_path()),
                ReplicaLocation::Remote(_) => None,
            })
            .collect()
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Picks the copies of every rejected photo. A "copy" that is really the source,
/// from importing a folder into itself, is left out.
pub fn plan_sweep<'a>(
    scan: &RwLock<ScanResponse>,
    replicas: &'a [VerifiedReplica],
) -> Result<SweepPlan<'a>, SweepError> {
    let rejected: HashSet<PathBuf> = {
        let scan = scan
            .read()
            .map_err(|err| format!("scan store lock poisoned: {err}"))?;
        scan.assets
            .values()
            .filter(|asset| asset.state == TriageState::Rejected)
            .flat_map(|asset| asset.files.get_all().iter().map(|(_, path)| path.clone()))
            .collect()
    };

    let mut plan = SweepPlan::default();
    for replica in replicas {
        if !rejected.contains(replica.source()) {
            continue;
        }
        match replica.location() {
            ReplicaLocation::Remote(_) => plan.left_in_cloud += 1,
            ReplicaLocation::Local(path) => {
                if !same_file(path, replica.source()) {
                    plan.to_trash.push(replica);
                }
            }
        }
    }
    Ok(plan)
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub trashed: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

/// Runs a plan. A file that no longer matches its proof was changed after the copy,
/// so it is kept and reported instead of trashed.
pub fn execute_sweep(plan: &SweepPlan<'_>) -> SweepReport {
    let mut report = SweepReport::default();
    for replica in &plan.to_trash {
        let ReplicaLocation::Local(path) = replica.location() else {
            continue;
        };
        let outcome: Result<(), String> = (|| {
            let observed = blake3::Hasher::new()
                .update_mmap(path)
                .map_err(|err| err.to_string())?
                .finalize();
            if observed != replica.hash() {
                return Err("changed since it was copied, so it was kept".to_string());
            }
            trash::delete(path).map_err(|err| err.to_string())
        })();
        match outcome {
            Ok(()) => report.trashed.push(path.clone()),
            Err(error) => report.failures.push((path.clone(), error)),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        CardGate, CopyEngine, Engine, EngineConfig, ImportOptions, LocalTarget, ReplicationReport,
        Target, TargetId, TargetKind, plan_import,
    };

    async fn imported(card: &Path, dest: &Path) -> (Engine, ReplicationReport) {
        let mut engine = Engine::new(EngineConfig::default());
        engine.scan(card);
        let plan =
            plan_import(&engine.scan_response, card, &ImportOptions::default(), None).unwrap();
        let target = Arc::new(LocalTarget::new(
            TargetId(1),
            Target {
                root: dest.to_path_buf(),
                kind: TargetKind::LocalNvme,
                write_permits: 2,
            },
        ));
        let report = CopyEngine::new(CardGate::new(2), 4, target)
            .replicate(plan)
            .await;
        (engine, report)
    }

    fn write_card(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("IMG_1.JPG"), b"keep me").unwrap();
        fs::write(dir.join("IMG_2.JPG"), b"reject me").unwrap();
        fs::write(dir.join("IMG_3.JPG"), b"also keep").unwrap();
    }

    fn reject(engine: &Engine, stem: &str) {
        let mut scan = engine.scan_response.write().unwrap();
        let asset = scan
            .assets
            .iter_mut()
            .find(|(key, _)| key.stem == stem)
            .unwrap()
            .1;
        asset.state = TriageState::Rejected;
    }

    #[tokio::test]
    async fn only_rejected_copies_are_planned_and_the_source_is_never_included() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (engine, report) = imported(&card, &dest).await;
        reject(&engine, "img_2");

        let plan = plan_sweep(&engine.scan_response, &report.verified).unwrap();

        assert_eq!(plan.paths(), [dest.join("IMG_2.JPG").as_path()]);
    }

    #[tokio::test]
    async fn nothing_rejected_means_nothing_planned() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (engine, report) = imported(&card, &dest).await;

        let plan = plan_sweep(&engine.scan_response, &report.verified).unwrap();

        assert!(plan.to_trash.is_empty());
    }

    #[tokio::test]
    async fn importing_a_folder_into_itself_cannot_trash_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("photos");
        write_card(&card);
        let (engine, report) = imported(&card, &card).await;
        assert_eq!(report.verified.len(), 3);
        reject(&engine, "img_2");

        let plan = plan_sweep(&engine.scan_response, &report.verified).unwrap();

        assert!(plan.to_trash.is_empty());
    }

    #[tokio::test]
    async fn a_copy_changed_after_import_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (engine, report) = imported(&card, &dest).await;
        reject(&engine, "img_2");
        let copy = dest.join("IMG_2.JPG");
        fs::write(&copy, b"edited by hand afterwards").unwrap();

        let plan = plan_sweep(&engine.scan_response, &report.verified).unwrap();
        let outcome = execute_sweep(&plan);

        assert!(outcome.trashed.is_empty());
        assert_eq!(outcome.failures.len(), 1);
        assert!(copy.exists());
    }
}
