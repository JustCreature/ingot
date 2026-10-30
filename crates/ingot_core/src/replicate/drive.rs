//! Google Drive as a replication target.
//!
//! Follows what rclone's Drive backend learned the hard way: HTTP/1.1 rather than
//! HTTP/2, four transfers by default, a shared pacer with truncated exponential
//! backoff, and folder ids cached because Drive allows duplicate names. Unlike
//! rclone it never chunks: the whole file is already in RAM, so one resumable PUT is
//! the fewest round trips, and an interrupted one resumes from what Google received.
//!
//! Ingot touches nothing outside its own [`DRIVE_ROOT_FOLDER`]. This is enforced
//! three ways: every call that creates or lists takes a [`ContainedFolder`], which
//! can only be obtained from that root or from a folder already inside it; this
//! module has no code that deletes, moves, renames or updates anything; and the
//! `drive.file` scope stops the token from seeing files Ingot didn't create.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use md5::{Digest, Md5};
use reqwest::{RequestBuilder, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::json;

use crate::replicate::{
    DestinationClaims, FileInfo, MAX_NAME_ATTEMPTS, ReplicaLocation, ReplicationError,
    ReplicationTarget, TargetId, VerifiedReplica, candidate_name,
    drive_auth::{DriveCredentials, TOKEN_URL, TokenSource},
};

/// The one folder in My Drive that Ingot works in. Fixed on purpose: there is no
/// setting that points Ingot anywhere else.
pub const DRIVE_ROOT_FOLDER: &str = "__ingot_backups";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str = "id,name,size,md5Checksum";
/// How long one upload may take at the slowest rate worth waiting for.
const SLOWEST_RATE_BYTES_PER_SEC: u64 = 128 * 1024;
const UPLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(60);
const API_TIMEOUT: Duration = Duration::from_secs(60);

/// A Drive folder known to be [`DRIVE_ROOT_FOLDER`] or inside it. The field is
/// private and the only constructors are `root_folder` and `child_folder`, so
/// nothing can create or list anything under a folder id from elsewhere.
#[derive(Clone, Debug)]
struct ContainedFolder(String);

#[derive(Clone, Debug)]
pub struct DriveConfig {
    /// Simultaneous uploads. rclone's default of 4 suits Drive's rate limits.
    pub concurrent_uploads: usize,
    /// Average gap between API calls once the burst is used up.
    pub min_call_interval: Duration,
    pub call_burst: u32,
    /// Attempts per operation before a file is reported as failed.
    pub max_attempts: u32,
    /// The first backoff; it doubles up to 16 times this.
    pub first_backoff: Duration,
    pub(crate) api_base: String,
    pub(crate) upload_base: String,
    pub(crate) token_url: String,
}

impl Default for DriveConfig {
    fn default() -> Self {
        DriveConfig {
            concurrent_uploads: 4,
            min_call_interval: Duration::from_millis(100),
            call_burst: 100,
            max_attempts: 10,
            first_backoff: Duration::from_secs(1),
            api_base: "https://www.googleapis.com/drive/v3".to_string(),
            upload_base: "https://www.googleapis.com/upload/drive/v3".to_string(),
            token_url: TOKEN_URL.to_string(),
        }
    }
}

/// Spaces API calls out while allowing a burst. Shared by all uploads.
struct Pacer {
    next_slot: Mutex<Instant>,
    interval: Duration,
    burst: u32,
}

impl Pacer {
    fn new(interval: Duration, burst: u32) -> Self {
        // Starting a full burst in the past makes the whole burst available at once.
        let now = Instant::now();
        Pacer {
            next_slot: Mutex::new(now.checked_sub(interval * burst).unwrap_or(now)),
            interval,
            burst,
        }
    }

    async fn wait(&self) {
        let slot = {
            let mut next = self
                .next_slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let earliest = now.checked_sub(self.interval * self.burst).unwrap_or(now);
            let slot = (*next).max(earliest);
            *next = slot + self.interval;
            slot
        };
        tokio::time::sleep_until(slot.into()).await;
    }
}

/// What to do about a failed call.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Worth another try, after this long if the server said.
    Retry(Option<Duration>),
    /// A new access token may fix it.
    Reauthorize,
    /// No file can succeed until the user does something, like freeing space.
    StopTarget,
    GiveUp,
}

#[derive(Deserialize, Default)]
struct ErrorBody {
    #[serde(default)]
    error: ErrorDetail,
}

#[derive(Deserialize, Default)]
struct ErrorDetail {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ErrorReason>,
}

#[derive(Deserialize)]
struct ErrorReason {
    #[serde(default)]
    reason: String,
}

fn classify(status: StatusCode, reason: &str, retry_after: Option<Duration>) -> Verdict {
    match status.as_u16() {
        401 => Verdict::Reauthorize,
        429 => Verdict::Retry(retry_after),
        500..=599 => Verdict::Retry(retry_after),
        403 => match reason {
            "rateLimitExceeded" | "userRateLimitExceeded" | "sharingRateLimitExceeded" => {
                Verdict::Retry(retry_after)
            }
            "storageQuotaExceeded" | "quotaExceeded" | "uploadLimitExceeded" => Verdict::StopTarget,
            _ => Verdict::GiveUp,
        },
        _ => Verdict::GiveUp,
    }
}

fn backoff(first: Duration, attempt: u32) -> Duration {
    // attempt 1,2,3,4,5,6.. -> 1,2,4,8,16,16 times the first backoff, plus jitter
    // so uploads that hit a limit together don't all return together.
    let base = first * (1u32 << attempt.clamp(1, 5).saturating_sub(1));
    base + first.mul_f64(rand::random::<f64>())
}

