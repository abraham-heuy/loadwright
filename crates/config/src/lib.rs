//! Parses a loadwright service definition (TOML) into strongly typed structs.
//!
//! Example file: see `examples/exam-portal.toml` in the repo root.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub service: ServiceMeta,
    #[serde(default)]
    pub resources: Resources,
    pub scaling: ScalingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceMeta {
    /// Human-readable name, also used as the docker container name prefix.
    pub name: String,
    /// Fully qualified container image, e.g. "ghcr.io/org/app:latest".
    pub image: String,
    /// Port the app listens on *inside* the container.
    pub port: u16,
    /// HTTP path used for health checks, e.g. "/healthz".
    pub health_check: String,
    /// Port the proxy listens on for this service (public facing).
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
}

fn default_listen_port() -> u16 {
    8080
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Resources {
    /// Fractional CPUs, e.g. "0.5" -> 50% of one core.
    pub cpu_limit: Option<String>,
    /// e.g. "512m", "1g".
    pub memory_limit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScalingConfig {
    pub min_instances: u32,
    pub max_instances: u32,
    pub metric: ScalingMetric,
    pub scale_up_threshold: f64,
    pub scale_down_threshold: f64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub schedule: Option<ScheduleConfig>,
}

fn default_cooldown() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScalingMetric {
    P95LatencyMs,
    Cpu,
    ActiveConnections,
    RequestsPerSecond,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    pub entries: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleEntry {
    /// 24h "HH:MM", evaluated against local server time.
    pub at: String,
    pub instances: u32,
}

impl ServiceConfig {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::from_str(&raw)
    }

    pub fn from_str(raw: &str) -> Result<Self> {
        let cfg: ServiceConfig = toml::from_str(raw).context("parsing service TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.scaling.min_instances >= 1,
            "min_instances must be >= 1 (need at least one replica to serve traffic)"
        );
        anyhow::ensure!(
            self.scaling.max_instances >= self.scaling.min_instances,
            "max_instances must be >= min_instances"
        );
        anyhow::ensure!(
            self.scaling.scale_up_threshold > self.scaling.scale_down_threshold,
            "scale_up_threshold must be greater than scale_down_threshold"
        );
        if let Some(sched) = &self.scaling.schedule {
            for entry in &sched.entries {
                anyhow::ensure!(
                    parse_hhmm(&entry.at).is_some(),
                    "invalid schedule time '{}', expected HH:MM",
                    entry.at
                );
            }
        }
        Ok(())
    }
}

/// Parses "HH:MM" into minutes-since-midnight. Returns None if malformed.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// Given a schedule and the current time (minutes since midnight, local
/// server time), returns the instance floor that should currently be in
/// effect — or `None` if the service has no schedule at all.
///
/// Semantics: a schedule entry is a *standing floor*, not a one-shot
/// trigger. The entry that most recently took effect (the latest `at` that
/// is `<= now`) wins. If `now` is earlier than every entry today, the
/// *last* entry in the list still applies, on the assumption it took effect
/// yesterday and hasn't been superseded yet — e.g. a `{ at: "10:30",
/// instances: 1 }` scale-down entry should still hold at 3am, not silently
/// reset to `min_instances` overnight.
pub fn current_scheduled_floor(schedule: &ScheduleConfig, now_minutes: u32) -> Option<u32> {
    if schedule.entries.is_empty() {
        return None;
    }
    let mut sorted: Vec<&ScheduleEntry> = schedule.entries.iter().collect();
    sorted.sort_by_key(|e| parse_hhmm(&e.at).unwrap_or(0));

    // Default: the last entry of the day, standing in for "still in effect
    // from before midnight" if nothing today has fired yet.
    let mut floor = sorted.last().map(|e| e.instances);
    for entry in &sorted {
        let at = parse_hhmm(&entry.at).unwrap_or(0);
        if at <= now_minutes {
            floor = Some(entry.instances);
        }
    }
    floor
}

/// What the autoscaler should do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleAction {
    Up,
    Down,
    Hold,
}

/// The core scaling decision, kept as a pure function so it's testable
/// without touching Docker, the proxy, or a clock. Callers are responsible
/// for cooldown enforcement (i.e. don't call this more often than
/// `cooldown_secs` and expect it to self-throttle) — this function only
/// answers "given these numbers right now, what should happen?"
///
/// `floor` is whatever `current_scheduled_floor` returned this tick (or
/// `scaling.min_instances` if there's no schedule); catching up to the
/// floor always takes priority over the reactive metric comparison.
pub fn decide_scale_action(
    current_instances: u32,
    metric_value: Option<f64>,
    floor: u32,
    scaling: &ScalingConfig,
) -> ScaleAction {
    let effective_floor = floor.max(scaling.min_instances);

    if current_instances < effective_floor {
        return ScaleAction::Up;
    }

    let Some(value) = metric_value else {
        // No data yet — e.g. no traffic, or the configured metric isn't
        // implemented (CPU, in v1). Don't guess; hold steady.
        return ScaleAction::Hold;
    };

    if value > scaling.scale_up_threshold && current_instances < scaling.max_instances {
        ScaleAction::Up
    } else if value < scaling.scale_down_threshold && current_instances > effective_floor {
        ScaleAction::Down
    } else {
        ScaleAction::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[service]
name = "exam-portal"
image = "ghcr.io/university/exam-portal:latest"
port = 8080
health_check = "/healthz"

[resources]
cpu_limit = "0.5"
memory_limit = "512m"

[scaling]
min_instances = 1
max_instances = 10
metric = "p95_latency_ms"
scale_up_threshold = 800
scale_down_threshold = 200
cooldown_secs = 60

[scaling.schedule]
entries = [
  { at = "08:55", instances = 6 },
  { at = "10:30", instances = 1 },
]
"#;

    #[test]
    fn parses_sample_config() {
        let cfg = ServiceConfig::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.service.name, "exam-portal");
        assert_eq!(cfg.scaling.metric, ScalingMetric::P95LatencyMs);
        assert_eq!(cfg.scaling.schedule.unwrap().entries.len(), 2);
    }

    #[test]
    fn rejects_bad_thresholds() {
        let bad = SAMPLE.replace("scale_up_threshold = 800", "scale_up_threshold = 100");
        assert!(ServiceConfig::from_str(&bad).is_err());
    }

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("08:55"), Some(8 * 60 + 55));
        assert_eq!(parse_hhmm("23:59"), Some(23 * 60 + 59));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("nonsense"), None);
    }

    fn sample_schedule() -> ScheduleConfig {
        ScheduleConfig {
            entries: vec![
                ScheduleEntry { at: "08:55".into(), instances: 6 },
                ScheduleEntry { at: "10:30".into(), instances: 1 },
            ],
        }
    }

    #[test]
    fn schedule_floor_before_first_entry_uses_last_entry_of_day() {
        // 3am: nothing has fired yet today, so yesterday's 10:30 entry
        // (the last one in the list) is still considered in effect.
        let floor = current_scheduled_floor(&sample_schedule(), 3 * 60);
        assert_eq!(floor, Some(1));
    }

    #[test]
    fn schedule_floor_during_prewarm_window() {
        // 9:10am: past 08:55, before 10:30 -> the pre-warm floor of 6 applies.
        let floor = current_scheduled_floor(&sample_schedule(), 9 * 60 + 10);
        assert_eq!(floor, Some(6));
    }

    #[test]
    fn schedule_floor_after_scale_down_entry() {
        let floor = current_scheduled_floor(&sample_schedule(), 11 * 60);
        assert_eq!(floor, Some(1));
    }

    #[test]
    fn schedule_floor_empty_schedule_is_none() {
        let empty = ScheduleConfig { entries: vec![] };
        assert_eq!(current_scheduled_floor(&empty, 9 * 60), None);
    }

    fn sample_scaling() -> ScalingConfig {
        ScalingConfig {
            min_instances: 1,
            max_instances: 10,
            metric: ScalingMetric::P95LatencyMs,
            scale_up_threshold: 800.0,
            scale_down_threshold: 200.0,
            cooldown_secs: 60,
            schedule: None,
        }
    }

    #[test]
    fn decide_catches_up_to_floor_regardless_of_metric() {
        // Only 1 instance running, but the schedule says we should have 6 —
        // scale up even though latency looks fine.
        let action = decide_scale_action(1, Some(50.0), 6, &sample_scaling());
        assert_eq!(action, ScaleAction::Up);
    }

    #[test]
    fn decide_scales_up_on_high_latency() {
        let action = decide_scale_action(2, Some(900.0), 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Up);
    }

    #[test]
    fn decide_respects_max_instances() {
        let action = decide_scale_action(10, Some(900.0), 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Hold);
    }

    #[test]
    fn decide_scales_down_on_low_latency() {
        let action = decide_scale_action(4, Some(50.0), 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Down);
    }

    #[test]
    fn decide_wont_scale_down_below_floor() {
        let action = decide_scale_action(1, Some(50.0), 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Hold);
    }

    #[test]
    fn decide_holds_with_no_metric_data() {
        let action = decide_scale_action(2, None, 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Hold);
    }

    #[test]
    fn decide_holds_in_dead_zone_between_thresholds() {
        let action = decide_scale_action(3, Some(400.0), 1, &sample_scaling());
        assert_eq!(action, ScaleAction::Hold);
    }
}
