use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use reqwest::header::HeaderMap;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::journal::{Journal, chunk_len, save_journal_bytes};
use crate::retry::{RetryPolicy, is_retryable, retry_async};
use crate::smart::TrafficBandit;
use crate::{Category, Error, ProbeInfo, StreamKind, probe_with_headers};

/// Minimum interval between progress events per download to avoid flooding UI/SSE consumers.
const PROGRESS_EMIT_INTERVAL_MS: u64 = 50;
/// Minimum interval between journal disk checkpoints during chunked downloads.
const JOURNAL_FLUSH_INTERVAL_MS: u64 = 400;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Max parallel connections for one file.
    pub connections: usize,
    /// Smallest chunk a connection will fetch at once.
    pub min_chunk_size: u64,
    /// Target chunks per connection — smaller chunks mean better work-stealing.
    pub chunks_per_connection: u64,
    /// Combined write-rate cap in bytes/sec. `None` = unlimited.
    pub rate_limit: Option<u64>,
    /// Continue from `.part` + `.odj` journal when present.
    pub resume: bool,
    /// Cancelling aborts the download; the journal keeps completed chunks
    /// so a later run resumes where this one stopped.
    pub cancel: Option<CancellationToken>,
    /// Sort the download into a type subfolder (Video/, Compressed/, …)
    /// under `spec.dest`'s directory. Leave off for explicit output paths.
    pub organize: bool,
    /// Retry policy for transient network / 5xx errors per chunk or segment.
    pub retry: RetryPolicy,
    /// Optional expected SHA-256 hex digest verified after the download finishes.
    pub expected_sha256: Option<String>,
    /// Fallback mirror URLs tried round-robin when the primary URL fails with a
    /// retryable error (e.g. 503, timeout, connection reset).
    pub mirrors: Vec<String>,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            min_chunk_size: 512 * 1024,
            chunks_per_connection: 4,
            rate_limit: None,
            resume: true,
            cancel: None,
            organize: false,
            retry: RetryPolicy::default(),
            expected_sha256: None,
            mirrors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadSpec {
    /// Caller-assigned id, echoed back in every `Event`.
    pub id: u64,
    pub url: String,
    pub dest: PathBuf,
    /// Extra request headers (cookies, referer) applied to probe and segments.
    pub headers: Vec<(String, String)>,
}

impl DownloadSpec {
    pub(crate) fn header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                k.parse::<reqwest::header::HeaderName>(),
                v.parse::<reqwest::header::HeaderValue>(),
            ) {
                map.append(name, value);
            }
        }
        map
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    /// True when completed chunks were recovered from a previous run.
    pub resumed: bool,
}

/// Terminal state reported by `Event::Finished`.
#[derive(Debug, Clone)]
pub enum FinishReason {
    Completed(DownloadOutcome),
    Failed(String),
    Cancelled,
}

/// Events emitted over the progress channel.
#[derive(Debug, Clone)]
pub enum Event {
    Progress {
        id: u64,
        downloaded: u64,
        total: Option<u64>,
    },
    /// Only sent by [`run`] and [`Scheduler::run`], which own the result so
    /// callers don't await twice.
    Finished { id: u64, reason: FinishReason },
}

/// Lock-free progress emitter that throttles updates to `PROGRESS_EMIT_INTERVAL_MS`
/// while always allowing a forced flush on completion.
pub(crate) struct ProgressEmitter {
    id: u64,
    total: Option<u64>,
    start: Instant,
    last_emit_ms: AtomicU64,
    tx: mpsc::UnboundedSender<Event>,
}

impl ProgressEmitter {
    pub(crate) fn new(id: u64, total: Option<u64>, tx: mpsc::UnboundedSender<Event>) -> Self {
        Self {
            id,
            total,
            start: Instant::now(),
            last_emit_ms: AtomicU64::new(0),
            tx,
        }
    }

