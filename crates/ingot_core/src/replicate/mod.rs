//! Verified replication from a source card to storage targets.
//!
//! Each source file is read once and hashed once. The reader is paced by the
//! primary target through a bounded channel, so the number of file buffers in
//! RAM stays bounded no matter how slow the primary is.

mod drive;
mod drive_auth;
mod gate;
mod local;

pub use drive::{DRIVE_ROOT_FOLDER, DriveConfig, DriveTarget};
pub use drive_auth::{DriveCredentials, drive_login};
pub use gate::{CardGate, CardPermit};
pub use local::LocalTarget;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    hash::Hash,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    task::{self, JoinSet},
};

use crate::import::{DuplicateKey, ImportPlan, PlannedFile};

pub(crate) type ReplicationError = Box<dyn std::error::Error + Send + Sync>;

/// How many numbered names to try before giving up on a file: `IMG_1.CR2`,
/// `IMG_1-2.CR2`, and so on.
pub(crate) const MAX_NAME_ATTEMPTS: u32 = 1000;

/// The name to try on `attempt`, counted from 1. Keeping the extension last means
/// the numbered copy still opens as what it is.
pub(crate) fn candidate_name(file_name: &OsStr, attempt: u32) -> OsString {
    if attempt <= 1 {
        return file_name.to_os_string();
    }
    let path = Path::new(file_name);
    let mut name = path.file_stem().unwrap_or(file_name).to_os_string();
    name.push(format!("-{attempt}"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    name
}

/// Destinations being worked on right now, by any key that names one. Two source
/// files can route to the same name (same name and date in different folders).
pub(crate) struct DestinationClaims<K = PathBuf> {
    in_flight: Mutex<HashSet<K>>,
    released: Notify,
}

impl<K> Default for DestinationClaims<K> {
    fn default() -> Self {
        DestinationClaims {
            in_flight: Mutex::new(HashSet::new()),
            released: Notify::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> DestinationClaims<K> {
    /// Waits until nobody else holds `key`. Waiting rather than moving on to the
    /// next name matters on a re-run, where the file that belongs under this name
    /// must find it, not a duplicate of itself under another.
    pub(crate) async fn claim(&self, key: K) -> DestinationClaim<'_, K> {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut in_flight = self
                    .in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if in_flight.insert(key.clone()) {
                    return DestinationClaim { claims: self, key };
                }
            }
            released.await;
        }
    }
}

pub(crate) struct DestinationClaim<'a, K: Eq + Hash> {
    claims: &'a DestinationClaims<K>,
    key: K,
}

impl<K: Eq + Hash> Drop for DestinationClaim<'_, K> {
    fn drop(&mut self) {
        self.claims
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        self.claims.released.notify_waiters();
    }
}

/// Where a verified copy lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaLocation {
    Local(PathBuf),
    /// A file on Google Drive, by its Drive file id.
    Remote(String),
}

/// Proof that `source` is on `target` with a matching hash. Can only be built
/// through `new`, which rejects mismatched hashes, and `new` is only reachable
/// from this module and its target implementations.
#[derive(Clone, Debug)]
pub struct VerifiedReplica {
    source: PathBuf,
    target: TargetId,
    location: ReplicaLocation,
    size: u64,
    newly_written: bool,
    hash: blake3::Hash,
}

