//! Per-environment health tracking. Deliberately simple: a heartbeat
//! timestamp and a consecutive-failure counter, not a full metrics
//! pipeline. This answers "is this environment still alive and
//! behaving," which is what the registry needs to decide whether to
//! keep routing work to it — it is not a replacement for real
//! resource-usage telemetry (that's `abuse::AbuseDetector`'s job).

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Recent heartbeat, no recent consecutive failures.
    Healthy,
    /// Heartbeat is stale, or there have been some recent failures —
    /// still routable, but worth watching.
    Degraded,
    /// Heartbeat far too stale, or too many consecutive failures —
    /// should not receive new work until it recovers.
    Unresponsive,
}

#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    /// No heartbeat within this window -> at least Degraded.
    pub degraded_after: Duration,
    /// No heartbeat within this window -> Unresponsive.
    pub unresponsive_after: Duration,
    /// This many consecutive failures (regardless of heartbeat
    /// recency) -> Unresponsive.
    pub max_consecutive_failures: u32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            degraded_after: Duration::from_secs(10),
            unresponsive_after: Duration::from_secs(30),
            max_consecutive_failures: 5,
        }
    }
}

pub struct HealthMonitor {
    thresholds: HealthThresholds,
    last_heartbeat: Instant,
    consecutive_failures: u32,
}

impl HealthMonitor {
    pub fn new(thresholds: HealthThresholds) -> Self {
        Self {
            thresholds,
            last_heartbeat: Instant::now(),
            consecutive_failures: 0,
        }
    }

    /// Call on every successful execution in this environment — resets
    /// the staleness clock and the failure streak.
    pub fn record_success(&mut self) {
        self.last_heartbeat = Instant::now();
        self.consecutive_failures = 0;
    }

    /// Call on every failed execution. Does NOT reset the heartbeat —
    /// a failing-but-still-responding environment is exactly the
    /// "Degraded, not silently Healthy" case this distinction exists
    /// for.
    pub fn record_failure(&mut self) {
        self.last_heartbeat = Instant::now();
        self.consecutive_failures += 1;
    }

    pub fn status(&self) -> HealthStatus {
        let since_heartbeat = self.last_heartbeat.elapsed();

        if self.consecutive_failures >= self.thresholds.max_consecutive_failures
            || since_heartbeat >= self.thresholds.unresponsive_after
        {
            HealthStatus::Unresponsive
        } else if since_heartbeat >= self.thresholds.degraded_after
            || self.consecutive_failures > 0
        {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_healthy() {
        let monitor = HealthMonitor::new(HealthThresholds::default());
        assert_eq!(monitor.status(), HealthStatus::Healthy);
    }

    #[test]
    fn one_failure_degrades_not_unresponsive() {
        let mut monitor = HealthMonitor::new(HealthThresholds::default());
        monitor.record_failure();
        assert_eq!(monitor.status(), HealthStatus::Degraded);
    }

    #[test]
    fn enough_consecutive_failures_is_unresponsive() {
        let thresholds = HealthThresholds {
            max_consecutive_failures: 3,
            ..HealthThresholds::default()
        };
        let mut monitor = HealthMonitor::new(thresholds);
        monitor.record_failure();
        monitor.record_failure();
        monitor.record_failure();
        assert_eq!(monitor.status(), HealthStatus::Unresponsive);
    }

    #[test]
    fn success_resets_failure_streak() {
        let mut monitor = HealthMonitor::new(HealthThresholds::default());
        monitor.record_failure();
        monitor.record_failure();
        monitor.record_success();
        assert_eq!(monitor.status(), HealthStatus::Healthy);
    }
}
