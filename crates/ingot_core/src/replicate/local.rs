//! Replication onto a locally mounted filesystem.
//!
//! A destination file is only ever created by renaming a temp file that has
//! already been verified, and this module never removes a destination file: an
//! existing file is reported as a failure and left untouched.

use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use chrono::NaiveDate;
use tokio::fs::{File, create_dir_all, remove_file, rename, try_exists};
use tokio::io::AsyncWriteExt;

use crate::replicate::ReplicationTarget;
use crate::{
    Target, build_destination_path,
    replicate::{ReplicationError, TargetId, VerifiedReplica},
};

pub struct LocalTarget {
    id: TargetId,
    config: Target,
    write: tokio::sync::Semaphore,
    write_permits: usize,
    in_flight: Mutex<HashSet<PathBuf>>,
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
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Claims `path` for one `store` call. Two source files can route to the same
    /// destination (same name and date in different folders); without the claim
    /// both would pass the existence check and share one temp file.
    fn claim(&self, path: &Path) -> Result<DestinationClaim<'_>, ReplicationError> {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !in_flight.insert(path.to_path_buf()) {
            return Err(ReplicationError::from(format!(
                "destination already being written: {}",
                path.display()
            )));
        }
        Ok(DestinationClaim {
            in_flight: &self.in_flight,
            path: path.to_path_buf(),
        })
    }
}

struct DestinationClaim<'a> {
    in_flight: &'a Mutex<HashSet<PathBuf>>,
    path: PathBuf,
}

impl Drop for DestinationClaim<'_> {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.path);
    }
}

async fn hash_file(path: PathBuf) -> Result<blake3::Hash, ReplicationError> {
    let hash = tokio::task::spawn_blocking(move || -> Result<blake3::Hash, std::io::Error> {
        Ok(blake3::Hasher::new().update_mmap_rayon(&path)?.finalize())
    })
    .await??;
    Ok(hash)
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
        file_name: &OsStr,
        source: &Path,
        captured_at: NaiveDate,
        bytes: Arc<[u8]>,
        src_hash: blake3::Hash,
    ) -> Result<VerifiedReplica, ReplicationError> {
        let _permit = self.write.acquire().await?;

        // TODO: probably build_destination_path should return error instead of None?
        let dst_path = build_destination_path(&self.config, captured_at, Path::new(file_name))
            .ok_or_else(|| {
                ReplicationError::from(format!(
                    "failed to route destination for {}",
                    file_name.to_string_lossy()
                ))
            })?;
        let dst_dir = dst_path.parent().ok_or_else(|| {
            ReplicationError::from(format!(
                "destination has no parent directory: {}",
                dst_path.display()
            ))
        })?;

        let _claim = self.claim(&dst_path)?;
        if try_exists(&dst_path).await? {
            return Err(ReplicationError::from(format!(
                "destination already exists: {}",
                dst_path.display()
            )));
        }

        create_dir_all(dst_dir).await?;

        let mut tmp_dst_path = dst_path.clone();
        tmp_dst_path.as_mut_os_string().push(".tmp");

        let published: Result<VerifiedReplica, ReplicationError> = async {
            let mut file = File::create(&tmp_dst_path).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            drop(file);

            let dst_hash = hash_file(tmp_dst_path.clone()).await?;
            let replica = VerifiedReplica::new(
                source.to_path_buf(),
                self.id,
                dst_path.clone(),
                src_hash,
                dst_hash,
            )?;

            rename(&tmp_dst_path, &dst_path).await?;
            Ok(replica)
        }
        .await;

        let replica = match published {
            Ok(replica) => replica,
            Err(err) => {
                // Best effort: the original failure is the one worth reporting, and
                // a leftover temp file is truncated by the next attempt.
                let _ = remove_file(&tmp_dst_path).await;
                return Err(err);
            }
        };

        // The rename is only durable once the directory entry is flushed. A
        // replica is what later allows other copies to be deleted.
        File::open(dst_dir).await?.sync_all().await?;

        Ok(replica)
    }
}
