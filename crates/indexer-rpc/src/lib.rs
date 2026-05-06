//! Rate limiting + retry layer for RPC calls.
//!
//! Two primitives:
//! - [`RateLimiter`]: a global token bucket + bounded-concurrency semaphore.
//!   Components must `acquire(weight)` before any RPC-heavy operation.
//! - [`with_retry`]: helper that retries a fallible async operation on
//!   transient failures, with jittered exponential backoff.
//!
//! Per-method token weights are exposed in [`weights`]. They roughly track
//! Alphine compute-unit costs across major providers (Alchemy / QuickNode).
//! Conservatively sized so we stay under quota even when the underlying
//! gas-analyzer crates make extra RPC calls we don't directly mediate.
//!
//! # Known v1 limitation
//!
//! `gas-analyzer-evmsketch::GasKillerEvmSketch` constructs its own internal
//! `alloy` provider when `builder().build()` is called. Storage reads issued
//! during gas estimation are *not* gated by this limiter. We compensate by
//! choosing the [`weights::ANALYZE_TX`] charge generously — it accounts for
//! the trace fetch *plus* an estimate of the bypassed storage reads.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::{Quota, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use rand::Rng;
use tokio::sync::Semaphore;

pub mod weights {
    //! Token weights per logical operation. One token ≈ 1 RPC compute unit.
    //!
    //! Values are conservative — better to over-charge than over-consume the
    //! provider quota. Tune via observed `rpc_token_bucket_remaining` metric.

    pub const HEAD_POLL: u32 = 1; // eth_blockNumber
    pub const BLOCK_HEADER: u32 = 12; // eth_getBlockByNumber (no full txs)
    pub const BLOCK_FULL: u32 = 25; // eth_getBlockByNumber (full=true)
    pub const RECEIPT: u32 = 15; // eth_getTransactionReceipt
    pub const TX_BY_HASH: u32 = 15; // eth_getTransactionByHash
    pub const TRACE_TX: u32 = 80; // debug_traceTransaction
    /// One-shot weight to cover an entire EvmSketch tx analysis: receipt +
    /// trace + block + preceding-tx fetch + the bypassed storage reads inside
    /// `GasKillerEvmSketch`. Generous on purpose.
    pub const ANALYZE_TX: u32 = 250;
}

#[derive(Debug, thiserror::Error)]
pub enum RetryError<E> {
    #[error("operation failed after {attempts} attempts: {source}")]
    Exhausted { attempts: u32, source: E },
}

#[derive(Debug, Clone)]
pub struct RateLimiterConfig {
    /// Sustained tokens per second. Rule of thumb: 80% of provider plan ceiling.
    pub rps_budget: u32,
    /// Burst capacity — how many tokens we can spend instantaneously.
    pub burst: u32,
    /// Hard cap on simultaneous outbound RPC operations regardless of bucket.
    pub max_concurrency: usize,
}

impl Default for RateLimiterConfig {
    fn default() -> Self {
        Self {
            rps_budget: 240,    // ~80% of a 300 rps plan
            burst: 60,          // small headroom for short spikes
            max_concurrency: 16,
        }
    }
}

type Bucket = governor::RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Global token bucket + bounded-concurrency semaphore.
///
/// Cheap to clone (`Arc` internally). Share one instance across head-tracker
/// and worker tasks in the same process.
#[derive(Clone)]
pub struct RateLimiter {
    bucket: Arc<Bucket>,
    semaphore: Arc<Semaphore>,
    /// The configured burst capacity. We clamp single-acquire weights to this
    /// because `governor::RateLimiter::until_n_ready(N)` errors immediately
    /// when N > burst (it can never be satisfied).
    burst: u32,
}

impl RateLimiter {
    pub fn new(config: RateLimiterConfig) -> Self {
        let rps = NonZeroU32::new(config.rps_budget.max(1)).expect("rps_budget >= 1");
        let burst_n = NonZeroU32::new(config.burst.max(1)).expect("burst >= 1");
        let quota = Quota::per_second(rps).allow_burst(burst_n);
        Self {
            bucket: Arc::new(governor::RateLimiter::direct(quota)),
            semaphore: Arc::new(Semaphore::new(config.max_concurrency)),
            burst: burst_n.get(),
        }
    }

