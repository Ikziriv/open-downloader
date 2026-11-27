use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::Error;

/// Resume state for one download, stored as JSON next to the `.part` file.
/// If this file is missing or corrupt we simply start over — never a hard error.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Journal {
    pub url: String,
    pub size: u64,
    pub etag: Option<String>,
    pub chunk_size: u64,
    /// Indices of chunks fully written to the `.part` file.
    pub completed: BTreeSet<u64>,
}

impl Journal {
    pub fn new(url: &str, size: u64, etag: Option<String>, chunk_size: u64) -> Self {
        Self {
            url: url.to_string(),
            size,
            etag,
            chunk_size,
            completed: BTreeSet::new(),
        }
    }

    /// Same remote file, same chunking plan → safe to resume. Etags are only
    /// compared when both sides provide one; a missing etag can't invalidate.
    pub fn matches(&self, url: &str, size: u64, etag: Option<&str>, chunk_size: u64) -> bool {
        self.url == url
            && self.size == size
            && self.chunk_size == chunk_size
            && match (&self.etag, etag) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }

    /// Bytes already on disk from previously completed chunks.
    pub fn completed_bytes(&self) -> u64 {
        self.completed
            .iter()
            .map(|&i| chunk_len(self.size, self.chunk_size, i))
            .sum()
    }

    pub async fn load(path: &Path) -> Option<Journal> {
        let data = tokio::fs::read(path).await.ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Serialize to bytes so callers can drop any lock before performing disk I/O.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Write-tmp-then-rename so a crash mid-save leaves the old journal intact.
    pub async fn save(&self, path: &Path) -> Result<(), Error> {
        let bytes = self.to_bytes()?;
        save_journal_bytes(path, &bytes).await
    }
}

/// Atomically persist pre-serialized journal bytes without holding any in-memory lock.
pub(crate) async fn save_journal_bytes(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, bytes).await?;
    // On Windows rename fails if the target exists and is locked briefly; remove best-effort.
    if tokio::fs::rename(&tmp, path).await.is_err() {
        let _ = tokio::fs::remove_file(path).await;
        tokio::fs::rename(&tmp, path).await?;
    }
    Ok(())
}

/// Byte length of chunk `idx` — every chunk except the last is `chunk_size`.
pub(crate) fn chunk_len(size: u64, chunk_size: u64, idx: u64) -> u64 {
    chunk_size.min(size - idx * chunk_size)
}
