//! Built-in Speedtest.net-style Internet connection & traffic diagnostics.
//!
//! Measures reachability, median latency (ping), jitter, multi-stream download
//! throughput, and optional upload throughput against Cloudflare's global edge
//! (`speed.cloudflare.com`) or any custom mirror configured via [`SpeedTestConfig`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::Error;

pub const DEFAULT_DOWNLOAD_URL: &str = "https://speed.cloudflare.com/__down";
pub const DEFAULT_UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";

/// Configuration for a speed test run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpeedTestConfig {
    /// Download endpoint accepting `?bytes=N` (default: Cloudflare edge).
    #[serde(default = "default_download_url")]
    pub download_url: String,
    /// Upload endpoint accepting `POST` body bytes (default: Cloudflare edge).
    #[serde(default = "default_upload_url")]
    pub upload_url: String,
    /// Number of RTT probes (the first is discarded as TCP/TLS warm-up).
    #[serde(default = "default_ping_samples")]
    pub ping_samples: usize,
    /// Parallel download streams.
    #[serde(default = "default_download_streams")]
    pub download_streams: usize,
    /// Target total download payload in bytes (bounded by `phase_budget`).
    #[serde(default = "default_download_bytes")]
    pub download_bytes: u64,
    /// Target upload payload in bytes.
    #[serde(default = "default_upload_bytes")]
    pub upload_bytes: u64,
    /// Whether to run the upload phase after download.
    #[serde(default = "default_true")]
    pub include_upload: bool,
    /// Max wall-clock duration for each transfer phase so slow links never stall.
    #[serde(default = "default_phase_budget_ms")]
    pub phase_budget_ms: u64,
}

fn default_download_url() -> String {
    DEFAULT_DOWNLOAD_URL.into()
}
fn default_upload_url() -> String {
    DEFAULT_UPLOAD_URL.into()
}
fn default_ping_samples() -> usize {
    5
}
fn default_download_streams() -> usize {
    4
}
fn default_download_bytes() -> u64 {
    12 * 1024 * 1024
}
fn default_upload_bytes() -> u64 {
    2 * 1024 * 1024
}
fn default_true() -> bool {
    true
}
fn default_phase_budget_ms() -> u64 {
    4500
}

impl Default for SpeedTestConfig {
    fn default() -> Self {
        Self {
            download_url: default_download_url(),
            upload_url: default_upload_url(),
            ping_samples: default_ping_samples(),
            download_streams: default_download_streams(),
            download_bytes: default_download_bytes(),
            upload_bytes: default_upload_bytes(),
            include_upload: true,
            phase_budget_ms: default_phase_budget_ms(),
        }
    }
}

/// Active phase of a running speed test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedTestPhase {
    Connecting,
    Ping,
    Download,
    Upload,
    Done,
}

/// Overall connection quality grade derived from latency and throughput.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionQuality {
    Excellent,
    Good,
    Fair,
    Poor,
    Offline,
}

impl ConnectionQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Excellent => "Excellent",
            Self::Good => "Good",
            Self::Fair => "Fair",
            Self::Poor => "Poor",
            Self::Offline => "Offline",
        }
    }
}

/// Live events emitted during [`run_speedtest`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpeedTestEvent {
    PhaseChanged {
        phase: SpeedTestPhase,
    },
    Sample {
        phase: SpeedTestPhase,
        mbps: f64,
        bytes: u64,
        elapsed_ms: u64,
    },
    Completed {
        report: Box<SpeedTestReport>,
    },
}

/// Final diagnostic summary produced by [`run_speedtest`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpeedTestReport {
    pub online: bool,
    /// Median RTT in milliseconds.
    pub ping_ms: f64,
    /// Mean absolute difference between consecutive RTT probes in milliseconds.
    pub jitter_ms: f64,
    /// Sustained download speed in megabits per second (Mbps).
    pub download_mbps: f64,
    /// Sustained download speed in bytes per second.
    pub download_bps: u64,
    /// Sustained upload speed in megabits per second (Mbps), if tested.
    pub upload_mbps: Option<f64>,
    /// Sustained upload speed in bytes per second, if tested.
    pub upload_bps: Option<u64>,
    pub quality: ConnectionQuality,
    /// Recommended `DownloadOptions::connections` for this link.
    pub recommended_connections: usize,
    /// Time-series throughput curve (Mbps) sampled during download.
    pub samples_mbps: Vec<f64>,
}

