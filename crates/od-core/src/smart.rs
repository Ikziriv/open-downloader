//! Smart download decision-making algorithms for Open Downloader:
//!
//! 1. **[`TrafficBandit`]** — EWMA + UCB1 multi-armed bandit for selecting the
//!    fastest mirror/traffic route per chunk while automatically deprioritizing
//!    slow or failing mirrors.
//! 2. **[`SmartQueue`]** — `BinaryHeap`-backed task scheduler combining user
//!    priority, Shortest Remaining Processing Time (SRPT), traffic-health
//!    scoring (deprioritizing low-traffic/throttled origins), and anti-starvation
//!    aging.
//! 3. **[`SmartPlan`]** — Initial decision maker that chooses connection count,
//!    chunk size, and traffic tier from file size + link speed before starting
//!    the first transfer.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

use crate::TaskPriority;

/// Traffic classification used by the decision engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficTier {
    /// High-throughput route — prioritized for full concurrency.
    High,
    /// Normal route — standard concurrency.
    Normal,
    /// Throttled / congested route (< 256 KB/s) — deprioritized so fast transfers finish first.
    Low,
}

/// Initial execution decision computed before starting a download.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmartPlan {
    pub connections: usize,
    pub chunk_size: u64,
    pub traffic_tier: TrafficTier,
    pub reason: String,
}

/// Decide the optimal initial download plan from file size, range support, and
/// observed/estimated link throughput (`estimated_bps`).
pub fn decide_initial_plan(
    size: Option<u64>,
    accepts_ranges: bool,
    estimated_bps: Option<u64>,
    max_connections: usize,
) -> SmartPlan {
    let max_c = max_connections.clamp(1, 32);
    if !accepts_ranges {
        return SmartPlan {
            connections: 1,
            chunk_size: 512 * 1024,
            traffic_tier: TrafficTier::Normal,
            reason: "server does not support HTTP Range; using single stream".into(),
        };
    }

    let size = size.unwrap_or(0);
    if size < 1024 * 1024 {
        return SmartPlan {
            connections: 1,
            chunk_size: size.max(64 * 1024),
            traffic_tier: TrafficTier::High,
            reason: "small file (< 1 MiB); single connection avoids handshake overhead".into(),
        };
    }

    let (traffic_tier, ideal_conns) = match estimated_bps {
        Some(bps) if bps < 256 * 1024 => (TrafficTier::Low, 2),
        Some(bps) if bps < 3 * 1024 * 1024 => (TrafficTier::Normal, 4),
        Some(bps) if bps < 20 * 1024 * 1024 => (TrafficTier::High, 8),
        Some(_) => (TrafficTier::High, 12),
        None => (TrafficTier::Normal, 8),
    };

    let connections = ideal_conns.min(max_c).max(1);
    let target_chunks = (connections as u64) * 4;
    let chunk_size = (size / target_chunks).clamp(256 * 1024, 8 * 1024 * 1024);

    SmartPlan {
        connections,
        chunk_size,
        traffic_tier,
        reason: format!(
            "{:?} traffic tier -> {connections} connections, {} KiB chunks",
            traffic_tier,
            chunk_size / 1024
        ),
    }
}

// ---------------------------------------------------------------------------
// 1. EWMA + UCB1 Adaptive Mirror / Traffic Bandit

#[derive(Debug, Clone)]
struct MirrorArm {
    url: String,
    ewma_bps: f64,
    samples: u64,
    failures: u32,
    cooldown_until: Option<Instant>,
}

/// Adaptive mirror selector using EWMA throughput tracking + UCB1 exploration.
///
/// - Probes every mirror at least once so fast mirrors are discovered quickly.
/// - Routes subsequent chunks overwhelmingly to the fastest healthy mirror.
/// - Demotes slow/low-traffic or failing mirrors automatically with cooldowns.
pub struct TrafficBandit {
    arms: Vec<MirrorArm>,
    total_pulls: u64,
    /// Exploration constant (smaller = greedier toward fastest traffic).
    exploration: f64,
}

impl TrafficBandit {
    /// Construct a bandit from `[primary, mirrors...]`.
    pub fn new(primary: impl Into<String>, mirrors: &[String]) -> Self {
        let mut arms = Vec::with_capacity(1 + mirrors.len());
        arms.push(MirrorArm {
            url: primary.into(),
            ewma_bps: 0.0,
            samples: 0,
            failures: 0,
            cooldown_until: None,
        });
        for m in mirrors {
            if !m.trim().is_empty() {
                arms.push(MirrorArm {
                    url: m.clone(),
                    ewma_bps: 0.0,
                    samples: 0,
                    failures: 0,
                    cooldown_until: None,
                });
            }
        }
        Self {
            arms,
            total_pulls: 0,
            exploration: 0.35,
        }
    }