fn query_literal(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

#[derive(Deserialize, Debug, Clone)]
struct RemoteFile {
    id: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default, rename = "md5Checksum")]
    md5: Option<String>,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<RemoteFile>,
}

pub struct DriveTarget {
    id: TargetId,
    config: DriveConfig,
    http: reqwest::Client,
    tokens: TokenSource,
    pacer: Pacer,
    uploads: tokio::sync::Semaphore,
    /// Folder ids by path below the root. The lock is held while resolving: Drive
    /// would happily create two folders with the same name if two uploads raced.
    folders: tokio::sync::Mutex<HashMap<PathBuf, ContainedFolder>>,
    claims: DestinationClaims,
    /// Set once Drive says no file can succeed, so the rest fail at once instead of
    /// each spending minutes in backoff.
    stopped: Mutex<Option<String>>,
}

enum Upload {
    Done(RemoteFile),
    /// The session is gone. Check the name again, since the file may have landed.
    Restart,
}

impl DriveTarget {
    pub fn new(
        id: TargetId,
        config: DriveConfig,
        credentials: DriveCredentials,
    ) -> Result<Self, ReplicationError> {
        // rclone turns HTTP/2 off for Drive by default: it has hit unexplained
        // failures on it, and one connection per transfer also uploads faster.
        let http = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        let tokens = TokenSource::new(credentials, config.token_url.clone(), http.clone());
        let pacer = Pacer::new(config.min_call_interval, config.call_burst.max(1));
        let uploads = tokio::sync::Semaphore::new(config.concurrent_uploads.max(1));
        Ok(DriveTarget {
            id,
            config,
            http,
            tokens,
            pacer,
            uploads,
            folders: tokio::sync::Mutex::new(HashMap::new()),
            claims: DestinationClaims::default(),
            stopped: Mutex::new(None),
        })
    }