    pub(crate) fn emit(&self, downloaded: u64) {
        let now_ms = self.start.elapsed().as_millis() as u64;
        let prev = self.last_emit_ms.load(Ordering::Relaxed);
        let is_complete = self.total.is_some_and(|t| downloaded >= t);
        let should_emit = is_complete
            || prev == 0
            || now_ms.saturating_sub(prev) >= PROGRESS_EMIT_INTERVAL_MS;
        if should_emit
            && (is_complete
                || self
                    .last_emit_ms
                    .compare_exchange(prev, now_ms.max(1), Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok())
        {
            let _ = self.tx.send(Event::Progress {
                id: self.id,
                downloaded,
                total: self.total,
            });
        }
    }

    pub(crate) fn flush(&self, downloaded: u64) {
        let _ = self.tx.send(Event::Progress {
            id: self.id,
            downloaded,
            total: self.total,
        });
    }
}

fn finish_reason(result: &Result<DownloadOutcome, Error>) -> FinishReason {
    match result {
        Ok(o) => FinishReason::Completed(o.clone()),
        Err(Error::Cancelled) => FinishReason::Cancelled,
        Err(e) => FinishReason::Failed(e.to_string()),
    }
}

/// Download and always finish by reporting through `events` — for UIs that
/// never await the future themselves.
pub async fn run(
    client: &reqwest::Client,
    spec: DownloadSpec,
    opts: DownloadOptions,
    events: mpsc::UnboundedSender<Event>,
) {
    let id = spec.id;
    let result = download(client, spec, &opts, events.clone()).await;
    let _ = events.send(Event::Finished {
        id,
        reason: finish_reason(&result),
    });
}

/// Select the active URL for retry `attempt`, rotating across `[primary, mirrors...]`.
fn url_for_attempt<'a>(primary: &'a str, mirrors: &'a [String], attempt: u32) -> &'a str {
    let total = 1 + mirrors.len();
    let idx = (attempt as usize) % total;
    if idx == 0 {
        primary
    } else {
        mirrors[idx - 1].as_str()
    }
}

/// Download `spec.url` to `spec.dest`. Writes to `dest.part`, journals progress
/// to `dest.odj`, renames on success. Resumes automatically when a journal
/// for the same URL/size/etag exists.
pub async fn download(
    client: &reqwest::Client,
    spec: DownloadSpec,
    opts: &DownloadOptions,
    events: mpsc::UnboundedSender<Event>,
) -> Result<DownloadOutcome, Error> {
    let headers = spec.header_map();
    let probe_cancel = CancellationToken::new();
    let info = retry_async(&opts.retry, &probe_cancel, |attempt| {
        let u = url_for_attempt(&spec.url, &opts.mirrors, attempt);
        probe_with_headers(client, u, &headers)
    })
    .await?;

    download_with_probe(client, spec, &headers, info, opts, events).await
}

/// Internal entry point shared with `HttpFetcher` when a resource was already
/// probed during the `Resolve` phase.
pub(crate) async fn download_with_probe(
    client: &reqwest::Client,
    spec: DownloadSpec,
    headers: &HeaderMap,
    info: ProbeInfo,
    opts: &DownloadOptions,
    events: mpsc::UnboundedSender<Event>,
) -> Result<DownloadOutcome, Error> {
    // Sort into a type subfolder — after probe so Content-Type and the
    // manifest kind are known. `Other` stays put.
    let mut dest = spec.dest.clone();
    if opts.organize
        && let Some(name) = dest.file_name().and_then(|n| n.to_str())
    {
        let cat = Category::detect(name, info.kind, info.content_type.as_deref());
        if cat != Category::Other
            && let Some(parent) = dest.parent()
            && parent.file_name().and_then(|n| n.to_str()) != Some(cat.dir_name())
        {
            let dir = parent.join(cat.dir_name());
            tokio::fs::create_dir_all(&dir).await?;
            dest = dir.join(name);
        }
    }

    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }

    let spec = DownloadSpec { dest, ..spec };

    // Streaming manifests expand into segments — a different code path
    // entirely (no ranges, file-based resume in `.segments/`).
    match info.kind {
        StreamKind::Hls => return crate::hls::download(client, &spec, headers, opts, &events).await,
        StreamKind::Dash => return crate::dash::download(client, &spec, headers, opts, &events).await,
        StreamKind::File => {}
    }

    let (part, journal_path) = sidecar_paths(&spec.dest);
    let ranged = info.accepts_ranges && info.size.unwrap_or(0) > 0;
    if ranged {
        let size = info.size.unwrap_or(0);
        let chunk_size = choose_chunk_size(size, opts);
        match download_chunked(
            client,
            &spec,
            headers,
            &part,
            &journal_path,
            &info,
            size,
            chunk_size,
            opts,
            &events,
        )
        .await
        {
            Ok(resumed) => {
                if let Some(expected) = &opts.expected_sha256 {
                    crate::verify::verify_file_sha256(&part, expected).await?;
                }
                return finish(part, &spec.dest, journal_path, resumed).await;
            }
            // Probe said ranges but the server refused mid-flight: restart clean.
            Err(Error::RangeRefused(_)) => {
                let _ = tokio::fs::remove_file(&journal_path).await;
            }
            Err(e) => return Err(e),
        }
    }
    download_single(client, spec.id, &spec.url, headers, &part, info.size, opts, &events).await?;
    if let Some(expected) = &opts.expected_sha256 {
        crate::verify::verify_file_sha256(&part, expected).await?;
    }
    finish(part, &spec.dest, journal_path, false).await
}