    /// Select the index and URL of the best mirror for the next chunk.
    pub fn select(&mut self) -> (usize, String) {
        if self.arms.len() == 1 {
            self.total_pulls += 1;
            return (0, self.arms[0].url.clone());
        }

        let now = Instant::now();

        // 1. Explore any untested, non-cooled-down mirror first.
        if let Some((idx, arm)) = self
            .arms
            .iter()
            .enumerate()
            .find(|(_, a)| a.samples == 0 && a.cooldown_until.is_none_or(|t| now >= t))
        {
            self.total_pulls += 1;
            return (idx, arm.url.clone());
        }

        // 2. Score each mirror with normalized EWMA throughput + UCB1 exploration bonus.
        let max_bps = self
            .arms
            .iter()
            .map(|a| a.ewma_bps)
            .fold(1.0_f64, f64::max);
        let ln_total = ((self.total_pulls.max(1) + 1) as f64).ln();

        let mut best_idx = 0usize;
        let mut best_score = f64::NEG_INFINITY;

        for (i, arm) in self.arms.iter().enumerate() {
            let cooling = arm.cooldown_until.is_some_and(|t| now < t);
            let exploit = arm.ewma_bps / max_bps;
            let explore = self.exploration * (ln_total / (arm.samples as f64 + 1.0)).sqrt();
            let fail_penalty = (arm.failures as f64) * 0.25;
            let cool_penalty = if cooling { 2.0 } else { 0.0 };
            let score = exploit + explore - fail_penalty - cool_penalty;

            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        self.total_pulls += 1;
        (best_idx, self.arms[best_idx].url.clone())
    }

    /// Record a successful chunk transfer on `arm_idx` to update its EWMA throughput.
    pub fn record_success(&mut self, arm_idx: usize, bytes: u64, elapsed: Duration) {
        let Some(arm) = self.arms.get_mut(arm_idx) else {
            return;
        };
        let secs = elapsed.as_secs_f64().max(0.0005);
        let bps = bytes as f64 / secs;
        arm.ewma_bps = if arm.samples == 0 {
            bps
        } else {
            0.45 * bps + 0.55 * arm.ewma_bps
        };
        arm.samples += 1;
        arm.failures = arm.failures.saturating_sub(1);
        arm.cooldown_until = None;
    }

    /// Record a failed attempt on `arm_idx`, applying a short exponential cooldown
    /// so subsequent chunks immediately route to healthier mirrors.
    pub fn record_failure(&mut self, arm_idx: usize) {
        let Some(arm) = self.arms.get_mut(arm_idx) else {
            return;
        };
        arm.failures = arm.failures.saturating_add(1);
        let backoff_ms = 250u64.saturating_mul(1u64 << arm.failures.min(5));
        arm.cooldown_until = Some(Instant::now() + Duration::from_millis(backoff_ms));
    }

    /// Return `(url, ewma_bps, samples)` telemetry for diagnostics.
    pub fn stats(&self) -> Vec<(&str, f64, u64)> {
        self.arms
            .iter()
            .map(|a| (a.url.as_str(), a.ewma_bps, a.samples))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 2. Traffic-Aware SRPT Priority Queue (`BinaryHeap`)

/// Candidate download task evaluated by [`SmartQueue`].
#[derive(Debug, Clone)]
pub struct SmartTaskCandidate {
    pub id: u64,
    pub priority: TaskPriority,
    /// Remaining bytes to finish (`None` = unknown size).
    pub remaining_bytes: Option<u64>,
    /// Observed or estimated throughput in bytes/sec.
    pub traffic_bps: u64,
    /// Seconds spent waiting in queue (prevents starvation of low-traffic tasks).
    pub wait_secs: u64,
}

impl SmartTaskCandidate {
    /// Compute composite scheduling score (higher = scheduled sooner).
    ///
    /// - High user priority dominates.
    /// - High-traffic & nearly-complete (SRPT) tasks finish first.
    /// - Low-traffic/throttled tasks (`< 256 KB/s`) receive a penalty so they
    ///   never block fast downloads, with gentle aging to guarantee completion.
    pub fn score(&self) -> i64 {
        let prio_base: i64 = match self.priority {
            TaskPriority::High => 10_000,
            TaskPriority::Normal => 5_000,
            TaskPriority::Low => 1_000,
        };

        // Traffic health score: reward fast traffic, penalize low/stalled traffic.
        let traffic_score: i64 = if self.traffic_bps == 0 {
            // Unknown / brand-new task: give neutral-positive probe priority.
            500
        } else if self.traffic_bps < 256 * 1024 {
            // Low traffic is NOT priority.
            -2_500
        } else {
            let mbps = (self.traffic_bps * 8) / 1_000_000;
            (mbps.min(1_000) as i64) * 3
        };

        // Shortest Remaining Processing Time (SRPT) boost: smaller remaining work finishes first.
        let srpt_boost: i64 = match self.remaining_bytes {
            Some(rem) if rem <= 5 * 1024 * 1024 => 1_500,
            Some(rem) if rem <= 50 * 1024 * 1024 => 800,
            Some(rem) if rem <= 500 * 1024 * 1024 => 250,
            _ => 0,
        };

        // Anti-starvation aging: +15 points per waiting second (capped at +4000).
        let aging = (self.wait_secs.min(266) * 15) as i64;

        prio_base + traffic_score + srpt_boost + aging
    }
}

struct ScoredTask {
    score: i64,
    candidate: SmartTaskCandidate,
}

impl PartialEq for ScoredTask {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.candidate.id == other.candidate.id
    }
}
impl Eq for ScoredTask {}

impl PartialOrd for ScoredTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredTask {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.candidate.id.cmp(&self.candidate.id))
    }
}