    fn check_stopped(&self) -> Result<(), ReplicationError> {
        match &*self.stopped.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(reason) => Err(ReplicationError::from(reason.clone())),
            None => Ok(()),
        }
    }

    fn stop(&self, reason: String) -> ReplicationError {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.clone());
        ReplicationError::from(reason)
    }

    /// Runs one API request with pacing, auth and retries. `request` is called again
    /// for every attempt.
    async fn call(
        &self,
        what: &str,
        request: impl Fn(&str) -> RequestBuilder,
    ) -> Result<Response, ReplicationError> {
        let mut last_error = String::new();
        for attempt in 1..=self.config.max_attempts.max(1) {
            self.check_stopped()?;
            self.pacer.wait().await;
            let token = self.tokens.access_token().await?;
            let verdict = match request(&token).timeout(API_TIMEOUT).send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let (verdict, description) = read_failure(response).await;
                    last_error = format!("{what}: {description}");
                    verdict
                }
                Err(err) => {
                    last_error = format!("{what}: {err}");
                    Verdict::Retry(None)
                }
            };
            match verdict {
                Verdict::Reauthorize => self.tokens.invalidate(&token).await,
                Verdict::Retry(after) => {
                    tokio::time::sleep(
                        after.unwrap_or_else(|| backoff(self.config.first_backoff, attempt)),
                    )
                    .await;
                }
                Verdict::StopTarget => return Err(self.stop(last_error)),
                Verdict::GiveUp => return Err(ReplicationError::from(last_error)),
            }
        }
        Err(ReplicationError::from(format!(
            "gave up after {} attempts: {last_error}",
            self.config.max_attempts
        )))
    }

    /// Lists folder children by name. Private to the two callers below, which are
    /// the only places a raw folder id reaches the API.
    async fn query_children(
        &self,
        name: &str,
        parent_id: &str,
        extra: &str,
    ) -> Result<Vec<RemoteFile>, ReplicationError> {
        let query = format!(
            "name = '{}' and '{}' in parents and trashed = false{extra}",
            query_literal(name),
            query_literal(parent_id)
        );
        let url = format!("{}/files", self.config.api_base);
        let fields = format!("files({FILE_FIELDS})");
        let response = self
            .call("look up file", |token| {
                self.http.get(&url).bearer_auth(token).query(&[
                    ("q", query.as_str()),
                    ("fields", fields.as_str()),
                    ("spaces", "drive"),
                    ("pageSize", "100"),
                ])
            })
            .await?;
        Ok(response.json::<FileList>().await?.files)
    }

    async fn create_folder_under(
        &self,
        name: &str,
        parent_id: &str,
    ) -> Result<String, ReplicationError> {
        let url = format!("{}/files", self.config.api_base);
        let body = json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent_id]});
        let response = self
            .call("create folder", |token| {
                self.http
                    .post(&url)
                    .bearer_auth(token)
                    .query(&[("fields", "id")])
                    .json(&body)
            })
            .await?;
        Ok(response.json::<RemoteFile>().await?.id)
    }

    /// The only code that looks at My Drive itself, and it only ever asks for or
    /// creates the one fixed folder.
    async fn root_folder(&self) -> Result<ContainedFolder, ReplicationError> {
        let is_folder = format!(" and mimeType = '{FOLDER_MIME}'");
        let found = self
            .query_children(DRIVE_ROOT_FOLDER, "root", &is_folder)
            .await?;
        let id = match found.into_iter().next() {
            Some(found) => found.id,
            None => self.create_folder_under(DRIVE_ROOT_FOLDER, "root").await?,
        };
        Ok(ContainedFolder(id))
    }

    async fn child_folder(
        &self,
        name: &str,
        parent: &ContainedFolder,
    ) -> Result<ContainedFolder, ReplicationError> {
        let is_folder = format!(" and mimeType = '{FOLDER_MIME}'");
        let found = self.query_children(name, &parent.0, &is_folder).await?;
        let id = match found.into_iter().next() {
            Some(found) => found.id,
            None => self.create_folder_under(name, &parent.0).await?,
        };
        Ok(ContainedFolder(id))
    }

    async fn find_by_name(
        &self,
        name: &str,
        parent: &ContainedFolder,
    ) -> Result<Vec<RemoteFile>, ReplicationError> {
        self.query_children(name, &parent.0, "").await
    }

    /// Resolves a folder below the root. Only plain folder names are accepted, so a
    /// path can't climb out of the root.
    async fn folder_for(&self, relative_dir: &Path) -> Result<ContainedFolder, ReplicationError> {
        let mut folders = self.folders.lock().await;
        let mut path = PathBuf::new();
        let mut parent = match folders.get(&path) {
            Some(folder) => folder.clone(),
            None => {
                let folder = self.root_folder().await?;
                folders.insert(path.clone(), folder.clone());
                folder
            }
        };
        for component in relative_dir.components() {
            let Component::Normal(name) = component else {
                return Err(ReplicationError::from(format!(
                    "refusing a path that leaves {DRIVE_ROOT_FOLDER}: {}",
                    relative_dir.display()
                )));
            };
            path.push(name);
            parent = match folders.get(&path) {
                Some(folder) => folder.clone(),
                None => {
                    let folder = self.child_folder(&name.to_string_lossy(), &parent).await?;
                    folders.insert(path.clone(), folder.clone());
                    folder
                }
            };
        }
        Ok(parent)
    }

    async fn start_session(
        &self,
        name: &str,
        parent: &ContainedFolder,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Result<String, ReplicationError> {
        let url = format!("{}/files", self.config.upload_base);
        let mut body = json!({"name": name, "parents": [parent.0]});
        if let Some(modified) = modified {
            body["modifiedTime"] =
                json!(DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Millis, true));
        }
        let size_header = size.to_string();
        let response = self
            .call("start upload", |token| {
                self.http
                    .post(&url)
                    .bearer_auth(token)
                    .query(&[("uploadType", "resumable"), ("fields", FILE_FIELDS)])
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", size_header.as_str())
                    .json(&body)
            })
            .await?;
        let location = response
            .headers()
            .get(header::LOCATION)
            .ok_or("Drive started an upload without giving its address")?
            .to_str()?;
        // The access token goes to this address, so it must be Google's upload host.
        let session = reqwest::Url::parse(location)?;
        let expected = reqwest::Url::parse(&self.config.upload_base)?;
        if session.host_str() != expected.host_str() || session.port() != expected.port() {
            return Err(ReplicationError::from(format!(
                "Drive gave an upload address on an unexpected host: {:?}",
                session.host_str()
            )));
        }
        Ok(location.to_string())
    }

    /// Sends `bytes` to a session, resuming from what Google has after anything goes
    /// wrong. `Bytes` shares the file's one buffer, so retries copy nothing.
    async fn send_to_session(
        &self,
        session: &str,
        bytes: &Bytes,
    ) -> Result<Upload, ReplicationError> {
        let total = bytes.len() as u64;
        let mut offset = 0u64;
        let mut failures = 0;
        let mut needs_status_check = false;
        let timeout = UPLOAD_BASE_TIMEOUT + Duration::from_secs(total / SLOWEST_RATE_BYTES_PER_SEC);

        loop {
            self.check_stopped()?;
            self.pacer.wait().await;
            let token = self.tokens.access_token().await?;
            let request = self.http.put(session).bearer_auth(&token).timeout(timeout);
            let request = if needs_status_check || total == 0 {
                request
                    .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                    .header(header::CONTENT_LENGTH, 0)
            } else {
                request
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {offset}-{}/{total}", total - 1),
                    )
                    .body(bytes.slice(offset as usize..))
            };

            let (verdict, description) = match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::OK || status == StatusCode::CREATED {
                        return Ok(Upload::Done(response.json().await?));
                    }
                    if status.as_u16() == 308 {
                        let received = received_bytes(&response);
                        if received > offset {
                            failures = 0;
                        }
                        offset = received;
                        needs_status_check = false;
                        continue;
                    }
                    if matches!(status.as_u16(), 404 | 410) {
                        return Ok(Upload::Restart);
                    }
                    let (verdict, description) = read_failure(response).await;
                    if verdict == Verdict::GiveUp {
                        // Any other 4xx ends a resumable session, per Google's guide.
                        return Ok(Upload::Restart);
                    }
                    (verdict, description)
                }
                Err(err) => (Verdict::Retry(None), err.to_string()),
            };

            failures += 1;
            if failures >= self.config.max_attempts.max(1) {
                return Err(ReplicationError::from(format!(
                    "upload gave up after {failures} attempts: {description}"
                )));
            }
            match verdict {
                Verdict::Reauthorize => self.tokens.invalidate(&token).await,
                Verdict::Retry(after) => {
                    tokio::time::sleep(
                        after.unwrap_or_else(|| backoff(self.config.first_backoff, failures)),
                    )
                    .await;
                }
                Verdict::StopTarget => return Err(self.stop(description)),
                Verdict::GiveUp => unreachable!("handled above"),
            }
            needs_status_check = true;
        }
    }

    async fn checksum_of(&self, file: RemoteFile) -> Result<RemoteFile, ReplicationError> {
        if file.md5.is_some() {
            return Ok(file);
        }
        let url = format!("{}/files/{}", self.config.api_base, file.id);
        let response = self
            .call("read checksum", |token| {
                self.http
                    .get(&url)
                    .bearer_auth(token)
                    .query(&[("fields", FILE_FIELDS)])
            })
            .await?;
        Ok(response.json().await?)
    }
}