/// Classify connection quality and recommend optimal parallel connections.
pub fn evaluate_connection(ping_ms: f64, download_mbps: f64) -> (ConnectionQuality, usize) {
    let quality = if download_mbps >= 100.0 && ping_ms <= 50.0 {
        ConnectionQuality::Excellent
    } else if download_mbps >= 25.0 && ping_ms <= 100.0 {
        ConnectionQuality::Good
    } else if download_mbps >= 5.0 {
        ConnectionQuality::Fair
    } else {
        ConnectionQuality::Poor
    };

    let recommended = if download_mbps >= 150.0 {
        12
    } else if download_mbps >= 25.0 {
        8
    } else {
        4
    };
    (quality, recommended)
}

/// Run a full cancellation-aware speed test, optionally streaming live
/// [`SpeedTestEvent`]s to `events`.
pub async fn run_speedtest(
    client: &reqwest::Client,
    cfg: &SpeedTestConfig,
    cancel: CancellationToken,
    events: Option<mpsc::UnboundedSender<SpeedTestEvent>>,
) -> Result<SpeedTestReport, Error> {
    let emit = |ev: SpeedTestEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(ev);
        }
    };

    emit(SpeedTestEvent::PhaseChanged {
        phase: SpeedTestPhase::Connecting,
    });
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    // 1. Ping & Jitter phase
    emit(SpeedTestEvent::PhaseChanged {
        phase: SpeedTestPhase::Ping,
    });
    let (ping_ms, jitter_ms) = measure_ping(client, cfg, &cancel).await?;

    // 2. Multi-stream Download phase (time-bounded)
    emit(SpeedTestEvent::PhaseChanged {
        phase: SpeedTestPhase::Download,
    });
    let (download_bps, samples_mbps) = measure_download(client, cfg, &cancel, &events).await?;
    let download_mbps = bytes_per_sec_to_mbps(download_bps);

    // 3. Optional Upload phase
    let (upload_bps, upload_mbps) = if cfg.include_upload && cfg.upload_bytes > 0 {
        emit(SpeedTestEvent::PhaseChanged {
            phase: SpeedTestPhase::Upload,
        });
        let bps = measure_upload(client, cfg, &cancel, &events).await?;
        (Some(bps), Some(bytes_per_sec_to_mbps(bps)))
    } else {
        (None, None)
    };

    let (quality, recommended_connections) = evaluate_connection(ping_ms, download_mbps);
    let report = SpeedTestReport {
        online: true,
        ping_ms: round2(ping_ms),
        jitter_ms: round2(jitter_ms),
        download_mbps: round2(download_mbps),
        download_bps,
        upload_mbps: upload_mbps.map(round2),
        upload_bps,
        quality,
        recommended_connections,
        samples_mbps,
    };

    emit(SpeedTestEvent::PhaseChanged {
        phase: SpeedTestPhase::Done,
    });
    emit(SpeedTestEvent::Completed {
        report: Box::new(report.clone()),
    });
    Ok(report)
}

async fn measure_ping(
    client: &reqwest::Client,
    cfg: &SpeedTestConfig,
    cancel: &CancellationToken,
) -> Result<(f64, f64), Error> {
    let probes = cfg.ping_samples.max(2);
    let url = with_bytes_query(&cfg.download_url, 0);
    let mut rtts = Vec::with_capacity(probes);

    for i in 0..probes {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let start = Instant::now();
        let req = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            res = req => res?,
        };
        let _ = resp.error_for_status()?.bytes().await?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        // Drop sample 0 (TCP + TLS cold handshake) when we have multiple probes.
        if i > 0 || probes == 1 {
            rtts.push(elapsed_ms);
        }
    }

    let jitter_ms = if rtts.len() >= 2 {
        let sum: f64 = rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        sum / (rtts.len() - 1) as f64
    } else {
        0.0
    };

    rtts.sort_by(f64::total_cmp);
    let median = rtts[rtts.len() / 2];
    Ok((median, jitter_ms))
}

