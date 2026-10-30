//! Verified replication from a source card to storage targets.
//!
//! Each source file is read once and hashed once. The reader is paced by the
//! primary target through a bounded channel, so the number of file buffers in
//! RAM stays bounded no matter how slow the primary is.

mod local;

pub use local::LocalTarget;

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use chrono::NaiveDate;
use tokio::{
    sync::{Semaphore, mpsc},
    task::{self, JoinSet},
};

use crate::ScanResponse;

pub(crate) type ReplicationError = Box<dyn std::error::Error + Send + Sync>;

/// Proof that `source` was written to `target` and read back with a matching
/// hash. Can only be built through `new`, which rejects mismatched hashes, and
/// `new` is only reachable from this module and its target implementations.
#[derive(Debug)]
pub struct VerifiedReplica {
    source: PathBuf,
    target: TargetId,
    location: PathBuf,
    hash: blake3::Hash,
}

impl VerifiedReplica {
    fn new(
        source: PathBuf,
        target: TargetId,
        location: PathBuf,
        expected: blake3::Hash,
        observed: blake3::Hash,
    ) -> Result<Self, ReplicationError> {
        if expected != observed {
            return Err(ReplicationError::from(format!(
                "verification failed for {}: expected {expected}, observed {observed}",
                source.display()
            )));
        }
        Ok(Self {
            source,
            target,
            location,
            hash: expected,
        })
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn target(&self) -> TargetId {
        self.target
    }

    /// Where the verified copy lives on the target.
    pub fn location(&self) -> &Path {
        &self.location
    }

    pub fn hash(&self) -> blake3::Hash {
        self.hash
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TargetId(pub u32);

#[async_trait::async_trait]
pub trait ReplicationTarget: Send + Sync {
    fn id(&self) -> TargetId;

    /// How many `store` calls this target can usefully run at once. Callers must
    /// not start more than this: a call waiting for a write permit still holds
    /// its file buffer in RAM.
    fn max_concurrent_writes(&self) -> usize;

    /// Writes `bytes` to this target, verifies them against `src_hash`, and
    /// returns the proof. `source` identifies the file for the proof only and must
    /// never be opened: the bytes were already read from it once.
    async fn store(
        &self,
        file_name: &OsStr,
        source: &Path,
        captured_at: NaiveDate,
        bytes: Arc<[u8]>,
        src_hash: blake3::Hash,
    ) -> Result<VerifiedReplica, ReplicationError>;
}

/// One file that was not copied to one target. `source` is `None` only when a
/// secondary worker panicked. `target` is `None` when the failure happened before
/// any target was involved.
#[derive(Debug)]
pub struct ReplicationFailure {
    pub source: Option<PathBuf>,
    pub target: Option<TargetId>,
    pub error: ReplicationError,
}

#[derive(Debug, Default)]
pub struct ReplicationReport {
    pub verified: Vec<VerifiedReplica>,
    pub failures: Vec<ReplicationFailure>,
    /// Secondary copies that had to come from the primary's file instead of RAM.
    pub deferred_copies: usize,
    /// When the card and the primary target were finished with. Secondaries may
    /// have kept running after this.
    pub primary_done_after: Duration,
}

impl ReplicationReport {
    fn record_store(
        &mut self,
        target: TargetId,
        source: PathBuf,
        result: Result<VerifiedReplica, ReplicationError>,
    ) {
        match result {
            Ok(replica) => self.verified.push(replica),
            Err(error) => self.failures.push(ReplicationFailure {
                source: Some(source),
                target: Some(target),
                error,
            }),
        }
    }
}

pub struct CopyEngine {
    source_read: Arc<Semaphore>,
    read_channel_depth: usize,
    primary_target: Arc<dyn ReplicationTarget>,
    secondary_targets: Vec<Arc<dyn ReplicationTarget>>,
}

#[derive(Clone)]
struct FileInfo {
    source: PathBuf,
    file_name: OsString,
    captured_at: NaiveDate,
    src_hash: blake3::Hash,
}

#[derive(Clone)]
struct CopyMessage {
    info: FileInfo,
    bytes: Arc<[u8]>,
}

/// A secondary copy that missed the RAM hand-off. It carries the card's hash, so
/// a corrupted primary file fails verification instead of spreading.
struct DeferredFile {
    info: FileInfo,
    primary_location: PathBuf,
}

type ReadOutcome = Result<CopyMessage, ReplicationFailure>;
type FileToRead = Result<(PathBuf, NaiveDate), ReplicationFailure>;

fn read_failure(source: PathBuf, error: ReplicationError) -> ReplicationFailure {
    ReplicationFailure {
        source: Some(source),
        target: None,
        error,
    }
}

/// Snapshots the files to copy so that no lock is held during I/O. A frame with
/// no capture date can't be routed, so it becomes a recorded failure.
fn list_files(scan: &RwLock<ScanResponse>) -> Result<Vec<FileToRead>, ReplicationError> {
    let scan = scan
        .read()
        .map_err(|err| format!("scan store lock poisoned: {err}"))?;

    let mut files = vec![];
    for asset in scan.assets.values() {
        for (_, path) in asset.files.get_all() {
            files.push(match asset.exif_data.captured_at {
                Some(captured_at) => Ok((path.clone(), captured_at.date())),
                None => Err(read_failure(
                    path.clone(),
                    ReplicationError::from(format!("no capture date for {}", path.display())),
                )),
            });
        }
    }
    Ok(files)
}

async fn read_file(source: PathBuf, captured_at: NaiveDate) -> ReadOutcome {
    let Some(file_name) = source.file_name().map(OsStr::to_os_string) else {
        let error = ReplicationError::from(format!("no file name in {}", source.display()));
        return Err(read_failure(source, error));
    };

    let path = source.clone();
    let read = tokio::task::spawn_blocking(move || -> Result<_, std::io::Error> {
        let bytes: Arc<[u8]> = Arc::from(std::fs::read(path)?);
        let src_hash = blake3::hash(&bytes);
        Ok((bytes, src_hash))
    })
    .await;

    match read {
        Ok(Ok((bytes, src_hash))) => Ok(CopyMessage {
            info: FileInfo {
                source,
                file_name,
                captured_at,
                src_hash,
            },
            bytes,
        }),
        Ok(Err(err)) => {
            let error =
                ReplicationError::from(format!("failed to read {}: {err}", source.display()));
            Err(read_failure(source, error))
        }
        Err(err) => {
            let error =
                ReplicationError::from(format!("read task failed for {}: {err}", source.display()));
            Err(read_failure(source, error))
        }
    }
}

struct PendingStore {
    info: FileInfo,
    /// Indexes of the secondaries waiting on this primary copy.
    deferred_to: Vec<usize>,
}

/// The running `store` calls of one target. Keeping each task's file by task id
/// means even a panicked store is reported against the right file.
struct StoreSet {
    tasks: JoinSet<Result<VerifiedReplica, ReplicationError>>,
    pending: HashMap<task::Id, PendingStore>,
}

impl StoreSet {
    fn new() -> Self {
        StoreSet {
            tasks: JoinSet::new(),
            pending: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.tasks.len()
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn spawn<F>(&mut self, pending: PendingStore, store: F)
    where
        F: Future<Output = Result<VerifiedReplica, ReplicationError>> + Send + 'static,
    {
        let handle = self.tasks.spawn(store);
        self.pending.insert(handle.id(), pending);
    }

    async fn next(&mut self) -> Option<(PendingStore, Result<VerifiedReplica, ReplicationError>)> {
        let (id, result) = match self.tasks.join_next_with_id().await? {
            Ok((id, result)) => (id, result),
            Err(err) => (
                err.id(),
                Err(ReplicationError::from(format!("store task failed: {err}"))),
            ),
        };
        let pending = self
            .pending
            .remove(&id)
            .expect("every store task is registered when spawned");
        Some((pending, result))
    }
}

async fn store_from_primary(
    target: &dyn ReplicationTarget,
    file: DeferredFile,
) -> Result<VerifiedReplica, ReplicationError> {
    let bytes = tokio::fs::read(&file.primary_location)
        .await
        .map_err(|err| {
            format!(
                "failed to read primary copy {}: {err}",
                file.primary_location.display()
            )
        })?;
    let info = file.info;
    target
        .store(
            &info.file_name,
            &info.source,
            info.captured_at,
            Arc::from(bytes),
            info.src_hash,
        )
        .await
}

/// Works through one secondary's RAM hand-offs and deferred files. While all its
/// write slots are busy it stops receiving, its queue fills, and later files
/// become deferred, so this target can never hold back the card.
async fn run_secondary(
    target: Arc<dyn ReplicationTarget>,
    mut hot: mpsc::Receiver<CopyMessage>,
    mut deferred: mpsc::UnboundedReceiver<DeferredFile>,
) -> ReplicationReport {
    let mut report = ReplicationReport::default();
    let mut stores = StoreSet::new();
    let max_stores = target.max_concurrent_writes().max(1);
    let (mut hot_open, mut deferred_open) = (true, true);

    while hot_open || deferred_open {
        while stores.len() >= max_stores {
            if let Some((pending, result)) = stores.next().await {
                report.record_store(target.id(), pending.info.source, result);
            }
        }

        tokio::select! {
            biased;
            finished = stores.next(), if !stores.is_empty() => {
                if let Some((pending, result)) = finished {
                    report.record_store(target.id(), pending.info.source, result);
                }
            }
            msg = hot.recv(), if hot_open => match msg {
                Some(msg) => {
                    let (target, info) = (Arc::clone(&target), msg.info.clone());
                    stores.spawn(
                        PendingStore { info: msg.info, deferred_to: vec![] },
                        async move {
                            target
                                .store(
                                    &info.file_name,
                                    &info.source,
                                    info.captured_at,
                                    msg.bytes,
                                    info.src_hash,
                                )
                                .await
                        },
                    );
                }
                None => hot_open = false,
            },
            file = deferred.recv(), if deferred_open => match file {
                Some(file) => {
                    let target = Arc::clone(&target);
                    stores.spawn(
                        PendingStore { info: file.info.clone(), deferred_to: vec![] },
                        async move { store_from_primary(&*target, file).await },
                    );
                }
                None => deferred_open = false,
            },
        }
    }

    while let Some((pending, result)) = stores.next().await {
        report.record_store(target.id(), pending.info.source, result);
    }
    report
}

struct SecondaryQueue {
    id: TargetId,
    hot: mpsc::Sender<CopyMessage>,
    deferred: mpsc::UnboundedSender<DeferredFile>,
}

/// Records a finished primary copy and releases the secondaries waiting on it.
fn settle_primary(
    report: &mut ReplicationReport,
    primary: TargetId,
    queues: &[SecondaryQueue],
    pending: PendingStore,
    result: Result<VerifiedReplica, ReplicationError>,
) {
    let PendingStore { info, deferred_to } = pending;

    for index in deferred_to {
        let queue = &queues[index];
        let queued = match &result {
            Ok(replica) => queue
                .deferred
                .send(DeferredFile {
                    info: info.clone(),
                    primary_location: replica.location().to_path_buf(),
                })
                .map_err(|_| ReplicationError::from("secondary target worker stopped")),
            Err(_) => Err(ReplicationError::from(
                "not copied: file missed the RAM hand-off and the primary copy failed",
            )),
        };
        match queued {
            Ok(()) => report.deferred_copies += 1,
            Err(error) => report.failures.push(ReplicationFailure {
                source: Some(info.source.clone()),
                target: Some(queue.id),
                error,
            }),
        }
    }

    report.record_store(primary, info.source, result);
}

impl CopyEngine {
    /// `read_channel_depth` is the memory governor: at most
    /// `read_channel_depth + card_read_permits` files are buffered ahead of the
    /// primary target's writes, and each secondary can hold another
    /// `read_channel_depth` files plus its own concurrent writes.
    pub fn new(
        card_read_permits: usize,
        read_channel_depth: usize,
        primary_target: Arc<dyn ReplicationTarget>,
    ) -> Self {
        CopyEngine {
            source_read: Arc::new(Semaphore::new(card_read_permits.max(1))),
            read_channel_depth: read_channel_depth.max(1),
            primary_target,
            secondary_targets: vec![],
        }
    }

    pub fn with_secondary(mut self, target: Arc<dyn ReplicationTarget>) -> Self {
        self.secondary_targets.push(target);
        self
    }

    /// Reads every file once in the background. A read permit is held until the
    /// bytes are in the channel, so a full channel stops further card reads.
    fn start_read_stream(
        &self,
        scan: &RwLock<ScanResponse>,
    ) -> Result<mpsc::Receiver<ReadOutcome>, ReplicationError> {
        let files = list_files(scan)?;
        let (tx, rx) = mpsc::channel(self.read_channel_depth);
        let source_read = Arc::clone(&self.source_read);

        tokio::spawn(async move {
            for file in files {
                let (source, captured_at) = match file {
                    Ok(file) => file,
                    Err(failure) => {
                        if tx.send(Err(failure)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };

                let permit = match Arc::clone(&source_read).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        let error =
                            ReplicationError::from(format!("read permit unavailable: {err}"));
                        if tx.send(Err(read_failure(source, error))).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };

                // Nobody is consuming, so stop reading the card.
                if tx.is_closed() {
                    return;
                }

                let tx = tx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let outcome = read_file(source, captured_at).await;
                    let _ = tx.send(outcome).await;
                });
            }
        });

        Ok(rx)
    }

    /// Copies every scanned file to all targets. The primary paces the card;
    /// secondaries take bytes from RAM when they keep up and from the primary's
    /// file when they don't. One bad file never stops the batch: its failure is
    /// recorded in the report.
    pub async fn replicate(
        &self,
        scan: &RwLock<ScanResponse>,
    ) -> Result<ReplicationReport, ReplicationError> {
        let started = Instant::now();
        let mut rx = self.start_read_stream(scan)?;
        let mut report = ReplicationReport::default();
        let mut stores = StoreSet::new();
        let primary_id = self.primary_target.id();
        let max_stores = self.primary_target.max_concurrent_writes().max(1);

        let mut queues = Vec::with_capacity(self.secondary_targets.len());
        let mut workers = JoinSet::new();
        for target in &self.secondary_targets {
            let (hot_tx, hot_rx) = mpsc::channel(self.read_channel_depth);
            let (deferred_tx, deferred_rx) = mpsc::unbounded_channel();
            queues.push(SecondaryQueue {
                id: target.id(),
                hot: hot_tx,
                deferred: deferred_tx,
            });
            workers.spawn(run_secondary(Arc::clone(target), hot_rx, deferred_rx));
        }

        while let Some(outcome) = rx.recv().await {
            let msg = match outcome {
                Ok(msg) => msg,
                Err(failure) => {
                    report.failures.push(failure);
                    continue;
                }
            };

            // Waiting here, rather than spawning and letting the target queue the
            // work, is what passes backpressure on to the reader.
            while stores.len() >= max_stores {
                if let Some((pending, result)) = stores.next().await {
                    settle_primary(&mut report, primary_id, &queues, pending, result);
                }
            }

            let deferred_to = queues
                .iter()
                .enumerate()
                .filter(|(_, queue)| queue.hot.try_send(msg.clone()).is_err())
                .map(|(index, _)| index)
                .collect();

            let target = Arc::clone(&self.primary_target);
            let info = msg.info;
            let bytes = msg.bytes;
            stores.spawn(
                PendingStore {
                    info: info.clone(),
                    deferred_to,
                },
                async move {
                    target
                        .store(
                            &info.file_name,
                            &info.source,
                            info.captured_at,
                            bytes,
                            info.src_hash,
                        )
                        .await
                },
            );
        }

        while let Some((pending, result)) = stores.next().await {
            settle_primary(&mut report, primary_id, &queues, pending, result);
        }
        report.primary_done_after = started.elapsed();

        // Closing the queues lets each worker finish once its backlog is done.
        drop(queues);
        while let Some(joined) = workers.join_next().await {
            match joined {
                Ok(secondary) => {
                    report.verified.extend(secondary.verified);
                    report.failures.extend(secondary.failures);
                }
                Err(err) => report.failures.push(ReplicationFailure {
                    source: None,
                    target: None,
                    error: ReplicationError::from(format!("secondary worker failed: {err}")),
                }),
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;
    use crate::{AssetKey, FileKind, PhotoAsset, Target, TargetKind};

    const DATE: (i32, u32, u32) = (2026, 5, 22);

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(DATE.0, DATE.1, DATE.2).unwrap()
    }

    fn write_source(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn scan_of(files: &[(&Path, FileKind)], dated: bool) -> RwLock<ScanResponse> {
        let mut assets: HashMap<AssetKey, PhotoAsset> = HashMap::new();
        for (path, kind) in files {
            let key = AssetKey {
                dir: path.parent().unwrap().to_path_buf(),
                stem: path.file_stem().unwrap().to_string_lossy().to_lowercase(),
            };
            match assets.get_mut(&key) {
                Some(asset) => asset.files.insert(*kind, path.to_path_buf()).unwrap(),
                None => {
                    let mut asset = PhotoAsset::new(*kind, path.to_path_buf());
                    if dated {
                        asset.exif_data.captured_at = date().and_hms_opt(12, 0, 0);
                    }
                    assets.insert(key, asset);
                }
            }
        }
        RwLock::new(ScanResponse {
            assets,
            collisions: vec![],
            errors: vec![],
        })
    }

    fn local_target(root: &Path, write_permits: usize) -> Arc<LocalTarget> {
        Arc::new(LocalTarget::new(
            TargetId(1),
            Target {
                root: root.to_path_buf(),
                kind: TargetKind::LocalNvme,
                write_permits,
            },
        ))
    }

    fn dated_dir(root: &Path) -> PathBuf {
        root.join("2026").join("2026-05-22")
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn pair_keeps_extensions_and_contents() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let raw = write_source(&card, "IMG_0001.CR2", b"raw bytes");
        let jpg = write_source(&card, "IMG_0001.JPG", b"jpeg bytes");
        let scan = scan_of(&[(&raw, FileKind::Raw), (&jpg, FileKind::Jpeg)], true);
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(2, 4, local_target(&root, 4));
        let report = engine.replicate(&scan).await.unwrap();

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), 2);
        let dir = dated_dir(&root);
        assert_eq!(dir_entries(&dir), ["IMG_0001.CR2", "IMG_0001.JPG"]);
        assert_eq!(fs::read(dir.join("IMG_0001.CR2")).unwrap(), b"raw bytes");
        assert_eq!(fs::read(dir.join("IMG_0001.JPG")).unwrap(), b"jpeg bytes");
        for replica in &report.verified {
            assert_eq!(replica.target(), TargetId(1));
            assert_eq!(
                replica.hash(),
                blake3::hash(&fs::read(replica.source()).unwrap())
            );
        }
    }

    #[tokio::test]
    async fn existing_destination_is_left_untouched() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let raw = write_source(&card, "IMG_0001.CR2", b"new bytes");
        let scan = scan_of(&[(&raw, FileKind::Raw)], true);
        let root = tmp.path().join("target");
        let existing = write_source(&dated_dir(&root), "IMG_0001.CR2", b"existing bytes");

        let engine = CopyEngine::new(1, 4, local_target(&root, 4));
        let report = engine.replicate(&scan).await.unwrap();

        assert!(report.verified.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].source.as_deref(), Some(raw.as_path()));
        assert_eq!(fs::read(&existing).unwrap(), b"existing bytes");
        assert_eq!(dir_entries(&dated_dir(&root)), ["IMG_0001.CR2"]);
    }

    #[tokio::test]
    async fn hash_mismatch_publishes_nothing() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("target");
        let target = local_target(&root, 1);

        let result = target
            .store(
                OsStr::new("IMG_0001.CR2"),
                Path::new("card/IMG_0001.CR2"),
                date(),
                Arc::from(&b"bytes that landed"[..]),
                blake3::hash(b"bytes the card held"),
            )
            .await;

        assert!(result.is_err());
        assert!(dir_entries(&dated_dir(&root)).is_empty());
    }

    #[tokio::test]
    async fn same_name_from_two_folders_keeps_one_intact_copy() {
        let tmp = tempdir().unwrap();
        let first = write_source(
            &tmp.path().join("100CANON"),
            "IMG_0001.CR2",
            &vec![1u8; 1 << 20],
        );
        let second = write_source(
            &tmp.path().join("101CANON"),
            "IMG_0001.CR2",
            &vec![2u8; 1 << 20],
        );
        let scan = scan_of(&[(&first, FileKind::Raw), (&second, FileKind::Raw)], true);
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(2, 4, local_target(&root, 4));
        let report = engine.replicate(&scan).await.unwrap();

        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.failures.len(), 1);
        let kept = fs::read(dated_dir(&root).join("IMG_0001.CR2")).unwrap();
        assert_eq!(kept, fs::read(report.verified[0].source()).unwrap());
        assert_eq!(dir_entries(&dated_dir(&root)), ["IMG_0001.CR2"]);
    }

    #[tokio::test]
    async fn undated_and_missing_files_are_recorded() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let undated = write_source(&card, "IMG_0001.CR2", b"bytes");
        let missing = card.join("IMG_0002.CR2");
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(1, 4, local_target(&root, 4));

        let report = engine
            .replicate(&scan_of(&[(&undated, FileKind::Raw)], false))
            .await
            .unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(report.verified.is_empty());

        let report = engine
            .replicate(&scan_of(&[(&missing, FileKind::Raw)], true))
            .await
            .unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].source.as_deref(),
            Some(missing.as_path())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_channel_stops_card_reads() {
        const FILES: usize = 20;
        const DEPTH: usize = 2;
        const READ_PERMITS: usize = 1;

        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let paths: Vec<PathBuf> = (0..FILES)
            .map(|i| write_source(&card, &format!("IMG_{i:04}.CR2"), b"bytes"))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let scan = scan_of(&files, true);

        let engine = CopyEngine::new(READ_PERMITS, DEPTH, local_target(tmp.path(), 1));
        let mut rx = engine.start_read_stream(&scan).unwrap();

        // Nothing is consuming. Any file the reader hasn't reached by the time it
        // is deleted fails, so the successes count exactly what was read.
        tokio::time::sleep(Duration::from_millis(200)).await;
        fs::remove_dir_all(&card).unwrap();

        let (mut read, mut failed) = (0, 0);
        while let Some(outcome) = rx.recv().await {
            match outcome {
                Ok(_) => read += 1,
                Err(_) => failed += 1,
            }
        }
        assert!(read >= 1);
        assert!(
            read <= DEPTH + READ_PERMITS,
            "read {read} files ahead of the consumer"
        );
        assert_eq!(read + failed, FILES);
    }