async fn finish(
    part: PathBuf,
    dest: &Path,
    journal: PathBuf,
    resumed: bool,
) -> Result<DownloadOutcome, Error> {
    // Windows rename fails when the target exists — remove it first.
    let _ = tokio::fs::remove_file(dest).await;
    tokio::fs::rename(&part, dest).await?;
    let _ = tokio::fs::remove_file(&journal).await;
    Ok(DownloadOutcome {
        path: dest.to_path_buf(),
        resumed,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_single(
    client: &reqwest::Client,
    id: u64,
    url: &str,
    headers: &HeaderMap,
    part: &Path,
    total: Option<u64>,
    opts: &DownloadOptions,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), Error> {
    let cancel = opts.cancel.clone().unwrap_or_default();
    let limiter = opts.rate_limit.map(RateLimiter::new);
    let emitter = ProgressEmitter::new(id, total, events.clone());

    retry_async(&opts.retry, &cancel, |attempt| {
        let active_url = url_for_attempt(url, &opts.mirrors, attempt);
        let cancel = &cancel;
        let emitter = &emitter;
        let limiter = &limiter;
        async move {
            let resp = client
                .get(active_url)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?;
            let mut file = tokio::fs::File::create(part).await?;
            let mut stream = resp.bytes_stream();
            let mut downloaded = 0u64;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    next = stream.next() => next,
                };
                let Some(chunk) = next else { break };
                let chunk = chunk?;
                file.write_all(&chunk).await?;
                downloaded += chunk.len() as u64;
                emitter.emit(downloaded);
                if let Some(l) = limiter {
                    l.acquire(chunk.len() as u64).await;
                }
            }
            file.flush().await?;
            emitter.flush(downloaded);
            Ok(())
        }
    })
    .await
}

struct JournalState {
    journal: Journal,
    last_flush: Instant,
    dirty: bool,
}

#[allow(clippy::too_many_arguments)]
async fn download_chunked(
    client: &reqwest::Client,
    spec: &DownloadSpec,
    headers: &HeaderMap,
    part: &Path,
    journal_path: &Path,
    info: &ProbeInfo,
    size: u64,
    chunk_size: u64,
    opts: &DownloadOptions,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<bool, Error> {
    let part_matches = tokio::fs::metadata(part)
        .await
        .map(|m| m.len() == size)
        .unwrap_or(false);

    let journal = if opts.resume && part_matches {
        Journal::load(journal_path)
            .await
            .filter(|j| j.matches(&spec.url, size, info.etag.as_deref(), chunk_size))
    } else {
        None
    };
    let resumed = journal.is_some();
    let journal = match journal {
        Some(j) => j,
        None => {
            // Preallocate on a blocking thread so large files never stall Tokio workers.
            let part_owned = part.to_path_buf();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let f = std::fs::File::create(&part_owned)?;
                f.set_len(size)?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Worker(e.to_string()))??;

            let j = Journal::new(&spec.url, size, info.etag.clone(), chunk_size);
            j.save(journal_path).await?;
            j
        }
    };

    let todo: Arc<[(u64, u64, u64)]> = chunk_ranges(size, chunk_size)
        .into_iter()
        .filter(|(i, _, _)| !journal.completed.contains(i))
        .collect::<Vec<_>>()
        .into();
    if todo.is_empty() {
        return Ok(resumed);
    }

    let next = Arc::new(AtomicUsize::new(0));
    let downloaded = Arc::new(AtomicU64::new(journal.completed_bytes()));
    let state = Arc::new(Mutex::new(JournalState {
        journal,
        last_flush: Instant::now(),
        dirty: false,
    }));
    let limiter = opts.rate_limit.map(|r| Arc::new(RateLimiter::new(r)));
    let emitter = Arc::new(ProgressEmitter::new(spec.id, Some(size), events.clone()));
    let bandit = Arc::new(Mutex::new(TrafficBandit::new(&spec.url, &opts.mirrors)));

    let workers = opts.connections.max(1).min(todo.len());
    let mut tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let client = client.clone();
        let part = part.to_path_buf();
        let journal_path = journal_path.to_path_buf();
        let todo = Arc::clone(&todo);
        let next = Arc::clone(&next);
        let counter = Arc::clone(&downloaded);
        let state = Arc::clone(&state);
        let emitter = Arc::clone(&emitter);
        let bandit = Arc::clone(&bandit);
        let limiter = limiter.clone();
        let headers = headers.clone();
        let cancel = opts.cancel.clone().unwrap_or_default();
        let retry = opts.retry.clone();

        tasks.push(tokio::spawn(async move {
            // Open one handle per worker instead of re-opening on every chunk.
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&part)
                .await?;

            while let Some(&(idx, start, _len)) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let expected_len = chunk_len(size, chunk_size, idx);
                let end = start + expected_len - 1;
                let range_val = format!("bytes={start}-{end}");

                let max_attempts = retry.max_attempts.max(1);
                let mut attempt = 0u32;
                loop {
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    // Adaptive UCB1/EWMA TrafficBandit selects the fastest healthy mirror.
                    let (arm_idx, active_url) = {
                        let mut b = bandit.lock().await;
                        b.select()
                    };
                    let chunk_start = Instant::now();
                    let attempt_res: Result<u64, Error> = async {
                        let resp = client
                            .get(&active_url)
                            .headers(headers.clone())
                            .header(reqwest::header::RANGE, range_val.as_str())
                            .send()
                            .await?
                            .error_for_status()?;
                        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                            return Err(Error::RangeRefused(resp.status().as_u16()));
                        }
                        file.seek(std::io::SeekFrom::Start(start)).await?;
                        let mut stream = resp.bytes_stream();
                        let mut chunk_written = 0u64;
                        let write_res: Result<(), Error> = async {
                            loop {
                                let next_item = tokio::select! {
                                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                                    next_item = stream.next() => next_item,
                                };
                                let Some(bytes) = next_item else { break };
                                let bytes = bytes?;
                                file.write_all(&bytes).await?;
                                let len = bytes.len() as u64;
                                chunk_written += len;
                                let n = counter.fetch_add(len, Ordering::Relaxed) + len;
                                emitter.emit(n);
                                if let Some(l) = &limiter {
                                    l.acquire(len).await;
                                }
                            }
                            file.flush().await?;
                            Ok(())
                        }
                        .await;

                        if let Err(e) = write_res {
                            // Roll back progress counter for partial failed attempt before retry.
                            counter.fetch_sub(chunk_written, Ordering::Relaxed);
                            return Err(e);
                        }
                        Ok(chunk_written)
                    }
                    .await;

                    match attempt_res {
                        Ok(bytes_written) => {
                            let mut b = bandit.lock().await;
                            b.record_success(arm_idx, bytes_written, chunk_start.elapsed());
                            break;
                        }
                        Err(err) => {
                            {
                                let mut b = bandit.lock().await;
                                b.record_failure(arm_idx);
                            }
                            attempt += 1;
                            if attempt >= max_attempts || !is_retryable(&err) {
                                return Err(err);
                            }
                            let delay = retry.delay_for(attempt - 1);
                            if !delay.is_zero() {
                                tokio::select! {
                                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                                    _ = tokio::time::sleep(delay) => {}
                                }
                            }
                        }
                    }
                }

                // Update journal and serialize under lock, then drop lock BEFORE disk I/O.
                let snapshot = {
                    let mut st = state.lock().await;
                    st.journal.completed.insert(idx);
                    st.dirty = true;
                    if st.last_flush.elapsed() >= Duration::from_millis(JOURNAL_FLUSH_INTERVAL_MS) {
                        st.last_flush = Instant::now();
                        st.dirty = false;
                        Some(st.journal.to_bytes()?)
                    } else {
                        None
                    }
                };
                if let Some(bytes) = snapshot {
                    save_journal_bytes(&journal_path, &bytes).await?;
                }
            }
            Ok::<(), Error>(())
        }));
    }

    let mut first_err: Option<Error> = None;
    for task in tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
            Ok(Err(_)) => {}
            Err(e) if first_err.is_none() => first_err = Some(Error::Worker(e.to_string())),
            Err(_) => {}
        }
    }

    // Always flush any remaining dirty journal entries (even on cancel/error) so resume is exact.
    let final_snapshot = {
        let mut st = state.lock().await;
        if st.dirty {
            st.dirty = false;
            Some(st.journal.to_bytes()?)
        } else {
            None
        }
    };
    if let Some(bytes) = final_snapshot {
        let _ = save_journal_bytes(journal_path, &bytes).await;
    }

    if let Some(err) = first_err {
        return Err(err);
    }

    emitter.flush(downloaded.load(Ordering::Relaxed));
    Ok(resumed)
}

