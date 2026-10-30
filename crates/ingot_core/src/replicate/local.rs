//! Replication onto a locally mounted filesystem.
//!
//! A destination file is only ever created from a temp file that has already been
//! verified, and this module never removes or replaces a destination file (`sweep.rs`
//! holds the one narrow, proof-gated exception, and only moves to the Trash). A file
//! already there with the same contents counts as copied; one with different
//! contents is left alone and the new file is stored under a numbered name.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::replicate::ReplicationTarget;
use crate::{
    Target,
    replicate::{
        DestinationClaims, FileInfo, MAX_NAME_ATTEMPTS, ReplicaLocation, ReplicationError,
        TargetId, VerifiedReplica, candidate_name,
    },
};

pub struct LocalTarget {
    id: TargetId,
    config: Target,
    write: tokio::sync::Semaphore,
    write_permits: usize,
    claims: DestinationClaims,
}

impl LocalTarget {
    pub fn new(id: TargetId, config: Target) -> Self {
        // Zero permits would make every `store` wait forever.
        let write_permits = config.write_permits.max(1);
        LocalTarget {
            id,
            config,
            write: tokio::sync::Semaphore::new(write_permits),
            write_permits,
            claims: DestinationClaims::default(),
        }
    }
}

enum Existing {
    Absent,
    Same,
    Different,
}

/// Hashes on one thread. Files are read at disk speed, and a parallel hash would
/// take CPU from preview generation for no gain in overall copy time.
fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    Ok(blake3::Hasher::new().update_mmap(path)?.finalize())
}

fn inspect_existing(path: &Path, size: u64, expected: blake3::Hash) -> io::Result<Existing> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() || metadata.len() != size {
        return Ok(Existing::Different);
    }
    Ok(if hash_file(path)? == expected {
        Existing::Same
    } else {
        Existing::Different
    })
}

fn write_temp(tmp: &Path, bytes: &[u8], modified: Option<SystemTime>) -> io::Result<blake3::Hash> {
    let mut file = fs::File::create(tmp)?;
    file.write_all(bytes)?;
    if let Some(modified) = modified {
        // Best effort: not every filesystem stores it, and the photo is intact either way.
        let _ = file.set_modified(modified);
    }
    file.sync_all()?;
    drop(file);
    hash_file(tmp)
}

/// Moves `tmp` to `dst` unless `dst` exists, and says whether it did. A hard link
/// fails atomically on an existing name, even against other processes, while a
/// rename would silently replace it. Filesystems without hard links, such as
/// exFAT, fall back to a check followed by a rename.
fn publish_no_replace(tmp: &Path, dst: &Path) -> io::Result<bool> {
    match fs::hard_link(tmp, dst) {
        Ok(()) => {
            fs::remove_file(tmp)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => {
            if dst.try_exists()? {
                return Ok(false);
            }
            fs::rename(tmp, dst)?;
            Ok(true)
        }
    }
}

enum Stored {
    AlreadyPresent,
    Written,
    NameTaken,
}

fn store_blocking(
    dst: &Path,
    bytes: &[u8],
    expected: blake3::Hash,
    modified: Option<SystemTime>,
) -> Result<Stored, ReplicationError> {
    match inspect_existing(dst, bytes.len() as u64, expected)? {
        Existing::Same => return Ok(Stored::AlreadyPresent),
        Existing::Different => return Ok(Stored::NameTaken),
        Existing::Absent => {}
    }

    let mut tmp = dst.to_path_buf();
    tmp.as_mut_os_string().push(".tmp");

    let published = write_temp(&tmp, bytes, modified)
        .map_err(ReplicationError::from)
        .and_then(|observed| {
            if observed != expected {
                return Err(ReplicationError::from(format!(
                    "verification failed for {}: expected {expected}, observed {observed}",
                    dst.display()
                )));
            }
            Ok(publish_no_replace(&tmp, dst)?)
        });

    match published {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_file(&tmp);
            return Ok(Stored::NameTaken);
        }
        Err(err) => {
            // Best effort: the original failure is the one worth reporting, and a
            // leftover temp file is truncated by the next attempt.
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    }

    // The new name is only durable once the directory entry is flushed.
    if let Some(dir) = dst.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(Stored::Written)
}

#[async_trait::async_trait]
impl ReplicationTarget for LocalTarget {
    fn id(&self) -> TargetId {
        self.id
    }

    fn max_concurrent_writes(&self) -> usize {
        self.write_permits
    }

    async fn store(
        &self,
        file: &FileInfo,
        bytes: Arc<[u8]>,
    ) -> Result<VerifiedReplica, ReplicationError> {
        let _permit = self.write.acquire().await?;

        let routed = self.config.root.join(&file.relative);
        let (Some(dir), Some(file_name)) = (routed.parent(), routed.file_name()) else {
            return Err(ReplicationError::from(format!(
                "not a file path: {}",
                routed.display()
            )));
        };
        let dir = dir.to_path_buf();
        tokio::fs::create_dir_all(&dir).await?;

        for attempt in 1..=MAX_NAME_ATTEMPTS {
            let dst: PathBuf = dir.join(candidate_name(file_name, attempt));
            // Held across the check and the write, so two source files routed to
            // one name take turns and share neither a name nor a temp file.
            let _claim = self.claims.claim(dst.clone()).await;

            let stored = {
                let (dst, bytes) = (dst.clone(), Arc::clone(&bytes));
                let (expected, modified) = (file.src_hash, file.modified);
                tokio::task::spawn_blocking(move || {
                    store_blocking(&dst, &bytes, expected, modified)
                })
                .await??
            };

            let newly_written = match stored {
                Stored::NameTaken => continue,
                Stored::AlreadyPresent => false,
                Stored::Written => true,
            };
            return VerifiedReplica::new(
                file.source.clone(),
                self.id,
                ReplicaLocation::Local(dst),
                bytes.len() as u64,
                newly_written,
                file.src_hash,
                file.src_hash,
            );
        }

        Err(ReplicationError::from(format!(
            "no free name for {} after {MAX_NAME_ATTEMPTS} attempts",
            routed.display()
        )))
    }
}
