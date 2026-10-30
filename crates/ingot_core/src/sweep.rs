//! Taking back copies an import made: rejected photos through the sweep, and photos
//! not chosen at Finalize.
//!
//! This is the one place in Ingot that removes a file from a target, and it is gated
//! on proof. A file goes only with a [`VerifiedReplica`] saying this import newly wrote
//! it, only if its hash still matches, and only to the Trash. A file that was already
//! there, the source, a copy changed since, and anything on Google Drive all stay.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::{
    ReplicaLocation, ScanResponse, TriageState, VerifiedReplica, is_own_sidecar, sidecar_path,
};

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

/// Picks the copies of every rejected photo. A "copy" that is really the source, from
/// importing a folder into itself, or one that was already there, is left out.
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
                if replica.newly_written() && !same_file(path, replica.source()) {
                    plan.to_trash.push(replica);
                }
            }
        }
    }
    Ok(plan)
}

/// What Finalize will take back. Reads nothing from disk, so it can fill a dialog.
#[derive(Debug, Default)]
pub struct RemovalPlan<'a> {
    pub to_trash: Vec<&'a VerifiedReplica>,
    /// Not chosen, but already on the target before this import. Not ours to remove.
    pub already_there: Vec<&'a VerifiedReplica>,
}

impl RemovalPlan<'_> {
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

/// Picks the copies of files not in `keep`.
pub fn plan_removal<'a>(
    replicas: &'a [VerifiedReplica],
    keep: &HashSet<PathBuf>,
) -> RemovalPlan<'a> {
    let mut plan = RemovalPlan::default();
    for replica in replicas {
        if keep.contains(replica.source()) {
            continue;
        }
        if let ReplicaLocation::Local(path) = replica.location() {
            if replica.newly_written() && path != replica.source() {
                plan.to_trash.push(replica);
            } else {
                plan.already_there.push(replica);
            }
        }
    }
    plan
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub trashed: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
    /// Ingot's own sidecars that went with the last copy they described.
    pub sidecars_trashed: Vec<PathBuf>,
}

fn trash_replica(replica: &VerifiedReplica, report: &mut SweepReport) {
    let ReplicaLocation::Local(path) = replica.location() else {
        return;
    };
    let outcome: Result<(), String> = (|| {
        if same_file(path, replica.source()) {
            return Err("this is the source file, so it was kept".to_string());
        }
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

/// Trashes the sidecar of a removed copy, if Ingot wrote it and no photo with the same
/// name is left beside it to use it. A RAW and JPEG share one sidecar.
fn trash_orphan_sidecars(report: &mut SweepReport) {
    let mut seen = HashSet::new();
    for photo in report.trashed.clone() {
        let sidecar = sidecar_path(&photo);
        if !seen.insert(sidecar.clone()) || !is_own_sidecar(&sidecar) {
            continue;
        }
        let Some(dir) = photo.parent() else { continue };
        let still_used = fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let path = entry.path();
                path != sidecar
                    && path.file_stem() == sidecar.file_stem()
                    && path.extension().is_none_or(|extension| extension != "tmp")
            })
        });
        if still_used {
            continue;
        }
        match trash::delete(&sidecar) {
            Ok(()) => report.sidecars_trashed.push(sidecar),
            Err(err) => report.failures.push((sidecar, err.to_string())),
        }
    }
}

/// Runs a plan. A file that no longer matches its proof was changed after the copy,
/// so it is kept and reported instead of trashed.
pub fn execute_sweep(plan: &SweepPlan<'_>) -> SweepReport {
    let mut report = SweepReport::default();
    for replica in &plan.to_trash {
        trash_replica(replica, &mut report);
    }
    trash_orphan_sidecars(&mut report);
    report
}