impl VerifiedReplica {
    fn new(
        source: PathBuf,
        target: TargetId,
        location: ReplicaLocation,
        size: u64,
        newly_written: bool,
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
            size,
            newly_written,
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
    pub fn location(&self) -> &ReplicaLocation {
        &self.location
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// False when an identical file was already on the target.
    pub fn newly_written(&self) -> bool {
        self.newly_written
    }

    pub fn hash(&self) -> blake3::Hash {
        self.hash
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TargetId(pub u32);

/// One source file on its way to the targets.
#[derive(Clone, Debug)]
pub struct FileInfo {
    /// Identifies the file for the proof only. Targets must never open it: the
    /// bytes were already read from it once.
    pub source: PathBuf,
    /// Where the file belongs below a target's root.
    pub relative: PathBuf,
    /// The source's modified time, kept on the copy where the target supports it.
    pub modified: Option<SystemTime>,
    pub src_hash: blake3::Hash,
}

#[async_trait::async_trait]
pub trait ReplicationTarget: Send + Sync {
    fn id(&self) -> TargetId;

    /// How many `store` calls this target can usefully run at once. Callers must
    /// not start more than this: a call waiting for a write permit still holds
    /// its file buffer in RAM.
    fn max_concurrent_writes(&self) -> usize;

    /// Puts `bytes` on this target, verifies them against `file.src_hash`, and
    /// returns the proof. Never replaces anything already on the target.
    async fn store(
        &self,
        file: &FileInfo,
        bytes: Arc<[u8]>,
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

/// Live progress, one event per file per target. Sending never waits, so a slow
/// consumer can't hold up a copy.
#[derive(Debug)]
pub enum ReplicationEvent {
    Verified {
        source: PathBuf,
        target: TargetId,
        size: u64,
        newly_written: bool,
        replica: VerifiedReplica,
    },
    Failed {
        source: Option<PathBuf>,
        target: Option<TargetId>,
        error: String,
    },
}

type Progress = Option<crossbeam_channel::Sender<ReplicationEvent>>;

impl ReplicationReport {
    fn record_failure(&mut self, progress: &Progress, failure: ReplicationFailure) {
        if let Some(progress) = progress {
            let _ = progress.send(ReplicationEvent::Failed {
                source: failure.source.clone(),
                target: failure.target,
                error: failure.error.to_string(),
            });
        }
        self.failures.push(failure);
    }

    fn record_store(
        &mut self,
        progress: &Progress,
        target: TargetId,
        source: PathBuf,
        result: Result<VerifiedReplica, ReplicationError>,
    ) {
        match result {
            Ok(replica) => {
                if let Some(progress) = progress {
                    let _ = progress.send(ReplicationEvent::Verified {
                        source,
                        target,
                        size: replica.size(),
                        newly_written: replica.newly_written(),
                        replica: replica.clone(),
                    });
                }
                self.verified.push(replica);
            }
            Err(error) => self.record_failure(
                progress,
                ReplicationFailure {
                    source: Some(source),
                    target: Some(target),
                    error,
                },
            ),
        }
    }
}

/// A copy on local disk to send to another target, carrying the card's hash so a
/// damaged local copy fails verification instead of spreading.
pub struct UploadFile {
    pub info: FileInfo,
    pub local: PathBuf,
}

/// Sends local copies to one target, as many at once as it allows. The card isn't
/// involved. Used for Google Drive, which only gets photos once they are chosen,
/// because Ingot can't take anything back from it.
pub async fn upload_copies(
    target: Arc<dyn ReplicationTarget>,
    files: Vec<UploadFile>,
    progress: Option<crossbeam_channel::Sender<ReplicationEvent>>,
) -> ReplicationReport {
    let started = Instant::now();
    let mut report = ReplicationReport::default();
    let mut stores = StoreSet::new();
    let max_stores = target.max_concurrent_writes().max(1);
    for file in files {
        while stores.len() >= max_stores {
            if let Some((pending, result)) = stores.next().await {
                report.record_store(&progress, target.id(), pending.info.source, result);
            }
        }
        let target = Arc::clone(&target);
        stores.spawn(
            PendingStore {
                info: file.info.clone(),
                deferred_to: vec![],
            },
            async move {
                let deferred = DeferredFile {
                    info: file.info,
                    primary_location: file.local,
                };
                store_from_primary(&*target, deferred).await
            },
        );
    }
    while let Some((pending, result)) = stores.next().await {
        report.record_store(&progress, target.id(), pending.info.source, result);
    }
    report.primary_done_after = started.elapsed();
    report
}

/// The files an import still has to read. It can change while the copy runs: a
/// photographer ticks and unticks photos, and a file is only out of reach once its
/// read has started.
pub struct ImportQueue {
    state: Mutex<QueueState>,
    changed: Notify,
}

#[derive(Default)]
struct QueueState {
    waiting: VecDeque<PlannedFile>,
    closed: bool,
    stopped: bool,
    duplicate_keys: HashMap<PathBuf, DuplicateKey>,
}

impl ImportQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(ImportQueue {
            state: Mutex::new(QueueState::default()),
            changed: Notify::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a file, unless it is already waiting. Returns whether it was added; a
    /// closed queue takes nothing.
    pub fn push(&self, file: PlannedFile) -> bool {
        let mut state = self.lock();
        if state.closed || state.waiting.iter().any(|f| f.source == file.source) {
            return false;
        }
        state
            .duplicate_keys
            .insert(file.source.clone(), file.duplicate_key.clone());
        state.waiting.push_back(file);
        self.changed.notify_one();
        true
    }

    /// Takes a file back if its read hasn't started. Returns whether it was.
    pub fn remove(&self, source: &Path) -> bool {
        let mut state = self.lock();
        let before = state.waiting.len();
        state.waiting.retain(|file| file.source != source);
        before != state.waiting.len()
    }

    /// No more files will come. What is already waiting still gets copied.
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_one();
    }

    /// Stops the import: nothing else starts, and reads in progress end at their next
    /// chunk. A write already under way finishes, so it is either a verified copy or
    /// nothing. Copies already made stay.
    pub fn stop(&self) {
        {
            let mut state = self.lock();
            state.stopped = true;
            state.closed = true;
            state.waiting.clear();
        }
        self.changed.notify_one();
    }

    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    pub fn waiting(&self) -> usize {
        self.lock().waiting.len()
    }

    pub(crate) fn duplicate_key(&self, source: &Path) -> Option<DuplicateKey> {
        self.lock().duplicate_keys.get(source).cloned()
    }

    /// The next file, waiting for one if need be. `None` once closed and empty.
    async fn next(&self) -> Option<PlannedFile> {
        loop {
            {
                let mut state = self.lock();
                if let Some(file) = state.waiting.pop_front() {
                    return Some(file);
                }
                if state.closed {
                    return None;
                }
            }
            // `notify_one` stores a wakeup when nobody is waiting yet, so a push
            // between the check above and this await isn't lost.
            self.changed.notified().await;
        }
    }
}

/// Bytes the copy reads before going back to the card gate. On a 20 MB/s card, 2 MiB is
/// the 100 ms or so a culling read can have to wait. See `bench_culling`.
pub const DEFAULT_READ_CHUNK: usize = 2 * 1024 * 1024;

/// Files allowed to be mid-read at once on top of the channel depth. The card gate
/// decides how many actually touch the card; this only has to keep it supplied.
const READS_IN_PROGRESS: usize = 2;

pub struct CopyEngine {
    card: Arc<CardGate>,
    read_channel_depth: usize,
    primary_target: Arc<dyn ReplicationTarget>,
    secondary_targets: Vec<Arc<dyn ReplicationTarget>>,
    progress: Progress,
    read_chunk: usize,
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

fn read_failure(source: PathBuf, error: ReplicationError) -> ReplicationFailure {
    ReplicationFailure {
        source: Some(source),
        target: None,
        error,
    }
}

/// Why a read ended early: the import was stopped, which isn't a failure.
const STOPPED: &str = "the import was stopped";

/// Reads one file, a chunk at a time, going back to the gate between chunks so a
/// culling read never waits for more than one chunk. It is still one read of the
/// file, hashed as a whole, and the bytes are in RAM once for all targets.
fn read_file_blocking(
    gate: &Arc<CardGate>,
    queue: &ImportQueue,
    chunk: usize,
    file: PlannedFile,
) -> Result<CopyMessage, ReplicationError> {
    let mut opened = {
        let _permit = gate.acquire_background();
        std::fs::File::open(&file.source)?
    };
    let mut bytes = vec![0u8; file.size as usize];
    let mut filled = 0;
    let mut hasher = blake3::Hasher::new();
    loop {
        if queue.is_stopped() {
            return Err(STOPPED.into());
        }
        if filled == bytes.len() {
            // The file may have grown since it was listed. Copy what it holds now.
            bytes.resize(filled + chunk.max(1), 0);
        }
        let end = (filled + chunk.max(1)).min(bytes.len());
        let read = {
            let _permit = gate.acquire_background();
            let read = opened.read(&mut bytes[filled..end])?;
            gate.simulate_read(read as u64);
            read
        };
        if read == 0 {
            break;
        }
        hasher.update(&bytes[filled..filled + read]);
        filled += read;
    }
    bytes.truncate(filled);

    Ok(CopyMessage {
        info: FileInfo {
            source: file.source,
            relative: file.relative,
            modified: file.modified,
            src_hash: hasher.finalize(),
        },
        bytes: Arc::from(bytes),
    })
}

async fn read_file(
    gate: Arc<CardGate>,
    queue: Arc<ImportQueue>,
    chunk: usize,
    file: PlannedFile,
) -> ReadOutcome {
    let source = file.source.clone();
    let read =
        tokio::task::spawn_blocking(move || read_file_blocking(&gate, &queue, chunk, file)).await;

    match read {
        Ok(Ok(message)) => Ok(message),
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
    target.store(&file.info, Arc::from(bytes)).await
}

/// Works through one secondary's RAM hand-offs and deferred files. While all its
/// write slots are busy it stops receiving, its queue fills, and later files
/// become deferred, so this target can never hold back the card.
async fn run_secondary(
    target: Arc<dyn ReplicationTarget>,
    mut hot: mpsc::Receiver<CopyMessage>,
    mut deferred: mpsc::UnboundedReceiver<DeferredFile>,
    progress: Progress,
    queue: Arc<ImportQueue>,
) -> ReplicationReport {
    let mut report = ReplicationReport::default();
    let mut stores = StoreSet::new();
    let max_stores = target.max_concurrent_writes().max(1);
    let (mut hot_open, mut deferred_open) = (true, true);

    while hot_open || deferred_open {
        while stores.len() >= max_stores {
            if let Some((pending, result)) = stores.next().await {
                report.record_store(&progress, target.id(), pending.info.source, result);
            }
        }

        tokio::select! {
            biased;
            finished = stores.next(), if !stores.is_empty() => {
                if let Some((pending, result)) = finished {
                    report.record_store(&progress, target.id(), pending.info.source, result);
                }
            }
            msg = hot.recv(), if hot_open => match msg {
                Some(_) if queue.is_stopped() => {}
                Some(msg) => {
                    let (target, info) = (Arc::clone(&target), msg.info.clone());
                    stores.spawn(
                        PendingStore { info: msg.info, deferred_to: vec![] },
                        async move {
                            target.store(&info, msg.bytes).await
                        },
                    );
                }
                None => hot_open = false,
            },
            file = deferred.recv(), if deferred_open => match file {
                Some(_) if queue.is_stopped() => {}
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
        report.record_store(&progress, target.id(), pending.info.source, result);
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
    progress: &Progress,
    pending: PendingStore,
    result: Result<VerifiedReplica, ReplicationError>,
) {
    let PendingStore { info, deferred_to } = pending;

    for index in deferred_to {
        let queue = &queues[index];
        let queued = match &result {
            Ok(replica) => match replica.location() {
                ReplicaLocation::Local(path) => queue
                    .deferred
                    .send(DeferredFile {
                        info: info.clone(),
                        primary_location: path.clone(),
                    })
                    .map_err(|_| ReplicationError::from("secondary target worker stopped")),
                ReplicaLocation::Remote(_) => Err(ReplicationError::from(
                    "not copied: file missed the RAM hand-off and the primary is not local",
                )),
            },
            Err(_) => Err(ReplicationError::from(
                "not copied: file missed the RAM hand-off and the primary copy failed",
            )),
        };
        match queued {
            Ok(()) => report.deferred_copies += 1,
            Err(error) => report.record_failure(
                progress,
                ReplicationFailure {
                    source: Some(info.source.clone()),
                    target: Some(queue.id),
                    error,
                },
            ),
        }
    }

    report.record_store(progress, primary, info.source, result);
}

impl CopyEngine {
    /// `read_channel_depth` is the memory governor: at most
    /// `read_channel_depth + READS_IN_PROGRESS` files are buffered ahead of the
    /// primary target's writes, and each secondary can hold another
    /// `read_channel_depth` files plus its own concurrent writes.
    pub fn new(
        card: Arc<CardGate>,
        read_channel_depth: usize,
        primary_target: Arc<dyn ReplicationTarget>,
    ) -> Self {
        CopyEngine {
            card,
            read_channel_depth: read_channel_depth.max(1),
            primary_target,
            secondary_targets: vec![],
            progress: None,
            read_chunk: DEFAULT_READ_CHUNK,
        }
    }

    pub fn with_secondary(mut self, target: Arc<dyn ReplicationTarget>) -> Self {
        self.secondary_targets.push(target);
        self
    }

    pub fn with_read_chunk(mut self, read_chunk: usize) -> Self {
        self.read_chunk = read_chunk.max(1);
        self
    }

    pub fn with_progress(mut self, progress: crossbeam_channel::Sender<ReplicationEvent>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Reads every file once in the background. A slot is held from before the read
    /// until the bytes are in the channel, so a full channel stops further card
    /// reads, while the card itself stays free for previews during the wait.
    fn start_read_stream(&self, queue: Arc<ImportQueue>) -> mpsc::Receiver<ReadOutcome> {
        let (tx, rx) = mpsc::channel(self.read_channel_depth);
        let buffers = Arc::new(Semaphore::new(READS_IN_PROGRESS));
        let card = Arc::clone(&self.card);
        let chunk = self.read_chunk;

        tokio::spawn(async move {
            loop {
                let slot: OwnedSemaphorePermit = match Arc::clone(&buffers).acquire_owned().await {
                    Ok(slot) => slot,
                    Err(_) => return,
                };

                // Taken from the queue only now, so a file stays removable until the
                // moment its read can really start. If nobody is consuming any more,
                // stop reading the card.
                let file = tokio::select! {
                    file = queue.next() => match file {
                        Some(file) => file,
                        None => return,
                    },
                    _ = tx.closed() => return,
                };

                let (tx, card, queue) = (tx.clone(), Arc::clone(&card), Arc::clone(&queue));
                tokio::spawn(async move {
                    let _slot = slot;
                    let outcome = read_file(card, queue, chunk, file).await;
                    let _ = tx.send(outcome).await;
                });
            }
        });

        rx
    }

    /// Copies every planned file to all targets. The primary paces the card;
    /// secondaries take bytes from RAM when they keep up and from the primary's
    /// file when they don't. One bad file never stops the batch: its failure is
    /// recorded in the report.
    pub async fn replicate(&self, plan: ImportPlan) -> ReplicationReport {
        let queue = ImportQueue::new();
        for file in plan.files {
            queue.push(file);
        }
        queue.close();
        let mut report = self.replicate_queue(queue).await;
        for failure in plan.failures {
            report.record_failure(&self.progress, failure);
        }
        report
    }

    /// Copies whatever arrives on `queue`, until it is closed and empty. Files can be
    /// added or taken back while this runs.
    pub async fn replicate_queue(&self, queue: Arc<ImportQueue>) -> ReplicationReport {
        let started = Instant::now();
        let mut report = ReplicationReport::default();
        let mut rx = self.start_read_stream(Arc::clone(&queue));
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
            workers.spawn(run_secondary(
                Arc::clone(target),
                hot_rx,
                deferred_rx,
                self.progress.clone(),
                Arc::clone(&queue),
            ));
        }

        // Finished stores are settled as they finish, not just when the next file
        // arrives: the queue may stay idle for a long time, and the secondaries and the
        // progress display are waiting on them. While all write slots are busy nothing
        // new is received, which is what passes backpressure on to the reader.
        let mut reading = true;
        loop {
            tokio::select! {
                biased;
                finished = stores.next(), if !stores.is_empty() => {
                    if let Some((pending, result)) = finished {
                        settle_primary(
                            &mut report,
                            primary_id,
                            &queues,
                            &self.progress,
                            pending,
                            result,
                        );
                    }
                }
                outcome = rx.recv(), if reading && stores.len() < max_stores => {
                    let msg = match outcome {
                        Some(Ok(msg)) => msg,
                        Some(Err(failure)) => {
                            if !queue.is_stopped() {
                                report.record_failure(&self.progress, failure);
                            }
                            continue;
                        }
                        None => {
                            reading = false;
                            continue;
                        }
                    };

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
                        async move { target.store(&info, bytes).await },
                    );
                }
                else => break,
            }
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
                Err(err) => report.record_failure(
                    &self.progress,
                    ReplicationFailure {
                        source: None,
                        target: None,
                        error: ReplicationError::from(format!("secondary worker failed: {err}")),
                    },
                ),
            }
        }

        report
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

    use std::sync::RwLock;

    use super::*;
    use crate::{
        AssetKey, DateFolders, FileKind, ImportOptions, Organize, PhotoAsset, ScanResponse, Target,
        TargetKind, plan_import,
    };
    use chrono::{Local, NaiveDate};

    fn closed_queue(plan: ImportPlan) -> Arc<ImportQueue> {
        let queue = ImportQueue::new();
        for file in plan.files {
            queue.push(file);
        }
        queue.close();
        queue
    }

    fn plan_of(scan: &RwLock<ScanResponse>) -> ImportPlan {
        let options = ImportOptions {
            organize: Organize::ByDate(DateFolders::default()),
            ..Default::default()
        };
        plan_import(scan, Path::new("/"), &options, None).unwrap()
    }

    fn file_info(name: &str, src_hash: blake3::Hash) -> FileInfo {
        FileInfo {
            source: Path::new("card").join(name),
            relative: Path::new("2026/2026-05-22").join(name),
            modified: None,
            src_hash,
        }
    }

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
            ignored: vec![],
            file_meta: HashMap::new(),
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

        let engine = CopyEngine::new(CardGate::new(2), 4, local_target(&root, 4));
        let report = engine.replicate(plan_of(&scan)).await;

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
    async fn different_file_under_the_same_name_is_kept_beside_it() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let raw = write_source(&card, "IMG_0001.CR2", b"new bytes");
        let scan = scan_of(&[(&raw, FileKind::Raw)], true);
        let root = tmp.path().join("target");
        let existing = write_source(&dated_dir(&root), "IMG_0001.CR2", b"existing bytes");

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 4));
        let report = engine.replicate(plan_of(&scan)).await;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(fs::read(&existing).unwrap(), b"existing bytes");
        assert_eq!(
            fs::read(dated_dir(&root).join("IMG_0001-2.CR2")).unwrap(),
            b"new bytes"
        );
    }

    #[tokio::test]
    async fn running_again_counts_copied_files_as_done_and_writes_nothing() {
        let tmp = tempdir().unwrap();
        let first = write_source(&tmp.path().join("100CANON"), "IMG_0001.CR2", b"first");
        let second = write_source(&tmp.path().join("101CANON"), "IMG_0001.CR2", b"second");
        let scan = scan_of(&[(&first, FileKind::Raw), (&second, FileKind::Raw)], true);
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(CardGate::new(2), 4, local_target(&root, 4));
        let report = engine.replicate(plan_of(&scan)).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(report.verified.iter().all(|r| r.newly_written()));

        let report = engine.replicate(plan_of(&scan)).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), 2);
        assert!(report.verified.iter().all(|r| !r.newly_written()));
        assert_eq!(
            dir_entries(&dated_dir(&root)),
            ["IMG_0001-2.CR2", "IMG_0001.CR2"]
        );
    }

    #[tokio::test]
    async fn copy_keeps_the_source_modified_time() {
        let tmp = tempdir().unwrap();
        let raw = write_source(&tmp.path().join("card"), "IMG_0001.CR2", b"bytes");
        let shot = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&raw)
            .unwrap()
            .set_modified(shot)
            .unwrap();
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 1));
        engine
            .replicate(plan_of(&scan_of(&[(&raw, FileKind::Raw)], true)))
            .await;

        let copied = fs::metadata(dated_dir(&root).join("IMG_0001.CR2")).unwrap();
        assert_eq!(copied.modified().unwrap(), shot);
    }

    #[tokio::test]
    async fn hash_mismatch_publishes_nothing() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("target");
        let target = local_target(&root, 1);

        let result = target
            .store(
                &file_info("IMG_0001.CR2", blake3::hash(b"bytes the card held")),
                Arc::from(&b"bytes that landed"[..]),
            )
            .await;

        assert!(result.is_err());
        assert!(dir_entries(&dated_dir(&root)).is_empty());
    }

    #[tokio::test]
    async fn same_name_from_two_folders_keeps_both_intact() {
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

        let engine = CopyEngine::new(CardGate::new(2), 4, local_target(&root, 4));
        let report = engine.replicate(plan_of(&scan)).await;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), 2);
        assert_eq!(
            dir_entries(&dated_dir(&root)),
            ["IMG_0001-2.CR2", "IMG_0001.CR2"]
        );
        for replica in &report.verified {
            let ReplicaLocation::Local(copied) = replica.location() else {
                panic!("expected a local copy");
            };
            assert_eq!(
                fs::read(copied).unwrap(),
                fs::read(replica.source()).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn undated_frame_is_routed_by_its_file_date() {
        let tmp = tempdir().unwrap();
        let undated = write_source(&tmp.path().join("card"), "IMG_0001.CR2", b"bytes");
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 4));
        let report = engine
            .replicate(plan_of(&scan_of(&[(&undated, FileKind::Raw)], false)))
            .await;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let today = Local::now().date_naive();
        assert!(
            root.join(today.format("%Y").to_string())
                .join(today.format("%Y-%m-%d").to_string())
                .join("IMG_0001.CR2")
                .exists()
        );
    }

