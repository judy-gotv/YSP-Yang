use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use rand::{rngs::OsRng, Rng};
use serde::Serialize;
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug, Clone)]
pub struct FlowOptions {
    pub concurrency: usize,
    pub min_interval: Duration,
    pub jitter: Duration,
    pub queue_timeout: Duration,
    pub retries: usize,
    pub retry_delay: Duration,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FlowStats {
    pub enqueued: u64,
    pub started: u64,
    pub completed: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub max_queue_depth: usize,
    pub last_started_at: u128,
    pub last_scheduled_at: u128,
    pub last_completed_at: u128,
    pub last_error_at: u128,
    pub last_error: String,
    pub total_wait_ms: u128,
    pub total_run_ms: u128,
}

#[derive(Debug, Serialize)]
pub struct FlowSnapshot {
    pub concurrency: usize,
    pub active: usize,
    pub queued: usize,
    pub min_interval_ms: u64,
    pub jitter_ms: u64,
    pub queue_timeout_ms: u64,
    pub retries: usize,
    pub retry_delay_ms: u64,
    pub average_wait_ms: u128,
    pub average_run_ms: u128,
    #[serde(flatten)]
    pub stats: FlowStats,
}

struct Inner {
    options: FlowOptions,
    semaphore: Arc<Semaphore>,
    next_start_at: Mutex<Instant>,
    stats: Mutex<FlowStats>,
    queued: Mutex<usize>,
}

#[derive(Clone)]
pub struct ApiFlowLimiter {
    inner: Arc<Inner>,
}

impl ApiFlowLimiter {
    pub fn new(options: FlowOptions) -> Self {
        let concurrency = options.concurrency.max(1);
        Self {
            inner: Arc::new(Inner {
                options,
                semaphore: Arc::new(Semaphore::new(concurrency)),
                next_start_at: Mutex::new(Instant::now()),
                stats: Mutex::new(FlowStats::default()),
                queued: Mutex::new(0),
            }),
        }
    }

    pub async fn run<F, T>(&self, task: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send,
        T: Send,
    {
        let enqueued_at = Instant::now();
        {
            let mut queued = self.inner.queued.lock().await;
            *queued += 1;
            let mut stats = self.inner.stats.lock().await;
            stats.enqueued += 1;
            stats.max_queue_depth = stats.max_queue_depth.max(*queued);
        }

        let permit = match tokio::time::timeout(
            self.inner.options.queue_timeout,
            self.inner.semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(anyhow!("api flow semaphore closed")),
            Err(_) => {
                self.finish_waiting_timeout().await;
                return Err(anyhow!(
                    "api flow queue timeout after {}ms",
                    enqueued_at.elapsed().as_millis()
                ));
            }
        };
        {
            let mut queued = self.inner.queued.lock().await;
            *queued = queued.saturating_sub(1);
        }

        let scheduled_at = {
            let mut next = self.inner.next_start_at.lock().await;
            let now = Instant::now();
            let jitter_ms = if self.inner.options.jitter.is_zero() {
                0
            } else {
                OsRng.gen_range(0..=self.inner.options.jitter.as_millis() as u64)
            };
            let scheduled_at = (*next).max(now) + Duration::from_millis(jitter_ms);
            *next = scheduled_at + self.inner.options.min_interval;
            scheduled_at
        };
        {
            let mut stats = self.inner.stats.lock().await;
            stats.last_scheduled_at = now_ms();
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled_at)).await;

        let started_at = Instant::now();
        {
            let mut stats = self.inner.stats.lock().await;
            stats.started += 1;
            stats.last_started_at = now_ms();
            stats.total_wait_ms += started_at.duration_since(enqueued_at).as_millis();
        }

        let result = task.await;
        drop(permit);

        let mut stats = self.inner.stats.lock().await;
        match &result {
            Ok(_) => {
                stats.completed += 1;
                stats.last_completed_at = now_ms();
                stats.total_run_ms += started_at.elapsed().as_millis();
            }
            Err(error) => {
                stats.failed += 1;
                stats.last_error_at = now_ms();
                stats.last_error = error.to_string().chars().take(500).collect();
            }
        }
        result
    }

    pub async fn snapshot(&self) -> FlowSnapshot {
        let stats = self.inner.stats.lock().await.clone();
        let queued = *self.inner.queued.lock().await;
        let available = self.inner.semaphore.available_permits();
        let active = self.inner.options.concurrency.saturating_sub(available);
        FlowSnapshot {
            concurrency: self.inner.options.concurrency,
            active,
            queued,
            min_interval_ms: self.inner.options.min_interval.as_millis() as u64,
            jitter_ms: self.inner.options.jitter.as_millis() as u64,
            queue_timeout_ms: self.inner.options.queue_timeout.as_millis() as u64,
            retries: self.inner.options.retries,
            retry_delay_ms: self.inner.options.retry_delay.as_millis() as u64,
            average_wait_ms: if stats.started == 0 {
                0
            } else {
                stats.total_wait_ms / stats.started as u128
            },
            average_run_ms: if stats.completed == 0 {
                0
            } else {
                stats.total_run_ms / stats.completed as u128
            },
            stats,
        }
    }

    async fn finish_waiting_timeout(&self) {
        {
            let mut queued = self.inner.queued.lock().await;
            *queued = queued.saturating_sub(1);
        }
        let mut stats = self.inner.stats.lock().await;
        stats.timed_out += 1;
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