// ---------------------------------------------------------------------------
// scheduler

/// Runs many downloads with a cap on how many files transfer at once.
pub struct Scheduler {
    pub opts: DownloadOptions,
    pub max_parallel: usize,
}

impl Scheduler {
    pub fn new(opts: DownloadOptions, max_parallel: usize) -> Self {
        Self { opts, max_parallel }
    }

    pub async fn run(
        &self,
        client: &reqwest::Client,
        specs: Vec<DownloadSpec>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Vec<Result<DownloadOutcome, Error>> {
        futures::stream::iter(specs)
            .map(|spec| {
                let events = events.clone();
                async move {
                    let id = spec.id;
                    let result = download(client, spec, &self.opts, events.clone()).await;
                    let _ = events.send(Event::Finished {
                        id,
                        reason: finish_reason(&result),
                    });
                    result
                }
            })
            .buffer_unordered(self.max_parallel.max(1))
            .collect()
            .await
    }
}

// ---------------------------------------------------------------------------
// rate limiting

/// Global leaky bucket: workers accumulate bytes and sleep until they've
/// been throttled back under the configured rate.
pub(crate) struct RateLimiter {
    rate: u64,
    state: Mutex<RateState>,
}

struct RateState {
    start: Instant,
    bytes: u64,
}

impl RateLimiter {
    pub(crate) fn new(rate: u64) -> Self {
        Self {
            rate: rate.max(1),
            state: Mutex::new(RateState {
                start: Instant::now(),
                bytes: 0,
            }),
        }
    }

