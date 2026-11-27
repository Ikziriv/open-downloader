//! Shared machinery for HLS/DASH downloads: a manifest expands into an
//! ordered list of segment URLs, which are fetched with the same
//! work-stealing worker pool as ranged chunks and concatenated in order.
//!
//! Resume is file-based rather than journal-based: each segment is written
//! to `dir/NNNNNN.tmp` and renamed to `dir/NNNNNN` only when fully written,
//! so a restart simply re-fetches indices that lack a final file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use futures::StreamExt;
use reqwest::header::HeaderMap;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::engine::{ProgressEmitter, RateLimiter};
use crate::retry::retry_async;
use crate::{DownloadOptions, Error, Event};

/// Optional per-segment decrypt hook (HLS AES-128). Given the segment index
/// and its fetched bytes, returns the plaintext. Unencrypted segments simply
/// return the input.
pub(crate) type Decrypt = Arc<dyn Fn(usize, &[u8]) -> Result<Vec<u8>, Error> + Send + Sync>;

fn segment_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i:06}"))
}

fn tmp_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i:06}.tmp"))
}

/// Directory holding segments for `dest`, e.g. `video.ts.segments`.
pub(crate) fn segments_dir(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_os_string();
    p.push(".segments");
    PathBuf::from(p)
}

/// Fetch every URL into `dir` as an ordered segment file. Existing final
/// files are reused, so restarting a stream download continues where it
/// stopped. Returns the ordered segment paths.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch_segments(
    client: &reqwest::Client,
    urls: &[String],
    headers: &HeaderMap,
    dir: &Path,
    opts: &DownloadOptions,
    events: &mpsc::UnboundedSender<Event>,
    id: u64,
    decrypt: Option<Decrypt>,
) -> Result<Vec<PathBuf>, Error> {
    tokio::fs::create_dir_all(dir).await?;
    let mut pending = Vec::with_capacity(urls.len());
    for i in 0..urls.len() {
        if !tokio::fs::try_exists(segment_path(dir, i))
            .await
            .unwrap_or(false)
        {
            pending.push(i);
        }
    }
    let pending: Arc<[usize]> = pending.into();
    let next = Arc::new(AtomicUsize::new(0));
    let downloaded = Arc::new(AtomicU64::new(0));
    let limiter = opts.rate_limit.map(|r| Arc::new(RateLimiter::new(r)));
    let cancel = opts.cancel.clone().unwrap_or_default();
    let emitter = Arc::new(ProgressEmitter::new(id, None, events.clone()));

    let urls: Arc<[String]> = urls.to_vec().into();
    let workers = opts.connections.max(1).min(pending.len().max(1));
    let mut tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let client = client.clone();
        let headers = headers.clone();
        let dir = dir.to_path_buf();
        let pending = Arc::clone(&pending);
        let urls = Arc::clone(&urls);
        let next = Arc::clone(&next);
        let counter = Arc::clone(&downloaded);
        let emitter = Arc::clone(&emitter);
        let limiter = limiter.clone();
        let cancel = cancel.clone();
        let decrypt = decrypt.clone();
        let retry = opts.retry.clone();

        tasks.push(tokio::spawn(async move {
            while let Some(&i) = pending.get(next.fetch_add(1, Ordering::Relaxed)) {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let tmp = tmp_path(&dir, i);
                let final_path = segment_path(&dir, i);
                let url = &urls[i];

                retry_async(&retry, &cancel, |_| {
                    let client = &client;
                    let headers = &headers;
                    let tmp = &tmp;
                    let final_path = &final_path;
                    let counter = &counter;
                    let emitter = &emitter;
                    let limiter = &limiter;
                    let cancel = &cancel;
                    let decrypt = &decrypt;
                    async move {
                        let resp = client
                            .get(url)
                            .headers(headers.clone())
                            .send()
                            .await?
                            .error_for_status()?;
                        let mut file = tokio::fs::File::create(tmp).await?;
                        if let Some(d) = decrypt {
                            // Encrypted segments must be whole before decrypting —
                            // buffer, decrypt, write once.
                            let data = resp.bytes().await?;
                            let data = d(i, &data)?;
                            file.write_all(&data).await?;
                            let len = data.len() as u64;
                            let n = counter.fetch_add(len, Ordering::Relaxed) + len;
                            emitter.emit(n);
                            if let Some(l) = limiter {
                                l.acquire(len).await;
                            }
                            file.flush().await?;
                            tokio::fs::rename(tmp, final_path).await?;
                            return Ok(());
                        }
                        let mut stream = resp.bytes_stream();
                        let mut seg_written = 0u64;
                        let res: Result<(), Error> = async {
                            loop {
                                let next_item = tokio::select! {
                                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                                    next_item = stream.next() => next_item,
                                };
                                let Some(bytes) = next_item else { break };
                                let bytes = bytes?;
                                file.write_all(&bytes).await?;
                                let len = bytes.len() as u64;
                                seg_written += len;
                                let n = counter.fetch_add(len, Ordering::Relaxed) + len;
                                emitter.emit(n);
                                if let Some(l) = limiter {
                                    l.acquire(len).await;
                                }
                            }
                            file.flush().await?;
                            tokio::fs::rename(tmp, final_path).await?;
                            Ok(())
                        }
                        .await;

                        if let Err(e) = res {
                            counter.fetch_sub(seg_written, Ordering::Relaxed);
                            return Err(e);
                        }
                        Ok(())
                    }
                })
                .await?;
            }
            Ok::<(), Error>(())
        }));
    }
    for task in tasks {
        task.await.map_err(|e| Error::Worker(e.to_string()))??;
    }
    emitter.flush(downloaded.load(Ordering::Relaxed));
    Ok((0..urls.len()).map(|i| segment_path(dir, i)).collect())
}

/// Append every segment file into `dest` in manifest order.
pub(crate) async fn concat_segments(paths: &[PathBuf], dest: &Path) -> Result<u64, Error> {
    let mut out = tokio::fs::File::create(dest).await?;
    let mut total = 0;
    for p in paths {
        let mut f = tokio::fs::File::open(p).await?;
        total += tokio::io::copy(&mut f, &mut out).await?;
    }
    out.flush().await?;
    Ok(total)
}
