//! The control loop that actually solves the "portal falls over at 9am"
//! problem: every tick it checks the current schedule floor and the live
//! metric value, asks `lw_config::decide_scale_action` what to do, and
//! calls the orchestrator to spawn or stop a container accordingly.
//!
//! The decision logic itself lives in `lw-config` (pure functions, unit
//! tested there without needing Docker or a clock). This crate is just the
//! wiring: gather inputs, call the decision function, act on the result.

use chrono::Timelike;
use lw_config::{decide_scale_action, current_scheduled_floor, ScaleAction, ScalingConfig, ServiceMeta};
use lw_orchestrator::{Instance, ScalingDriver};
use lw_proxy::{BackendPool, MetricsRegistry};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub struct Autoscaler {
    driver: Arc<dyn ScalingDriver>,
    pool: BackendPool,
    metrics: MetricsRegistry,
    service: ServiceMeta,
    scaling: ScalingConfig,
    /// Guards against scaling more than once per `cooldown_secs`, in either
    /// direction — this is what stops the classic "scale up, latency drops,
    /// scale down immediately, latency spikes again" flapping loop.
    last_scale: Mutex<Instant>,
    tick_interval: Duration,
}

impl Autoscaler {
    pub fn new(
        driver: Arc<dyn ScalingDriver>,
        pool: BackendPool,
        metrics: MetricsRegistry,
        service: ServiceMeta,
        scaling: ScalingConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            driver,
            pool,
            metrics,
            service,
            scaling,
            // Start with no cooldown in effect, so a schedule floor can be
            // caught up to immediately on startup rather than waiting.
            last_scale: Mutex::new(Instant::now() - Duration::from_secs(24 * 60 * 60)),
            tick_interval: Duration::from_secs(5),
        })
    }

    /// Runs forever. Intended to be `tokio::spawn`ed alongside the proxy.
    pub async fn run(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(self.tick_interval);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                error!(service = %self.service.name, error = %e, "autoscaler tick failed");
            }
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let backends = self.pool.get().await;
        let current = backends.len() as u32;

        let now_minutes = local_now_minutes();
        let floor = self
            .scaling
            .schedule
            .as_ref()
            .and_then(|s| current_scheduled_floor(s, now_minutes))
            .unwrap_or(self.scaling.min_instances);

        let metric_value = self.metrics.value_for(self.scaling.metric);

        let cooldown_elapsed = {
            let last = *self.last_scale.lock().await;
            last.elapsed() >= Duration::from_secs(self.scaling.cooldown_secs)
        };

        // Catching up to a schedule floor always bypasses the cooldown —
        // if admins pre-warm for 8:55 and we're only realizing it at 8:56
        // because of a recent reactive scale-up, waiting out the cooldown
        // defeats the point of scheduling in the first place.
        let action = if current < floor.max(self.scaling.min_instances) {
            ScaleAction::Up
        } else if !cooldown_elapsed {
            ScaleAction::Hold
        } else {
            decide_scale_action(current, metric_value, floor, &self.scaling)
        };

        match action {
            ScaleAction::Up => self.scale_up().await,
            ScaleAction::Down => self.scale_down(&backends).await,
            ScaleAction::Hold => Ok(()),
        }
    }

    async fn scale_up(&self) -> anyhow::Result<()> {
        let instance = self.driver.spawn_instance(&self.service).await?;
        self.metrics.track(&instance.container_id);

        // Give it a short window to become healthy, but don't block the
        // whole control loop indefinitely on one slow-starting container.
        let mut healthy = false;
        for _ in 0..10 {
            if self
                .driver
                .health_check(&instance, &self.service.health_check)
                .await
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !healthy {
            warn!(
                container_id = %instance.container_id,
                "new instance not healthy after 5s, adding to pool anyway"
            );
        }

        let mut current = self.pool.get().await;
        current.push(instance);
        let new_count = current.len();
        self.pool.set(current).await;
        *self.last_scale.lock().await = Instant::now();

        info!(service = %self.service.name, replicas = new_count, "scaled up");
        Ok(())
    }

    async fn scale_down(&self, backends: &[Instance]) -> anyhow::Result<()> {
        // Stop whichever instance is currently handling the fewest active
        // requests, so we're not more likely to interrupt someone mid
        // exam-card download than a strict "oldest" or "first" policy would be.
        let Some(victim) = backends
            .iter()
            .min_by_key(|b| self.metrics.active_for(&b.container_id))
            .cloned()
        else {
            return Ok(());
        };

        self.driver.stop_instance(&victim).await?;
        self.metrics.untrack(&victim.container_id);

        let remaining: Vec<Instance> = backends
            .iter()
            .filter(|b| b.container_id != victim.container_id)
            .cloned()
            .collect();
        let new_count = remaining.len();
        self.pool.set(remaining).await;
        *self.last_scale.lock().await = Instant::now();

        info!(service = %self.service.name, replicas = new_count, "scaled down");
        Ok(())
    }
}

fn local_now_minutes() -> u32 {
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}