fn received_bytes(response: &Response) -> u64 {
    response
        .headers()
        .get(header::RANGE)
        .and_then(|range| range.to_str().ok())
        .and_then(|range| range.rsplit('-').next())
        .and_then(|last| last.parse::<u64>().ok())
        .map_or(0, |last| last + 1)
}

async fn read_failure(response: Response) -> (Verdict, String) {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .map(Duration::from_secs);
    let body: ErrorBody = response.json().await.unwrap_or_default();
    let reason = body
        .error
        .errors
        .first()
        .map_or("", |first| first.reason.as_str());
    let verdict = classify(status, reason, retry_after);
    (
        verdict,
        format!("{status} {reason}: {}", body.error.message),
    )
}

fn same_contents(file: &RemoteFile, size: u64, md5: &str) -> bool {
    file.size.as_deref() == Some(size.to_string().as_str()) && file.md5.as_deref() == Some(md5)
}

#[async_trait::async_trait]
impl ReplicationTarget for DriveTarget {
    fn id(&self) -> TargetId {
        self.id
    }

    fn max_concurrent_writes(&self) -> usize {
        self.config.concurrent_uploads.max(1)
    }

    async fn store(
        &self,
        file: &FileInfo,
        bytes: Arc<[u8]>,
    ) -> Result<VerifiedReplica, ReplicationError> {
        let _permit = self.uploads.acquire().await?;
        self.check_stopped()?;

        // The chain of proof: these bytes match the card, and Drive's MD5 of what it
        // stored matches these bytes. Drive offers no BLAKE3.
        let (observed, md5) = {
            let bytes = Arc::clone(&bytes);
            tokio::task::spawn_blocking(move || {
                let md5 = Md5::digest(&bytes[..]);
                let md5: String = md5.iter().map(|byte| format!("{byte:02x}")).collect();
                (blake3::hash(&bytes), md5)
            })
            .await?
        };
        if observed != file.src_hash {
            return Err(ReplicationError::from(format!(
                "verification failed for {}: expected {}, observed {observed}",
                file.source.display(),
                file.src_hash
            )));
        }

        let size = bytes.len() as u64;
        let (Some(file_name), dir) = (
            file.relative.file_name(),
            file.relative.parent().unwrap_or(Path::new("")),
        ) else {
            return Err(ReplicationError::from(format!(
                "not a file path: {}",
                file.relative.display()
            )));
        };
        let parent = self.folder_for(dir).await?;
        let body = Bytes::from_owner(Arc::clone(&bytes));
        let proof = |remote: RemoteFile, newly_written| {
            VerifiedReplica::new(
                file.source.clone(),
                self.id,
                ReplicaLocation::Remote(remote.id),
                size,
                newly_written,
                file.src_hash,
                observed,
            )
        };

        let mut attempt = 1;
        let mut restarts = 0;
        while attempt <= MAX_NAME_ATTEMPTS {
            let name_os = candidate_name(file_name, attempt);
            let name = name_os.to_string_lossy();
            let _claim = self.claims.claim(dir.join(&name_os)).await;

            let existing = self.find_by_name(&name, &parent).await?;
            if let Some(same) = existing.iter().find(|f| same_contents(f, size, &md5)) {
                return proof(same.clone(), false);
            }
            if !existing.is_empty() {
                attempt += 1;
                continue;
            }

            let session = self
                .start_session(&name, &parent, size, file.modified)
                .await?;
            match self.send_to_session(&session, &body).await? {
                Upload::Done(remote) => {
                    let remote = self.checksum_of(remote).await?;
                    if !same_contents(&remote, size, &md5) {
                        return Err(ReplicationError::from(format!(
                            "Drive stored different bytes for {}: file id {}, expected MD5 {md5}, got {:?}",
                            file.source.display(),
                            remote.id,
                            remote.md5
                        )));
                    }
                    return proof(remote, true);
                }
                Upload::Restart => {
                    restarts += 1;
                    if restarts >= self.config.max_attempts.max(1) {
                        return Err(ReplicationError::from(format!(
                            "upload of {} kept losing its session",
                            file.source.display()
                        )));
                    }
                }
            }
        }

        Err(ReplicationError::from(format!(
            "no free name for {} after {MAX_NAME_ATTEMPTS} attempts",
            file.relative.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use axum::{
        Json, Router,
        body::Bytes as Body,
        extract::{Path as UrlPath, Query, State},
        http::{HeaderMap, StatusCode as Code},
        response::{IntoResponse, Response as AxumResponse},
        routing::{get, post, put},
    };
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone, Debug)]
    struct StoredFile {
        id: String,
        name: String,
        parent: String,
        mime: Option<String>,
        bytes: Vec<u8>,
        modified: Option<String>,
    }

    #[derive(Clone, Debug)]
    enum Fault {
        Status(u16, &'static str),
        RetryAfter(u16, &'static str, u64),
        /// Keeps half the bytes, then fails the request.
        DropHalfWay,
        /// Finishes the upload but loses the response.
        LoseResponse,
        /// Stores different bytes than it was sent.
        Corrupt,
    }

    struct Session {
        name: String,
        parent: String,
        modified: Option<String>,
        total: u64,
        received: Vec<u8>,
    }

    #[derive(Default)]
    struct Mock {
        files: Vec<StoredFile>,
        sessions: HashMap<String, Session>,
        next_id: usize,
        put_faults: VecDeque<Fault>,
        api_faults: VecDeque<Fault>,
        session_faults: VecDeque<Fault>,
        valid_token: Option<String>,
        expire_token_once: bool,
        refreshes: usize,
        full_body_puts: usize,
        put_offsets: Vec<u64>,
        peak_uploads: usize,
        foreign_session: bool,
    }

    type Shared = Arc<(Mutex<Mock>, AtomicUsize)>;

    fn error(status: u16, reason: &str) -> AxumResponse {
        (
            Code::from_u16(status).unwrap(),
            Json(json!({"error": {"message": "mock", "errors": [{"reason": reason}]}})),
        )
            .into_response()
    }

    fn fault_response(fault: &Fault) -> AxumResponse {
        match fault {
            Fault::Status(status, reason) => error(*status, reason),
            Fault::RetryAfter(status, reason, secs) => {
                let mut response = error(*status, reason);
                response
                    .headers_mut()
                    .insert("retry-after", secs.to_string().parse().unwrap());
                response
            }
            _ => error(500, "backendError"),
        }
    }

    fn authorized(mock: &mut Mock, headers: &HeaderMap) -> bool {
        if mock.expire_token_once {
            mock.expire_token_once = false;
            mock.valid_token = None;
            return false;
        }
        let given = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        given.is_some() && given == mock.valid_token.as_deref()
    }

    fn md5_hex(bytes: &[u8]) -> String {
        Md5::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn file_json(file: &StoredFile) -> serde_json::Value {
        json!({
            "id": file.id,
            "name": file.name,
            "size": file.bytes.len().to_string(),
            "md5Checksum": md5_hex(&file.bytes),
        })
    }

    async fn token(State(state): State<Shared>) -> impl IntoResponse {
        let mut mock = state.0.lock().unwrap();
        mock.refreshes += 1;
        let value = format!("token-{}", mock.refreshes);
        mock.valid_token = Some(value.clone());
        Json(json!({"access_token": value, "expires_in": 3600}))
    }

    fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let from = text.find(start)? + start.len();
        Some(&text[from..from + text[from..].find(end)?])
    }

    async fn list(
        State(state): State<Shared>,
        headers: HeaderMap,
        Query(params): Query<HashMap<String, String>>,
    ) -> AxumResponse {
        let mut mock = state.0.lock().unwrap();
        if !authorized(&mut mock, &headers) {
            return error(401, "authError");
        }
        if let Some(fault) = mock.api_faults.pop_front() {
            return fault_response(&fault);
        }
        let query = &params["q"];
        let name = between(query, "name = '", "' and '")
            .unwrap()
            .replace("\\'", "'");
        let parent = between(query, "and '", "' in parents").unwrap();
        let folders_only = query.contains("mimeType");
        let files: Vec<_> = mock
            .files
            .iter()
            .filter(|f| f.name == name && f.parent == parent && (!folders_only || f.mime.is_some()))
            .map(file_json)
            .collect();
        Json(json!({"files": files})).into_response()
    }

    async fn create_folder(
        State(state): State<Shared>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> AxumResponse {
        let mut mock = state.0.lock().unwrap();
        if !authorized(&mut mock, &headers) {
            return error(401, "authError");
        }
        mock.next_id += 1;
        let id = format!("id{}", mock.next_id);
        mock.files.push(StoredFile {
            id: id.clone(),
            name: body["name"].as_str().unwrap().to_string(),
            parent: body["parents"][0].as_str().unwrap().to_string(),
            mime: body["mimeType"].as_str().map(String::from),
            bytes: vec![],
            modified: None,
        });
        Json(json!({"id": id})).into_response()
    }

    async fn start_upload(
        State(state): State<Shared>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> AxumResponse {
        let mut mock = state.0.lock().unwrap();
        if !authorized(&mut mock, &headers) {
            return error(401, "authError");
        }
        if let Some(fault) = mock.session_faults.pop_front() {
            return fault_response(&fault);
        }
        mock.next_id += 1;
        let session = format!("s{}", mock.next_id);
        mock.sessions.insert(
            session.clone(),
            Session {
                name: body["name"].as_str().unwrap().to_string(),
                parent: body["parents"][0].as_str().unwrap().to_string(),
                modified: body["modifiedTime"].as_str().map(String::from),
                total: headers["x-upload-content-length"]
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
                received: vec![],
            },
        );
        mock.peak_uploads = mock.peak_uploads.max(mock.sessions.len());
        let host = if mock.foreign_session {
            "elsewhere.example".to_string()
        } else {
            headers["host"].to_str().unwrap().to_string()
        };
        let mut response = Code::OK.into_response();
        response.headers_mut().insert(
            "location",
            format!("http://{host}/session/{session}").parse().unwrap(),
        );
        response
    }

    fn complete(mock: &mut Mock, session_id: &str, corrupt: bool) -> serde_json::Value {
        let session = mock.sessions.remove(session_id).unwrap();
        mock.next_id += 1;
        let mut bytes = session.received;
        if corrupt {
            bytes[0] ^= 0xff;
        }
        let file = StoredFile {
            id: format!("id{}", mock.next_id),
            name: session.name,
            parent: session.parent,
            mime: None,
            bytes,
            modified: session.modified,
        };
        let value = file_json(&file);
        mock.files.push(file);
        value
    }

    async fn upload_bytes(
        State(state): State<Shared>,
        UrlPath(session_id): UrlPath<String>,
        headers: HeaderMap,
        body: Body,
    ) -> AxumResponse {
        let mut mock = state.0.lock().unwrap();
        let Some(session) = mock.sessions.get(&session_id) else {
            return error(404, "notFound");
        };
        let total = session.total;
        let have = session.received.len() as u64;
        let range = headers["content-range"].to_str().unwrap().to_string();

        if range.starts_with("bytes */") {
            if have == total && total > 0 {
                return (Code::CREATED, Json(complete(&mut mock, &session_id, false)))
                    .into_response();
            }
            if total == 0 {
                return (Code::CREATED, Json(complete(&mut mock, &session_id, false)))
                    .into_response();
            }
            return incomplete(have);
        }

        let start: u64 = between(&range, "bytes ", "-").unwrap().parse().unwrap();
        assert_eq!(start, have, "client resumed from the wrong offset");
        mock.put_offsets.push(start);
        mock.full_body_puts += 1;

        let fault = mock.put_faults.pop_front();
        let session = mock.sessions.get_mut(&session_id).unwrap();
        match fault {
            Some(Fault::DropHalfWay) => {
                let half = body.len() / 2;
                session.received.extend_from_slice(&body[..half]);
                return error(503, "backendError");
            }
            Some(Fault::LoseResponse) => {
                session.received.extend_from_slice(&body);
                return error(503, "backendError");
            }
            Some(Fault::Corrupt) => {
                session.received.extend_from_slice(&body);
                return (Code::OK, Json(complete(&mut mock, &session_id, true))).into_response();
            }
            Some(other) => return fault_response(&other),
            None => {}
        }
        session.received.extend_from_slice(&body);
        (Code::OK, Json(complete(&mut mock, &session_id, false))).into_response()
    }

    fn incomplete(have: u64) -> AxumResponse {
        let mut response = Code::from_u16(308).unwrap().into_response();
        if have > 0 {
            response
                .headers_mut()
                .insert("range", format!("bytes=0-{}", have - 1).parse().unwrap());
        }
        response
    }

    async fn get_file(State(state): State<Shared>, UrlPath(id): UrlPath<String>) -> AxumResponse {
        let mock = state.0.lock().unwrap();
        match mock.files.iter().find(|f| f.id == id) {
            Some(file) => Json(file_json(file)).into_response(),
            None => error(404, "notFound"),
        }
    }

    async fn mock_drive() -> (Shared, DriveConfig) {
        let state: Shared = Arc::new((Mutex::new(Mock::default()), AtomicUsize::new(0)));
        let app = Router::new()
            .route("/token", post(token))
            .route("/api/files", get(list).post(create_folder))
            .route("/api/files/{id}", get(get_file))
            .route("/upload/files", post(start_upload))
            .route("/session/{id}", put(upload_bytes))
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let config = DriveConfig {
            first_backoff: Duration::from_millis(1),
            min_call_interval: Duration::from_millis(0),
            api_base: format!("{base}/api"),
            upload_base: format!("{base}/upload"),
            token_url: format!("{base}/token"),
            ..Default::default()
        };
        (state, config)
    }

    fn target(config: DriveConfig) -> DriveTarget {
        DriveTarget::new(
            TargetId(7),
            config,
            DriveCredentials {
                client_id: "id".into(),
                client_secret: None,
                refresh_token: "refresh".into(),
            },
        )
        .unwrap()
    }

    fn info(relative: &str, bytes: &[u8]) -> FileInfo {
        FileInfo {
            source: Path::new("card").join(relative),
            relative: PathBuf::from(relative),
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            src_hash: blake3::hash(bytes),
        }
    }

    fn photos(state: &Shared) -> Vec<StoredFile> {
        let mut files: Vec<_> = state
            .0
            .lock()
            .unwrap()
            .files
            .iter()
            .filter(|f| f.mime.is_none())
            .cloned()
            .collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files
    }

    const BYTES: &[u8] = b"pretend this is a thirty megabyte raw file";

    #[tokio::test]
    async fn uploads_into_the_planned_folders_and_reuses_them() {
        let (state, config) = mock_drive().await;
        let drive = target(config);

        let first = drive
            .store(&info("Wedding/2026/IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();
        drive
            .store(&info("Wedding/2026/IMG_2.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert!(first.newly_written());
        assert!(matches!(first.location(), ReplicaLocation::Remote(_)));
        let mock = state.0.lock().unwrap();
        let mut folders: Vec<&str> = mock
            .files
            .iter()
            .filter(|f| f.mime.is_some())
            .map(|f| f.name.as_str())
            .collect();
        folders.sort();
        assert_eq!(folders, ["2026", "Wedding", "__ingot_backups"]);
        assert_eq!(mock.refreshes, 1);
        let stored = mock.files.iter().find(|f| f.name == "IMG_1.CR2").unwrap();
        assert_eq!(stored.bytes, BYTES);
        assert_eq!(stored.modified.as_deref(), Some("2023-11-14T22:13:20.000Z"));
    }

    #[tokio::test]
    async fn concurrent_uploads_create_each_folder_once() {
        let (state, config) = mock_drive().await;
        let drive = Arc::new(target(config));

        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..8 {
            let drive = Arc::clone(&drive);
            tasks.spawn(async move {
                let relative = format!("A/B/IMG_{i}.CR2");
                drive.store(&info(&relative, BYTES), Arc::from(BYTES)).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }

        let mock = state.0.lock().unwrap();
        assert_eq!(mock.files.iter().filter(|f| f.mime.is_some()).count(), 3);
        assert_eq!(mock.files.iter().filter(|f| f.mime.is_none()).count(), 8);
    }

    #[tokio::test]
    async fn identical_file_is_not_uploaded_again() {
        let (state, config) = mock_drive().await;
        let drive = target(config);
        let file = info("IMG_1.CR2", BYTES);

        drive.store(&file, Arc::from(BYTES)).await.unwrap();
        let again = drive.store(&file, Arc::from(BYTES)).await.unwrap();

        assert!(!again.newly_written());
        assert_eq!(photos(&state).len(), 1);
    }

    #[tokio::test]
    async fn different_file_under_the_same_name_is_kept_beside_it() {
        let (state, config) = mock_drive().await;
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();
        drive
            .store(&info("IMG_1.CR2", b"other"), Arc::from(&b"other"[..]))
            .await
            .unwrap();

        let files = photos(&state);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "IMG_1-2.CR2");
        assert_eq!(files[0].bytes, b"other");
        assert_eq!(files[1].bytes, BYTES);
    }

    #[tokio::test]
    async fn interrupted_upload_resumes_without_resending_what_arrived() {
        let (state, config) = mock_drive().await;
        state
            .0
            .lock()
            .unwrap()
            .put_faults
            .push_back(Fault::DropHalfWay);
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert_eq!(photos(&state)[0].bytes, BYTES);
        let mock = state.0.lock().unwrap();
        assert_eq!(mock.put_offsets, [0, (BYTES.len() / 2) as u64]);
    }

    #[tokio::test]
    async fn lost_response_does_not_create_a_second_file() {
        let (state, config) = mock_drive().await;
        state
            .0
            .lock()
            .unwrap()
            .put_faults
            .push_back(Fault::LoseResponse);
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert_eq!(photos(&state).len(), 1);
        assert_eq!(state.0.lock().unwrap().full_body_puts, 1);
    }

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_retried() {
        let (state, config) = mock_drive().await;
        {
            let mut mock = state.0.lock().unwrap();
            mock.api_faults
                .push_back(Fault::Status(403, "userRateLimitExceeded"));
            mock.api_faults
                .push_back(Fault::Status(500, "backendError"));
            mock.api_faults
                .push_back(Fault::RetryAfter(429, "rateLimitExceeded", 0));
            mock.session_faults
                .push_back(Fault::Status(503, "backendError"));
            mock.put_faults
                .push_back(Fault::Status(503, "backendError"));
        }
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert_eq!(photos(&state)[0].bytes, BYTES);
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_once_and_the_call_retried() {
        let (state, config) = mock_drive().await;
        let drive = target(config);
        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();
        state.0.lock().unwrap().expire_token_once = true;

        drive
            .store(&info("IMG_2.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert_eq!(state.0.lock().unwrap().refreshes, 2);
        assert_eq!(photos(&state).len(), 2);
    }

    #[tokio::test]
    async fn expired_session_restarts_the_upload() {
        let (state, config) = mock_drive().await;
        state
            .0
            .lock()
            .unwrap()
            .put_faults
            .push_back(Fault::Status(400, "badRequest"));
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await
            .unwrap();

        assert_eq!(photos(&state).len(), 1);
    }

    #[tokio::test]
    async fn full_drive_fails_fast_for_every_later_file() {
        let (state, config) = mock_drive().await;
        state
            .0
            .lock()
            .unwrap()
            .session_faults
            .push_back(Fault::Status(403, "storageQuotaExceeded"));
        let drive = target(config);

        let first = drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await;
        let second = drive
            .store(&info("IMG_2.CR2", BYTES), Arc::from(BYTES))
            .await;

        assert!(first.unwrap_err().to_string().contains("storageQuota"));
        assert!(second.unwrap_err().to_string().contains("storageQuota"));
        assert!(photos(&state).is_empty());
    }

    #[tokio::test]
    async fn bytes_that_no_longer_match_the_card_are_not_uploaded() {
        let (state, config) = mock_drive().await;
        let drive = target(config);

        let result = drive
            .store(
                &info("IMG_1.CR2", b"what the card held"),
                Arc::from(&b"corrupted in between"[..]),
            )
            .await;

        assert!(result.is_err());
        assert!(photos(&state).is_empty());
    }

    #[tokio::test]
    async fn checksum_mismatch_from_drive_gives_no_proof() {
        let (state, config) = mock_drive().await;
        state.0.lock().unwrap().put_faults.push_back(Fault::Corrupt);
        let drive = target(config);

        let result = drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await;

        assert!(result.unwrap_err().to_string().contains("different bytes"));
    }

    #[tokio::test]
    async fn names_with_quotes_are_escaped_in_queries() {
        let (state, config) = mock_drive().await;
        let drive = target(config);
        let file = info("Anna's/IMG_1.CR2", BYTES);

        drive.store(&file, Arc::from(BYTES)).await.unwrap();
        let again = drive.store(&file, Arc::from(BYTES)).await.unwrap();

        assert!(!again.newly_written());
        assert_eq!(photos(&state).len(), 1);
    }

    #[tokio::test]
    async fn empty_file_uploads() {
        let (state, config) = mock_drive().await;
        let drive = target(config);

        drive
            .store(&info("IMG_1.CR2", b""), Arc::from(&b""[..]))
            .await
            .unwrap();

        assert_eq!(photos(&state).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_import_reaches_local_and_drive_and_stays_contained() {
        const FILES: usize = 30;
        let (state, config) = mock_drive().await;
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        std::fs::create_dir_all(&card).unwrap();
        for i in 0..FILES {
            std::fs::write(card.join(format!("IMG_{i}.JPG")), vec![i as u8; 64 * 1024]).unwrap();
        }

        let mut engine = crate::Engine::new(crate::EngineConfig {
            read_channel_depth: 1,
            ..Default::default()
        });
        engine.scan(&card);
        let plan = crate::plan_import(
            &engine.scan_response,
            &card,
            &crate::ImportOptions {
                subfolder: Some("Wedding".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let primary = Arc::new(crate::LocalTarget::new(
            TargetId(1),
            crate::Target {
                root: tmp.path().join("photos"),
                kind: crate::TargetKind::LocalNvme,
                write_permits: 4,
            },
        ));
        let import = engine.start_import(plan, primary, vec![Arc::new(target(config))], None);
        let report = tokio::task::spawn_blocking(move || import.wait())
            .await
            .unwrap();

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.verified.len(), FILES * 2);
        assert_eq!(photos(&state).len(), FILES);
        assert_all_inside_the_root(&state);
    }

    /// Follows parents upward from every object on the mock Drive.
    fn assert_all_inside_the_root(state: &Shared) {
        let mock = state.0.lock().unwrap();
        let at_top: Vec<_> = mock.files.iter().filter(|f| f.parent == "root").collect();
        assert_eq!(at_top.len(), 1, "{at_top:?}");
        assert_eq!(at_top[0].name, DRIVE_ROOT_FOLDER);
        assert!(at_top[0].mime.is_some());
        for file in &mock.files {
            let mut current = file;
            while current.parent != "root" {
                current = mock
                    .files
                    .iter()
                    .find(|f| f.id == current.parent)
                    .unwrap_or_else(|| panic!("{} has an unknown parent", current.name));
            }
            assert_eq!(current.name, DRIVE_ROOT_FOLDER);
        }
    }

    #[tokio::test]
    async fn everything_stays_inside_the_ingot_folder() {
        let (state, config) = mock_drive().await;
        let drive = target(config);

        for relative in ["IMG_0.CR2", "Wedding/IMG_1.CR2", "a/b/c/IMG_2.CR2"] {
            drive
                .store(&info(relative, BYTES), Arc::from(BYTES))
                .await
                .unwrap();
        }

        assert_all_inside_the_root(&state);
    }

    #[tokio::test]
    async fn paths_that_climb_out_are_refused_before_any_write() {
        for relative in [
            "../IMG_1.CR2",
            "a/../../IMG_1.CR2",
            "/IMG_1.CR2",
            "/etc/IMG_1.CR2",
        ] {
            let (state, config) = mock_drive().await;
            let drive = target(config);

            let result = drive.store(&info(relative, BYTES), Arc::from(BYTES)).await;

            assert!(result.is_err(), "{relative}");
            assert!(photos(&state).is_empty(), "{relative}");
            assert_all_inside_or_empty(&state);
        }
    }

    fn assert_all_inside_or_empty(state: &Shared) {
        if !state.0.lock().unwrap().files.is_empty() {
            assert_all_inside_the_root(state);
        }
    }

    #[tokio::test]
    async fn upload_address_on_another_host_gets_no_token_or_bytes() {
        let (state, config) = mock_drive().await;
        state.0.lock().unwrap().foreign_session = true;
        let drive = target(config);

        let result = drive
            .store(&info("IMG_1.CR2", BYTES), Arc::from(BYTES))
            .await;

        assert!(result.unwrap_err().to_string().contains("unexpected host"));
    }

    #[test]
    fn verdicts_follow_rclone() {
        let verdict =
            |status, reason| classify(StatusCode::from_u16(status).unwrap(), reason, None);
        assert_eq!(verdict(503, ""), Verdict::Retry(None));
        assert_eq!(verdict(429, ""), Verdict::Retry(None));
        assert_eq!(verdict(403, "userRateLimitExceeded"), Verdict::Retry(None));
        assert_eq!(verdict(403, "storageQuotaExceeded"), Verdict::StopTarget);
        assert_eq!(verdict(401, ""), Verdict::Reauthorize);
        assert_eq!(verdict(403, "insufficientPermissions"), Verdict::GiveUp);
        assert_eq!(verdict(400, ""), Verdict::GiveUp);
    }

    #[test]
    fn backoff_doubles_and_stays_capped() {
        let first = Duration::from_secs(1);
        assert!(backoff(first, 1) < Duration::from_secs(2));
        assert!(backoff(first, 3) >= Duration::from_secs(4));
        assert!(backoff(first, 9) < Duration::from_secs(17));
    }

    #[tokio::test]
    async fn pacer_allows_the_burst_then_spaces_calls() {
        let pacer = Pacer::new(Duration::from_millis(40), 2);
        let started = Instant::now();
        pacer.wait().await;
        pacer.wait().await;
        assert!(started.elapsed() < Duration::from_millis(40));
        pacer.wait().await;
        pacer.wait().await;
        assert!(started.elapsed() >= Duration::from_millis(40));
    }
}