    /// Records how many `store` calls overlap, without touching a disk.
    struct CountingTarget {
        limit: usize,
        running: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ReplicationTarget for CountingTarget {
        fn id(&self) -> TargetId {
            TargetId(2)
        }

        fn max_concurrent_writes(&self) -> usize {
            self.limit
        }

        async fn store(
            &self,
            _file_name: &OsStr,
            source: &Path,
            _captured_at: NaiveDate,
            bytes: Arc<[u8]>,
            src_hash: blake3::Hash,
        ) -> Result<VerifiedReplica, ReplicationError> {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(running, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            VerifiedReplica::new(
                source.to_path_buf(),
                self.id(),
                source.to_path_buf(),
                src_hash,
                blake3::hash(&bytes),
            )
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn store_calls_never_exceed_the_target_limit() {
        let tmp = tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..30)
            .map(|i| write_source(tmp.path(), &format!("IMG_{i:04}.CR2"), b"bytes"))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let target = Arc::new(CountingTarget {
            limit: 3,
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });

        let engine = CopyEngine::new(4, 8, target.clone());
        let report = engine.replicate(&scan_of(&files, true)).await.unwrap();

        assert_eq!(report.verified.len(), 30);
        assert_eq!(target.peak.load(Ordering::SeqCst), 3);
    }

    /// Wraps a real target and holds every store until `gate` opens.
    struct GatedTarget {
        inner: Arc<LocalTarget>,
        gate: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl ReplicationTarget for GatedTarget {
        fn id(&self) -> TargetId {
            TargetId(3)
        }

        fn max_concurrent_writes(&self) -> usize {
            1
        }

        async fn store(
            &self,
            file_name: &OsStr,
            source: &Path,
            captured_at: NaiveDate,
            bytes: Arc<[u8]>,
            src_hash: blake3::Hash,
        ) -> Result<VerifiedReplica, ReplicationError> {
            let _open = self.gate.acquire().await?;
            self.inner
                .store(file_name, source, captured_at, bytes, src_hash)
                .await
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stalled_secondary_never_holds_the_card_and_catches_up_without_it() {
        const FILES: usize = 12;

        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let paths: Vec<PathBuf> = (0..FILES)
            .map(|i| write_source(&card, &format!("IMG_{i:04}.CR2"), &[i as u8; 64]))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let scan = scan_of(&files, true);

        let primary_root = tmp.path().join("primary");
        let secondary_root = tmp.path().join("secondary");
        let gate = Arc::new(Semaphore::new(0));
        let secondary = GatedTarget {
            inner: local_target(&secondary_root, 1),
            gate: Arc::clone(&gate),
        };
        let engine = Arc::new(
            CopyEngine::new(1, 1, local_target(&primary_root, 2))
                .with_secondary(Arc::new(secondary)),
        );

        let scan = Arc::new(scan);
        let running = {
            let (engine, scan) = (Arc::clone(&engine), Arc::clone(&scan));
            tokio::spawn(async move { engine.replicate(&scan).await.unwrap() })
        };

        // With the secondary stalled, the primary must still receive everything.
        let primary_dir = dated_dir(&primary_root);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !primary_dir.exists() || dir_entries(&primary_dir).len() < FILES {
            assert!(
                tokio::time::Instant::now() < deadline,
                "primary stalled behind the secondary"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The card is gone, so the secondary can only use RAM or the primary copy.
        fs::remove_dir_all(&card).unwrap();
        gate.add_permits(FILES);
        let report = running.await.unwrap();

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), FILES * 2);
        assert!(report.deferred_copies > 0);
        let secondary_dir = dated_dir(&secondary_root);
        assert_eq!(dir_entries(&secondary_dir).len(), FILES);
        for i in 0..FILES {
            let name = format!("IMG_{i:04}.CR2");
            assert_eq!(fs::read(secondary_dir.join(&name)).unwrap(), [i as u8; 64]);
        }
    }

    #[tokio::test]
    async fn corrupt_primary_copy_does_not_reach_the_secondary() {
        let tmp = tempdir().unwrap();
        let primary_copy = write_source(tmp.path(), "IMG_0001.CR2", b"corrupted on disk");
        let root = tmp.path().join("secondary");
        let target = local_target(&root, 1);

        let result = store_from_primary(
            &*target,
            DeferredFile {
                info: FileInfo {
                    source: "card/IMG_0001.CR2".into(),
                    file_name: "IMG_0001.CR2".into(),
                    captured_at: date(),
                    src_hash: blake3::hash(b"what the card held"),
                },
                primary_location: primary_copy,
            },
        )
        .await;

        assert!(result.is_err());
        assert!(dir_entries(&dated_dir(&root)).is_empty());
    }

    #[tokio::test]
    async fn failed_primary_is_reported_for_waiting_secondaries() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let raw = write_source(&card, "IMG_0001.CR2", b"new bytes");
        let scan = scan_of(&[(&raw, FileKind::Raw)], true);
        let primary_root = tmp.path().join("primary");
        let existing = write_source(&dated_dir(&primary_root), "IMG_0001.CR2", b"existing");
        let secondary_root = tmp.path().join("secondary");
        let secondary = Arc::new(LocalTarget::new(
            TargetId(9),
            Target {
                root: secondary_root.clone(),
                kind: TargetKind::LocalNvme,
                write_permits: 1,
            },
        ));

        let engine =
            CopyEngine::new(1, 4, local_target(&primary_root, 1)).with_secondary(secondary);
        let report = engine.replicate(&scan).await.unwrap();

        // The secondary had room, so it still got its copy from RAM.
        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.verified[0].target(), TargetId(9));
        assert_eq!(report.failures.len(), 1);
        assert_eq!(fs::read(&existing).unwrap(), b"existing");
        assert_eq!(
            fs::read(dated_dir(&secondary_root).join("IMG_0001.CR2")).unwrap(),
            b"new bytes"
        );
    }
}