async fn measure_download(
    client: &reqwest::Client,
    cfg: &SpeedTestConfig,
    cancel: &CancellationToken,
    events: &Option<mpsc::UnboundedSender<SpeedTestEvent>>,
) -> Result<(u64, Vec<f64>), Error> {
    let streams = cfg.download_streams.max(1);
    let per_stream = (cfg.download_bytes / streams as u64).max(64 * 1024);
    let url = with_bytes_query(&cfg.download_url, per_stream);
    let budget = Duration::from_millis(cfg.phase_budget_ms.max(500));
    let phase_cancel = cancel.child_token();

    let total_bytes = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    let mut handles = Vec::with_capacity(streams);
    for _ in 0..streams {
        let client = client.clone();
        let url = url.clone();
        let counter = Arc::clone(&total_bytes);
        let tok = phase_cancel.clone();
        handles.push(tokio::spawn(async move {
            let resp = tokio::select! {
                _ = tok.cancelled() => return Ok::<(), Error>(()),
                res = client.get(&url).send() => res?.error_for_status()?,
            };
            let mut stream = resp.bytes_stream();
            loop {
                let next = tokio::select! {
                    _ = tok.cancelled() => break,
                    item = stream.next() => item,
                };
                let Some(chunk) = next else { break };
                let chunk = chunk?;
                counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Ok(())
        }));
    }

    // Sample throughput every 150ms until all workers finish or phase budget expires.
    let mut samples = Vec::new();
    let mut last_bytes = 0u64;
    let mut last_t = start;
    loop {
        if cancel.is_cancelled() {
            phase_cancel.cancel();
            return Err(Error::Cancelled);
        }
        let all_done = handles.iter().all(|h| h.is_finished());
        let elapsed = start.elapsed();
        if all_done || elapsed >= budget {
            phase_cancel.cancel();
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                phase_cancel.cancel();
                return Err(Error::Cancelled);
            }
            _ = tokio::time::sleep(Duration::from_millis(150)) => {}
        }
        let now = Instant::now();
        let bytes = total_bytes.load(Ordering::Relaxed);
        let dt = now.duration_since(last_t).as_secs_f64();
        if dt > 0.0 {
            let delta = bytes.saturating_sub(last_bytes);
            let mbps = bytes_per_sec_to_mbps((delta as f64 / dt) as u64);
            samples.push(round2(mbps));
            if let Some(tx) = events {
                let _ = tx.send(SpeedTestEvent::Sample {
                    phase: SpeedTestPhase::Download,
                    mbps: round2(mbps),
                    bytes,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
        }
        last_bytes = bytes;
        last_t = now;
    }

    for h in handles {
        let _ = h.await;
    }

    let bytes = total_bytes.load(Ordering::Relaxed);
    let elapsed_secs = start.elapsed().as_secs_f64().max(0.001);
    let bps = (bytes as f64 / elapsed_secs) as u64;
    if samples.is_empty() {
        samples.push(round2(bytes_per_sec_to_mbps(bps)));
    }
    Ok((bps, samples))
}

async fn measure_upload(
    client: &reqwest::Client,
    cfg: &SpeedTestConfig,
    cancel: &CancellationToken,
    events: &Option<mpsc::UnboundedSender<SpeedTestEvent>>,
) -> Result<u64, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let size = cfg.upload_bytes.min(8 * 1024 * 1024) as usize;
    let payload = vec![0x5Au8; size];
    let budget = Duration::from_millis(cfg.phase_budget_ms.max(500));
    let start = Instant::now();

    let send = client
        .post(&cfg.upload_url)
        .timeout(budget)
        .body(payload)
        .send();

    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        res = send => res?,
    };
    resp.error_for_status()?;

    let elapsed_secs = start.elapsed().as_secs_f64().max(0.001);
    let bps = (size as f64 / elapsed_secs) as u64;
    if let Some(tx) = events {
        let _ = tx.send(SpeedTestEvent::Sample {
            phase: SpeedTestPhase::Upload,
            mbps: round2(bytes_per_sec_to_mbps(bps)),
            bytes: size as u64,
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
    }
    Ok(bps)
}

fn with_bytes_query(base: &str, bytes: u64) -> String {
    if base.contains('?') {
        format!("{base}&bytes={bytes}")
    } else {
        format!("{base}?bytes={bytes}")
    }
}

fn bytes_per_sec_to_mbps(bps: u64) -> f64 {
    (bps as f64 * 8.0) / 1_000_000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_quality_and_recommended_connections() {
        assert_eq!(
            evaluate_connection(12.0, 250.0),
            (ConnectionQuality::Excellent, 12)
        );
        assert_eq!(
            evaluate_connection(35.0, 60.0),
            (ConnectionQuality::Good, 8)
        );
        assert_eq!(
            evaluate_connection(140.0, 12.0),
            (ConnectionQuality::Fair, 4)
        );
        assert_eq!(
            evaluate_connection(200.0, 2.0),
            (ConnectionQuality::Poor, 4)
        );
    }
}