/// Runs a Finalize plan, with the same checks as [`execute_sweep`].
pub fn execute_removal(plan: &RemovalPlan<'_>) -> SweepReport {
    let mut report = SweepReport::default();
    for replica in &plan.to_trash {
        trash_replica(replica, &mut report);
    }
    trash_orphan_sidecars(&mut report);
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

    fn replicas_of<'a>(report: &'a ReplicationReport, name: &str) -> Vec<&'a VerifiedReplica> {
        report
            .verified
            .iter()
            .filter(|r| r.source().file_name().unwrap() == name)
            .collect()
    }

    #[tokio::test]
    async fn finalize_plans_only_copies_this_import_wrote_of_unchosen_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        // Already at the destination, with the same contents.
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("IMG_3.JPG"), b"also keep").unwrap();
        let (_engine, report) = imported(&card, &dest).await;
        assert!(!replicas_of(&report, "IMG_3.JPG")[0].newly_written());

        let keep = HashSet::from([card.join("IMG_1.JPG")]);
        let plan = plan_removal(&report.verified, &keep);

        assert_eq!(plan.paths(), [dest.join("IMG_2.JPG").as_path()]);
        assert_eq!(plan.already_there.len(), 1);
    }

    #[tokio::test]
    async fn finalize_never_plans_the_source_or_a_chosen_file() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("photos");
        write_card(&card);
        let (_engine, report) = imported(&card, &card).await;
        let plan = plan_removal(&report.verified, &HashSet::new());
        assert!(plan.to_trash.is_empty());
        assert_eq!(plan.already_there.len(), 3);

        let (other, dest) = (tmp.path().join("card2"), tmp.path().join("dest"));
        write_card(&other);
        let (_engine, report) = imported(&other, &dest).await;
        let everything: HashSet<PathBuf> = report
            .verified
            .iter()
            .map(|r| r.source().to_path_buf())
            .collect();
        assert!(
            plan_removal(&report.verified, &everything)
                .to_trash
                .is_empty()
        );
    }

    #[tokio::test]
    async fn finalize_keeps_a_copy_changed_after_import() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (_engine, report) = imported(&card, &dest).await;
        let keep = HashSet::from([card.join("IMG_1.JPG"), card.join("IMG_3.JPG")]);
        fs::write(dest.join("IMG_2.JPG"), b"edited afterwards").unwrap();

        let outcome = execute_removal(&plan_removal(&report.verified, &keep));

        assert!(outcome.trashed.is_empty());
        assert_eq!(outcome.failures.len(), 1);
        assert!(dest.join("IMG_2.JPG").exists());
    }

    /// Puts small files in the real Trash, so it isn't part of the normal run.
    #[tokio::test]
    #[ignore = "adds files to the user's Trash"]
    async fn really_removes_an_unchosen_copy_and_its_own_sidecar_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (_engine, report) = imported(&card, &dest).await;
        let record = crate::TriageRecord {
            rating: 1,
            state: TriageState::Pending,
            develop: None,
        };
        crate::write_sidecar(&dest.join("IMG_2.JPG"), &record).unwrap();
        crate::write_sidecar(&dest.join("IMG_1.JPG"), &record).unwrap();
        fs::write(dest.join("IMG_3.xmp"), "another program's").unwrap();
        let keep = HashSet::from([card.join("IMG_1.JPG")]);

        let outcome = execute_removal(&plan_removal(&report.verified, &keep));

        assert_eq!(outcome.trashed.len(), 2, "{:?}", outcome.failures);
        assert_eq!(outcome.sidecars_trashed.len(), 1);
        assert!(!dest.join("IMG_2.JPG").exists() && !dest.join("IMG_2.xmp").exists());
        assert!(dest.join("IMG_1.JPG").exists() && dest.join("IMG_1.xmp").exists());
        assert!(dest.join("IMG_3.xmp").exists());
        assert!(card.join("IMG_2.JPG").exists());
    }

    /// Puts a small file in the real Trash, so it isn't part of the normal run:
    /// `cargo test -p ingot_core really_moves -- --ignored`
    #[tokio::test]
    #[ignore = "adds a file to the user's Trash"]
    async fn really_moves_a_rejected_copy_to_the_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        write_card(&card);
        let (engine, report) = imported(&card, &dest).await;
        reject(&engine, "img_2");

        let plan = plan_sweep(&engine.scan_response, &report.verified).unwrap();
        let outcome = execute_sweep(&plan);

        assert_eq!(outcome.trashed.len(), 1, "{:?}", outcome.failures);
        assert!(!dest.join("IMG_2.JPG").exists());
        assert!(dest.join("IMG_1.JPG").exists());
        assert!(card.join("IMG_2.JPG").exists());
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