    pub(crate) async fn acquire(&self, n: u64) {
        let wait = {
            let mut s = self.state.lock().await;
            s.bytes += n;
            let expected = s.bytes as f64 / self.rate as f64;
            let elapsed = s.start.elapsed().as_secs_f64();
            if expected > elapsed {
                Duration::from_secs_f64(expected - elapsed)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

// ---------------------------------------------------------------------------
// chunk plan

pub(crate) fn sidecar_paths(dest: &Path) -> (PathBuf, PathBuf) {
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let mut journal = dest.as_os_str().to_os_string();
    journal.push(".odj");
    (PathBuf::from(part), PathBuf::from(journal))
}

/// (index, start_byte, len) for every chunk covering `size` bytes.
fn chunk_ranges(size: u64, chunk_size: u64) -> Vec<(u64, u64, u64)> {
    if size == 0 {
        return Vec::new();
    }
    (0..size.div_ceil(chunk_size))
        .map(|i| (i, i * chunk_size, chunk_len(size, chunk_size, i)))
        .collect()
}

/// Aim for `connections * chunks_per_connection` chunks so fast connections
/// can steal work from slow ones, but never smaller than `min_chunk_size`.
fn choose_chunk_size(size: u64, opts: &DownloadOptions) -> u64 {
    let target = size / (opts.connections as u64 * opts.chunks_per_connection.max(1));
    target.max(opts.min_chunk_size).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_the_file() {
        let chunks = chunk_ranges(100, 30);
        assert_eq!(
            chunks,
            vec![(0, 0, 30), (1, 30, 30), (2, 60, 30), (3, 90, 10)]
        );
    }

    #[test]
    fn chunk_ranges_empty_for_zero_size() {
        assert!(chunk_ranges(0, 1024).is_empty());
    }

    #[test]
    fn chunk_size_scales_with_connection_count() {
        let opts = DownloadOptions::default();
        // 100 MB / (8 * 4) = ~3 MB per chunk
        let cs = choose_chunk_size(100 * 1024 * 1024, &opts);
        assert_eq!(cs, 100 * 1024 * 1024 / 32);
        // small files bottom out at min_chunk_size
        assert_eq!(choose_chunk_size(1024 * 1024, &opts), opts.min_chunk_size);
    }

    #[test]
    fn journal_roundtrips_and_validates() {
        let mut j = Journal::new("http://x/f", 100, Some("\"e1\"".into()), 30);
        j.completed.insert(0);
        j.completed.insert(2);
        let bytes = serde_json::to_vec(&j).unwrap();
        let j2: Journal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(j2.completed, [0, 2].into_iter().collect());
        assert!(j2.matches("http://x/f", 100, Some("\"e1\""), 30));
        assert!(!j2.matches("http://x/f", 100, Some("\"e2\""), 30));
        assert!(!j2.matches("http://x/f", 101, Some("\"e1\""), 30));
        assert!(!j2.matches("http://x/f", 100, Some("\"e1\""), 40));
        // missing etag can't invalidate
        assert!(j2.matches("http://x/f", 100, None, 30));
    }

    #[test]
    fn completed_bytes_counts_short_tail_chunk() {
        let mut j = Journal::new("u", 100, None, 30);
        j.completed.insert(3); // last chunk: 10 bytes
        assert_eq!(j.completed_bytes(), 10);
        j.completed.insert(0);
        assert_eq!(j.completed_bytes(), 40);
    }
}
