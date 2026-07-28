//! Thin Redis-backed job queue.
//!
//! Two lists per job kind:
//!   - `<key>:pending`    — RPUSH on enqueue, BLPOP to claim
//!   - `<key>:dead`       — failed jobs after exhausting retries
//!
//! No visibility-timeout / in-flight tracking in v1: a worker that crashes
//! mid-job loses that job. We tolerate this because (a) we ignore reorgs
//! anyway, (b) the per-block tx volume regenerates the state quickly, and
//! (c) it keeps the implementation under 100 LOC.

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const QUEUE_KEY: &str = "analyzer:queue:pending";
pub const DEAD_KEY: &str = "analyzer:queue:dead";

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzeTxJob {
    pub chain_id: u64,
    pub tx_hash: [u8; 32],
    pub block_number: u64,
    pub tx_index: u64,
    /// Number of times this job has been claimed and failed. 0 = first try.
    pub attempt: u32,
    /// Unix seconds when the job was first enqueued. Requeues keep the
    /// original value so the TTL spans the job's whole lifetime, retries
    /// included. `0` on payloads enqueued before this field existed; those
    /// never expire.
    #[serde(default)]
    pub enqueued_at: i64,
}

impl AnalyzeTxJob {
    /// Seconds since first enqueue, or `None` for legacy payloads without a
    /// timestamp.
    pub fn age_secs(&self, now: i64) -> Option<i64> {
        (self.enqueued_at > 0).then(|| now - self.enqueued_at)
    }
}

#[derive(Clone)]
pub struct Queue {
    conn: ConnectionManager,
}

impl Queue {
    pub async fn connect(redis_url: &str) -> Result<Self, QueueError> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn })
    }

    pub async fn enqueue(&self, job: &AnalyzeTxJob) -> Result<(), QueueError> {
        let payload = serde_json::to_vec(job)?;
        let mut conn = self.conn.clone();
        let _: () = conn.rpush(QUEUE_KEY, payload).await?;
        Ok(())
    }

    pub async fn depth(&self) -> Result<usize, QueueError> {
        let mut conn = self.conn.clone();
        let n: usize = conn.llen(QUEUE_KEY).await?;
        Ok(n)
    }

    /// Block until a job is available or `timeout` elapses.
    pub async fn claim(&self, timeout: Duration) -> Result<Option<AnalyzeTxJob>, QueueError> {
        let mut conn = self.conn.clone();
        // BLPOP returns Option<(key, value)>; we discard the key.
        let result: Option<(String, Vec<u8>)> =
            conn.blpop(QUEUE_KEY, timeout.as_secs_f64()).await?;
        match result {
            Some((_, payload)) => {
                let job: AnalyzeTxJob = serde_json::from_slice(&payload)?;
                Ok(Some(job))
            }
            None => Ok(None),
        }
    }

    /// Re-queue a failed job for retry.
    pub async fn requeue(&self, job: &AnalyzeTxJob) -> Result<(), QueueError> {
        self.enqueue(job).await
    }

    /// Publish the latest observed chain head to a small Redis key so the
    /// admin health view can compute "blocks behind head" without spending an
    /// `eth_blockNumber` call of its own.
    pub async fn publish_last_head(&self, block_number: u64) -> Result<(), QueueError> {
        let mut conn = self.conn.clone();
        let _: () = redis::cmd("SET")
            .arg(crate::state::LAST_HEAD_KEY)
            .arg(block_number)
            .arg("EX")
            .arg(crate::state::LAST_HEAD_TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    pub async fn dead_letter(&self, job: &AnalyzeTxJob, reason: &str) -> Result<(), QueueError> {
        #[derive(Serialize)]
        struct Dead<'a> {
            #[serde(flatten)]
            job: &'a AnalyzeTxJob,
            failed_at: i64,
            reason: &'a str,
        }
        let payload = serde_json::to_vec(&Dead {
            job,
            failed_at: chrono::Utc::now().timestamp(),
            reason,
        })?;
        let mut conn = self.conn.clone();
        let _: () = conn.rpush(DEAD_KEY, payload).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AnalyzeTxJob;

    fn job(enqueued_at: i64) -> AnalyzeTxJob {
        AnalyzeTxJob {
            chain_id: 1,
            tx_hash: [7u8; 32],
            block_number: 100,
            tx_index: 3,
            attempt: 0,
            enqueued_at,
        }
    }

    #[test]
    fn legacy_payload_without_enqueued_at_never_expires() {
        let mut v = serde_json::to_value(job(1_234)).unwrap();
        v.as_object_mut().unwrap().remove("enqueued_at");
        let legacy: AnalyzeTxJob = serde_json::from_value(v).unwrap();
        assert_eq!(legacy.enqueued_at, 0);
        assert_eq!(legacy.age_secs(i64::MAX), None);
    }

    #[test]
    fn age_is_measured_from_first_enqueue() {
        assert_eq!(job(900).age_secs(1_000), Some(100));
    }
}
