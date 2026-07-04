//! Per-backend metrics the autoscaler reads from. Deliberately lightweight:
//! no external metrics backend, no async locking on the hot path — just
//! atomics and short-lived std::sync::Mutex sections around small buffers.

use dashmap::DashMap;
use lw_config::ScalingMetric;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many recent latency samples to keep per backend for the p95
/// calculation. Small enough to be cheap, large enough to smooth out noise.
const LATENCY_WINDOW: usize = 200;
/// How far back to count requests for requests-per-second.
const RPS_WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
struct BackendStats {
    active: AtomicUsize,
    recent_latencies_ms: Mutex<VecDeque<u64>>,
    recent_request_times: Mutex<VecDeque<Instant>>,
}

/// Decrements a backend's active-connection count when dropped. Hold this
/// for the *entire* request lifetime, including response body streaming —
/// that's what makes a handful of slow, large downloads (the exam-card
/// case) show up as sustained load instead of a single fast blip that's
/// invisible to the autoscaler by the time it ticks.
pub struct ActiveGuard {
    stats: Option<Arc<BackendStats>>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Some(stats) = &self.stats {
            stats.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Default)]
pub struct MetricsRegistry {
    backends: Arc<DashMap<String, Arc<BackendStats>>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend so its metrics start at zero. Call this right
    /// after adding an instance to the `BackendPool`.
    pub fn track(&self, container_id: &str) {
        self.backends
            .entry(container_id.to_string())
            .or_insert_with(|| Arc::new(BackendStats::default()));
    }

    /// Drop a backend's metrics. Call this right after removing an instance
    /// from the `BackendPool` (e.g. the autoscaler scaling down).
    pub fn untrack(&self, container_id: &str) {
        self.backends.remove(container_id);
    }

    /// Marks the start of a request to `container_id`. Keep the returned
    /// guard alive until the response is fully sent.
    pub fn begin_active(&self, container_id: &str) -> ActiveGuard {
        match self.backends.get(container_id) {
            Some(stats) => {
                stats.active.fetch_add(1, Ordering::Relaxed);
                ActiveGuard { stats: Some(stats.clone()) }
            }
            None => ActiveGuard { stats: None },
        }
    }

    /// Records time-to-first-byte for one request (used for the p95
    /// latency metric) and counts it toward requests-per-second.
    pub fn record_latency(&self, container_id: &str, elapsed: Duration) {
        let Some(stats) = self.backends.get(container_id) else {
            return;
        };

        {
            let mut lat = stats.recent_latencies_ms.lock().unwrap();
            lat.push_back(elapsed.as_millis() as u64);
            if lat.len() > LATENCY_WINDOW {
                lat.pop_front();
            }
        }
        {
            let mut times = stats.recent_request_times.lock().unwrap();
            let now = Instant::now();
            times.push_back(now);
            while times.front().is_some_and(|t| now.duration_since(*t) > RPS_WINDOW) {
                times.pop_front();
            }
        }
    }

    pub fn active_for(&self, container_id: &str) -> u64 {
        self.backends
            .get(container_id)
            .map(|s| s.active.load(Ordering::Relaxed) as u64)
            .unwrap_or(0)
    }

    pub fn active_connections_total(&self) -> u64 {
        self.backends
            .iter()
            .map(|e| e.active.load(Ordering::Relaxed) as u64)
            .sum()
    }

    pub fn p95_latency_ms(&self) -> Option<f64> {
        let mut all: Vec<u64> = Vec::new();
        for entry in self.backends.iter() {
            all.extend(entry.recent_latencies_ms.lock().unwrap().iter().copied());
        }
        if all.is_empty() {
            return None;
        }
        all.sort_unstable();
        let idx = (((all.len() as f64) * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(all.len() - 1);
        Some(all[idx] as f64)
    }

    pub fn requests_per_second(&self) -> f64 {
        let total: usize = self
            .backends
            .iter()
            .map(|e| e.recent_request_times.lock().unwrap().len())
            .sum();
        total as f64 / RPS_WINDOW.as_secs_f64()
    }

    /// Fetches whatever value corresponds to a service's configured scaling
    /// metric. Returns `None` when there's no data yet (no traffic) or the
    /// metric isn't implemented yet (CPU — needs Docker stats polling,
    /// planned for a later step). The autoscaler treats `None` as "hold".
    pub fn value_for(&self, metric: ScalingMetric) -> Option<f64> {
        match metric {
            ScalingMetric::P95LatencyMs => self.p95_latency_ms(),
            ScalingMetric::ActiveConnections => Some(self.active_connections_total() as f64),
            ScalingMetric::RequestsPerSecond => Some(self.requests_per_second()),
            ScalingMetric::Cpu => None,
        }
    }
}