    #[tokio::test]
    async fn missing_file_is_recorded() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("card").join("IMG_0002.CR2");
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 4));
        let report = engine
            .replicate(plan_of(&scan_of(&[(&missing, FileKind::Raw)], true)))
            .await;

        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].source.as_deref(),
            Some(missing.as_path())
        );
    }

    #[tokio::test]
    async fn file_displaced_by_a_scan_collision_is_still_copied() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let kept = write_source(&card, "IMG_0001.JPG", b"kept");
        let displaced = write_source(&card, "IMG_0001.JPEG", b"displaced");
        let scan = scan_of(&[(&kept, FileKind::Jpeg)], true);
        let key = scan.read().unwrap().assets.keys().next().unwrap().clone();
        scan.write().unwrap().collisions.push(crate::Collision {
            key,
            kind: FileKind::Jpeg,
            kept: kept.clone(),
            displaced,
        });
        let root = tmp.path().join("target");

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 4));
        let report = engine.replicate(plan_of(&scan)).await;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            dir_entries(&dated_dir(&root)),
            ["IMG_0001.JPEG", "IMG_0001.JPG"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_import_keeps_finished_copies_and_leaves_nothing_half_written() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let paths: Vec<PathBuf> = (0..30)
            .map(|i| write_source(&card, &format!("IMG_{i:04}.CR2"), &vec![i as u8; 1 << 20]))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let root = tmp.path().join("target");
        let gate = CardGate::with_simulated_speed(1, Some(20_000_000));
        let engine = CopyEngine::new(gate, 2, local_target(&root, 2));
        let queue = closed_queue(plan_of(&scan_of(&files, true)));
        let stopper = {
            let (queue, dir) = (Arc::clone(&queue), dated_dir(&root));
            tokio::spawn(async move {
                wait_for_entries(&dir, 2).await;
                queue.stop();
            })
        };

        let report = engine.replicate_queue(queue).await;
        stopper.await.unwrap();

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let entries = dir_entries(&dated_dir(&root));
        assert!(entries.len() < 30, "{}", entries.len());
        assert_eq!(entries.len(), report.verified.len());
        assert!(
            entries.iter().all(|name| !name.ends_with(".tmp")),
            "{entries:?}"
        );
        for replica in &report.verified {
            let ReplicaLocation::Local(path) = replica.location() else {
                panic!("expected a local copy");
            };
            assert_eq!(blake3::hash(&fs::read(path).unwrap()), replica.hash());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunked_reads_give_the_same_file_and_hash_at_any_chunk_size() {
        let tmp = tempdir().unwrap();
        let contents: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let raw = write_source(&tmp.path().join("card"), "IMG_1.CR2", &contents);
        for chunk in [1000, 64 * 1024, 300_000, 1 << 24] {
            let root = tmp.path().join(format!("target{chunk}"));
            let engine =
                CopyEngine::new(CardGate::new(1), 4, local_target(&root, 1)).with_read_chunk(chunk);
            let report = engine
                .replicate(plan_of(&scan_of(&[(&raw, FileKind::Raw)], true)))
                .await;
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert_eq!(report.verified[0].hash(), blake3::hash(&contents));
            assert_eq!(
                fs::read(dated_dir(&root).join("IMG_1.CR2")).unwrap(),
                contents
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn waiting_on_a_full_channel_leaves_the_card_free_for_previews() {
        let tmp = tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..10)
            .map(|i| write_source(tmp.path(), &format!("IMG_{i:04}.CR2"), b"bytes"))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let scan = scan_of(&files, true);

        let gate = CardGate::new(1);
        let engine = CopyEngine::new(Arc::clone(&gate), 1, local_target(tmp.path(), 1));
        let rx = engine.start_read_stream(closed_queue(plan_of(&scan)));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Nothing is consuming, so the reader is stuck on the channel.
        let preview = tokio::task::spawn_blocking(move || gate.acquire_priority());
        tokio::time::timeout(Duration::from_secs(5), preview)
            .await
            .expect("a backed-up copy kept a preview read off the card")
            .unwrap();
        drop(rx);
    }

    /// Planned files for a card with these names, by name.
    fn queue_fixture(tmp: &Path, names: &[&str]) -> HashMap<String, PlannedFile> {
        let card = tmp.join("card");
        let paths: Vec<PathBuf> = names
            .iter()
            .map(|name| write_source(&card, name, name.as_bytes()))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        plan_of(&scan_of(&files, true))
            .files
            .into_iter()
            .map(|file| {
                let name = file
                    .source
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (name, file)
            })
            .collect()
    }

    async fn wait_for_entries(dir: &Path, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !dir.exists() || dir_entries(dir).len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{dir:?} never got {count} files"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_taken_back_before_its_read_is_never_copied() {
        let tmp = tempdir().unwrap();
        let mut files = queue_fixture(tmp.path(), &["IMG_1.CR2", "IMG_2.CR2", "IMG_3.CR2"]);
        let root = tmp.path().join("target");
        let queue = ImportQueue::new();
        for name in ["IMG_1.CR2", "IMG_2.CR2", "IMG_3.CR2"] {
            queue.push(files.remove(name).unwrap());
        }
        // Taken back before the copy starts, so it can't have been read.
        assert!(queue.remove(&tmp.path().join("card/IMG_2.CR2")));
        assert!(!queue.remove(&tmp.path().join("card/IMG_2.CR2")));
        queue.close();

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&root, 2));
        let report = engine.replicate_queue(queue).await;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), 2);
        assert_eq!(dir_entries(&dated_dir(&root)), ["IMG_1.CR2", "IMG_3.CR2"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_drained_queue_stays_open_for_files_ticked_later() {
        let tmp = tempdir().unwrap();
        let mut files = queue_fixture(tmp.path(), &["IMG_1.CR2", "IMG_2.CR2"]);
        let root = tmp.path().join("target");
        let queue = ImportQueue::new();
        queue.push(files.remove("IMG_1.CR2").unwrap());

        let engine = Arc::new(CopyEngine::new(CardGate::new(1), 4, local_target(&root, 2)));
        let running = {
            let (engine, queue) = (Arc::clone(&engine), Arc::clone(&queue));
            tokio::spawn(async move { engine.replicate_queue(queue).await })
        };
        wait_for_entries(&dated_dir(&root), 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(queue.waiting(), 0);
        assert!(
            !running.is_finished(),
            "the copy ended while the queue was open"
        );

        queue.push(files.remove("IMG_2.CR2").unwrap());
        wait_for_entries(&dated_dir(&root), 2).await;
        queue.close();

        let report = running.await.unwrap();
        assert_eq!(report.verified.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn progress_arrives_while_the_queue_sits_idle() {
        let tmp = tempdir().unwrap();
        let mut files = queue_fixture(tmp.path(), &["IMG_1.CR2"]);
        let queue = ImportQueue::new();
        queue.push(files.remove("IMG_1.CR2").unwrap());
        let (progress, events) = crossbeam_channel::unbounded();
        let engine = Arc::new(
            CopyEngine::new(CardGate::new(1), 4, local_target(&tmp.path().join("t"), 2))
                .with_progress(progress),
        );
        let running = {
            let (engine, queue) = (Arc::clone(&engine), Arc::clone(&queue));
            tokio::spawn(async move { engine.replicate_queue(queue).await })
        };

        // The queue is still open and idle, and the file must still be reported.
        let event =
            tokio::task::spawn_blocking(move || events.recv_timeout(Duration::from_secs(10)))
                .await
                .unwrap()
                .expect("no progress until the queue closed");
        assert!(matches!(event, ReplicationEvent::Verified { .. }));

        queue.close();
        running.await.unwrap();
    }

    #[tokio::test]
    async fn uploading_sends_only_what_is_listed_and_checks_it_against_the_card() {
        let tmp = tempdir().unwrap();
        let good = write_source(tmp.path(), "good.CR2", b"the card held this");
        let damaged = write_source(tmp.path(), "damaged.CR2", b"bit rot");
        let unlisted = write_source(tmp.path(), "unlisted.CR2", b"never chosen");
        let root = tmp.path().join("drive");
        let upload = |local: &Path, card: &[u8]| UploadFile {
            info: file_info(
                local.file_name().unwrap().to_str().unwrap(),
                blake3::hash(card),
            ),
            local: local.to_path_buf(),
        };

        let report = upload_copies(
            local_target(&root, 2),
            vec![
                upload(&good, b"the card held this"),
                upload(&damaged, b"what the card really held"),
            ],
            None,
        )
        .await;

        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(dir_entries(&dated_dir(&root)), ["good.CR2"]);
        assert!(unlisted.exists());
    }

    #[tokio::test]
    async fn a_closed_queue_takes_nothing_and_a_waiting_file_is_not_queued_twice() {
        let tmp = tempdir().unwrap();
        let mut files = queue_fixture(tmp.path(), &["IMG_1.CR2", "IMG_2.CR2"]);
        let queue = ImportQueue::new();
        let first = files.remove("IMG_1.CR2").unwrap();
        assert!(queue.push(first.clone()));
        assert!(!queue.push(first));
        assert_eq!(queue.waiting(), 1);
        queue.close();
        assert!(!queue.push(files.remove("IMG_2.CR2").unwrap()));
        assert!(queue.next().await.is_some());
        assert!(queue.next().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_channel_stops_card_reads() {
        const FILES: usize = 20;
        const DEPTH: usize = 2;

        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        let paths: Vec<PathBuf> = (0..FILES)
            .map(|i| write_source(&card, &format!("IMG_{i:04}.CR2"), b"bytes"))
            .collect();
        let files: Vec<(&Path, FileKind)> =
            paths.iter().map(|p| (p.as_path(), FileKind::Raw)).collect();
        let scan = scan_of(&files, true);

        let engine = CopyEngine::new(CardGate::new(1), DEPTH, local_target(tmp.path(), 1));
        let mut rx = engine.start_read_stream(closed_queue(plan_of(&scan)));

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
            read <= DEPTH + READS_IN_PROGRESS,
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
            file: &FileInfo,
            bytes: Arc<[u8]>,
        ) -> Result<VerifiedReplica, ReplicationError> {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(running, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            VerifiedReplica::new(
                file.source.clone(),
                self.id(),
                ReplicaLocation::Local(file.source.clone()),
                bytes.len() as u64,
                true,
                file.src_hash,
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

        let engine = CopyEngine::new(CardGate::new(4), 8, target.clone());
        let report = engine.replicate(plan_of(&scan_of(&files, true))).await;

        assert_eq!(report.verified.len(), 30);
        let peak = target.peak.load(Ordering::SeqCst);
        assert!((1..=3).contains(&peak), "{peak}");
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
            file: &FileInfo,
            bytes: Arc<[u8]>,
        ) -> Result<VerifiedReplica, ReplicationError> {
            let _open = self.gate.acquire().await?;
            self.inner.store(file, bytes).await
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
            CopyEngine::new(CardGate::new(1), 1, local_target(&primary_root, 2))
                .with_secondary(Arc::new(secondary)),
        );

        let scan = Arc::new(scan);
        let running = {
            let (engine, scan) = (Arc::clone(&engine), Arc::clone(&scan));
            tokio::spawn(async move { engine.replicate(plan_of(&scan)).await })
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
                info: file_info("IMG_0001.CR2", blake3::hash(b"what the card held")),
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
        // A file where the root should be makes every primary store fail.
        let primary_root = write_source(tmp.path(), "not-a-directory", b"");
        let secondary_root = tmp.path().join("secondary");
        let secondary = Arc::new(LocalTarget::new(
            TargetId(9),
            Target {
                root: secondary_root.clone(),
                kind: TargetKind::LocalNvme,
                write_permits: 1,
            },
        ));

        let engine = CopyEngine::new(CardGate::new(1), 4, local_target(&primary_root, 1))
            .with_secondary(secondary);
        let report = engine.replicate(plan_of(&scan)).await;

        // The secondary had room, so it still got its copy from RAM.
        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.verified[0].target(), TargetId(9));
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            fs::read(dated_dir(&secondary_root).join("IMG_0001.CR2")).unwrap(),
            b"new bytes"
        );
    }
}