    /// Acquire up to `weight` tokens AND a concurrency slot. The weight is
    /// clamped to the configured burst — requesting more would error
    /// immediately because the bucket can never hold that many tokens.
    /// Callers that genuinely need a heavy charge should reach this in
    /// multiple smaller acquires.
    pub async fn acquire(&self, weight: u32) -> Permit {
        let requested = weight.max(1);
        if requested > self.burst {
            tracing::warn!(
                requested,
                burst = self.burst,
                "weight exceeds burst; clamping (raise RPC_BURST to charge accurately)"
            );
        }
        let clamped = requested.min(self.burst);
        let n = NonZeroU32::new(clamped).expect("clamped weight >= 1");
        if let Err(e) = self.bucket.until_n_ready(n).await {
            // Should not happen now that we clamp to burst, but log just in case.
            tracing::error!(?e, "rate limiter bucket error");
        }
        let semaphore_permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore not closed");
        Permit {
            _semaphore_permit: semaphore_permit,
        }
    }
}

/// Held by callers for the duration of an RPC operation. Drop to release the
/// concurrency slot. Tokens are non-refundable.
pub struct Permit {
    _semaphore_permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// Retry an async fallible operation with jittered exponential backoff.
///
/// `is_transient` returns true for retryable errors (e.g. 429, 5xx, connection
/// reset). Permanent errors (4xx other than 429) bypass retries.
pub async fn with_retry<F, Fut, T, E, P>(
    config: &RetryConfig,
    is_transient: P,
    mut f: F,
) -> Result<T, RetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
    P: Fn(&E) -> bool,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt >= config.max_attempts || !is_transient(&e) => {
                return Err(RetryError::Exhausted {
                    attempts: attempt,
                    source: e,
                });
            }
            Err(e) => {
                let backoff = backoff_with_jitter(config, attempt);
                tracing::warn!(
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "rpc transient error, retrying"
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Default `is_transient` predicate: retry on 5xx, 429, and common network
/// transport failures (connection reset, EOF, timeouts). Conservative —
/// errors that don't match are treated as permanent.
///
/// Pattern-matches on the `Display` form of the error because alloy's
/// transport errors don't expose status codes through a stable typed API
/// (yet); the rendered string is the most reliable signal across providers.
pub fn is_transient_rpc_error<E: std::fmt::Display>(e: &E) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("503")
        || s.contains("502")
        || s.contains("504")
        || s.contains("429")
        || s.contains("error sending request")
        || s.contains("connection reset")
        || s.contains("connection closed")
        || s.contains("connection refused")
        || s.contains("timed out")
        || s.contains("timeout")
        || s.contains("temporarily unavailable")
        || s.contains("unexpected eof")
}

fn backoff_with_jitter(config: &RetryConfig, attempt: u32) -> Duration {
    // 200ms * 4^(attempt-1) -> 200ms, 800ms, 3.2s. Capped at max_delay.
    let exp = config.base_delay.as_millis() as u64 * 4u64.pow(attempt.saturating_sub(1));
    let exp = exp.min(config.max_delay.as_millis() as u64);
    let jitter = rand::thread_rng().gen_range(0.75..=1.25);
    Duration::from_millis((exp as f64 * jitter) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn limiter_serializes_above_burst() {
        // 10 rps, 5 burst — the 6th immediate acquire should wait at least ~100ms.
        let limiter = RateLimiter::new(RateLimiterConfig {
            rps_budget: 10,
            burst: 5,
            max_concurrency: 32,
        });

        let start = std::time::Instant::now();
        let mut handles = vec![];
        for _ in 0..6 {
            let l = limiter.clone();
            handles.push(tokio::spawn(async move {
                let _p = l.acquire(1).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80),
            "expected ~100ms wait, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_succeeds_on_third_attempt() {
        let attempts = AtomicU32::new(0);
        let result: Result<u32, RetryError<&'static str>> = with_retry(
            &RetryConfig {
                max_attempts: 5,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
            },
            |_| true,
            || {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n < 3 { Err("fail") } else { Ok(n) }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 3);
    }

    #[tokio::test]
    async fn retry_gives_up_on_permanent_error() {
        let result: Result<(), RetryError<&'static str>> = with_retry(
            &RetryConfig {
                max_attempts: 5,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
            },
            |_| false, // never transient
            || async { Err("permanent") },
        )
        .await;
        match result {
            Err(RetryError::Exhausted { attempts, .. }) => assert_eq!(attempts, 1),
            _ => panic!("expected exhausted after 1 attempt"),
        }
    }
}