/// Priority queue selecting the next best download task to execute.
#[derive(Default)]
pub struct SmartQueue {
    heap: BinaryHeap<ScoredTask>,
}

impl SmartQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, candidate: SmartTaskCandidate) {
        let score = candidate.score();
        self.heap.push(ScoredTask { score, candidate });
    }

    pub fn pop_best(&mut self) -> Option<SmartTaskCandidate> {
        self.heap.pop().map(|s| s.candidate)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandit_converges_on_fastest_mirror_and_avoids_slow_or_failing() {
        let mut bandit = TrafficBandit::new(
            "https://slow.mirror/file.bin",
            &[
                "https://fast.mirror/file.bin".into(),
                "https://broken.mirror/file.bin".into(),
            ],
        );

        // Initial 3 pulls explore all 3 arms.
        let (i0, _) = bandit.select();
        bandit.record_success(i0, 100 * 1024, Duration::from_secs(1)); // 100 KB/s (slow)

        let (i1, _) = bandit.select();
        bandit.record_success(i1, 20 * 1024 * 1024, Duration::from_secs(1)); // 20 MB/s (fast)

        let (i2, _) = bandit.select();
        bandit.record_failure(i2); // broken

        // Next 20 selections should strongly favor the fast mirror (index 1).
        let mut fast_picks = 0;
        for _ in 0..20 {
            let (idx, _) = bandit.select();
            if idx == 1 {
                fast_picks += 1;
                bandit.record_success(idx, 20 * 1024 * 1024, Duration::from_secs(1));
            } else {
                bandit.record_success(idx, 100 * 1024, Duration::from_secs(1));
            }
        }
        assert!(fast_picks >= 16, "expected fast mirror to dominate, got {fast_picks}/20");
    }

    #[test]
    fn smart_queue_prioritizes_high_traffic_and_fast_finishing_over_low_traffic() {
        let mut q = SmartQueue::new();
        // Low-traffic huge file
        q.push(SmartTaskCandidate {
            id: 1,
            priority: TaskPriority::Normal,
            remaining_bytes: Some(2 * 1024 * 1024 * 1024),
            traffic_bps: 80 * 1024, // 80 KB/s (low traffic)
            wait_secs: 0,
        });
        // High-traffic fast-finishing file
        q.push(SmartTaskCandidate {
            id: 2,
            priority: TaskPriority::Normal,
            remaining_bytes: Some(4 * 1024 * 1024),
            traffic_bps: 25 * 1024 * 1024, // 25 MB/s
            wait_secs: 0,
        });
        // Explicit High priority task
        q.push(SmartTaskCandidate {
            id: 3,
            priority: TaskPriority::High,
            remaining_bytes: Some(100 * 1024 * 1024),
            traffic_bps: 10 * 1024 * 1024,
            wait_secs: 0,
        });

        assert_eq!(q.pop_best().unwrap().id, 3); // High priority first
        assert_eq!(q.pop_best().unwrap().id, 2); // Fast high-traffic SRPT second
        assert_eq!(q.pop_best().unwrap().id, 1); // Low-traffic deprioritized last
    }

    #[test]
    fn initial_plan_adapts_connections_to_file_size_and_traffic() {
        let small = decide_initial_plan(Some(256 * 1024), true, Some(50_000_000), 16);
        assert_eq!(small.connections, 1);

        let low_traffic = decide_initial_plan(Some(100 * 1024 * 1024), true, Some(100 * 1024), 16);
        assert_eq!(low_traffic.traffic_tier, TrafficTier::Low);
        assert_eq!(low_traffic.connections, 2);

        let fast_large =
            decide_initial_plan(Some(500 * 1024 * 1024), true, Some(50 * 1024 * 1024), 16);
        assert_eq!(fast_large.traffic_tier, TrafficTier::High);
        assert_eq!(fast_large.connections, 12);
    }
}
