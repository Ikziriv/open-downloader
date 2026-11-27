//! Cancellation-aware retry policy with exponential backoff and deterministic jitter.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::Error;

/// Configuration for retrying transient network / HTTP errors.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts including the initial try (`1` = no retries).
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub initial_delay: Duration,
    /// Upper bound on backoff delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Disable retries completely (single attempt).
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// Compute the backoff delay for 0-indexed retry `attempt` (0 = first retry).
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(16);
        let factor = 1u32 << shift;
        let base = self
            .initial_delay
            .saturating_mul(factor)
            .min(self.max_delay);
        // Deterministic light jitter (±12.5%) using attempt index so concurrent
        // workers don't herd without needing an RNG dependency.
        let base_ms = base.as_millis() as u64;
        if base_ms < 8 {
            return base;
        }
        let jitter_range = base_ms / 8;
        let offset = ((attempt as u64).wrapping_mul(2654435761) % (jitter_range * 2 + 1))
            .saturating_sub(jitter_range);
        Duration::from_millis(base_ms.saturating_add(offset))
    }
}

/// Returns `true` when `err` is likely transient and safe to retry.
pub fn is_retryable(err: &Error) -> bool {
    match err {
        Error::Http(e) => {
            if e.is_timeout() || e.is_connect() || e.is_body() || e.is_request() {
                return true;
            }
            if let Some(status) = e.status() {
                return is_retryable_status(status.as_u16());
            }
            false
        }
        Error::HttpStatus(code) => is_retryable_status(*code),
        Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::UnexpectedEof
        ),
        Error::Cancelled
        | Error::RangeRefused(_)
        | Error::Json(_)
        | Error::Worker(_)
        | Error::Stream(_)
        | Error::UnsupportedProtocol(_)
        | Error::TaskNotFound(_)
        | Error::InvalidState(_)
        | Error::ChecksumMismatch { .. } => false,
    }
}

fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Execute `op` with retries according to `policy`, aborting immediately if
/// `cancel` is triggered between attempts.
pub async fn retry_async<T, F, Fut>(
    policy: &RetryPolicy,
    cancel: &CancellationToken,
    mut op: F,
) -> Result<T, Error>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(err) => {
                attempt += 1;
                if attempt >= max_attempts || !is_retryable(&err) {
                    return Err(err);
                }
                let delay = policy.delay_for(attempt - 1);
                if !delay.is_zero() {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retries_transient_errors_until_success() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        };
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_ref = Arc::clone(&calls);

        let res: Result<u32, Error> = retry_async(&policy, &cancel, |_| {
            let calls = Arc::clone(&calls_ref);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(Error::HttpStatus(503))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_errors() {
        let policy = RetryPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        };
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_ref = Arc::clone(&calls);

        let res: Result<(), Error> = retry_async(&policy, &cancel, |_| {
            let calls = Arc::clone(&calls_ref);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::HttpStatus(404))
            }
        })
        .await;

        assert!(matches!(res, Err(Error::HttpStatus(404))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn respects_cancellation_during_backoff() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_secs(10),
            max_delay: Duration::from_secs(10),
        };
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            c2.cancel();
        });

        let res: Result<(), Error> =
            retry_async(&policy, &cancel, |_| async { Err(Error::HttpStatus(502)) }).await;
        assert!(matches!(res, Err(Error::Cancelled)));
    }
}
