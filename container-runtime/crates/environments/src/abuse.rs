//! Abuse detection: "someone spamming a button that sends data and
//! causing too much CPU/network usage" — tracked per caller, per
//! environment, across four independent windowed counters (request
//! rate, CPU time, network bytes, disk bytes). Crossing any one
//! threshold flags abuse; it's an OR, not requiring all four at once,
//! since any single dimension maxed out is already a real problem.
//!
//! **Scoping note, honestly stated:** this module tracks and reacts to
//! resource usage that's *reported to it* — it does not itself measure
//! OS-level CPU/network consumption. Real measurement is
//! `sandbox-primitives`/`resource-limits`'s job (still stubbed, not
//! part of this change) — once those exist, they'd call
//! `record_execution` with real numbers. Until then, this is the
//! policy/detection layer, tested and ready, waiting on the
//! measurement layer underneath it.
//!
//! Not reusing the engine's `security::WindowedCounter` here on
//! purpose: pulling engine's `security` crate (which depends on
//! `axum`/`tower`) into the container runtime just for one small
//! counter utility would be the wrong kind of coupling for a process
//! meant to stay minimal and independently auditable. Small enough to
//! duplicate correctly rather than share incorrectly.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct AbuseThresholds {
    pub window: Duration,
    pub max_requests_per_window: u32,
    pub max_cpu_ms_per_window: u64,
    pub max_network_bytes_per_window: u64,
    pub max_disk_bytes_per_window: u64,
}

impl Default for AbuseThresholds {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_requests_per_window: 120,
            max_cpu_ms_per_window: 30_000,   // 30s of CPU time per minute
            max_network_bytes_per_window: 50 * 1024 * 1024, // 50MB/min
            max_disk_bytes_per_window: 20 * 1024 * 1024,    // 20MB/min
        }
    }
}

/// Payment gets tighter defaults than general environments — smaller,
/// more predictable payloads are expected there, so a caller pushing
/// payment-environment usage toward general-environment volume is
/// itself a signal worth reacting to sooner.
impl AbuseThresholds {
    pub fn payment_defaults() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_requests_per_window: 30,
            max_cpu_ms_per_window: 10_000,
            max_network_bytes_per_window: 5 * 1024 * 1024,
            max_disk_bytes_per_window: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbuseVerdict {
    Allowed,
    /// Which dimension tripped, for logging/metrics — doesn't change
    /// the caller-facing behavior (still rejected either way), just
    /// makes "why was this blocked" answerable without guessing.
    Blocked(AbuseDimension),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbuseDimension {
    RequestRate,
    CpuTime,
    NetworkBytes,
    DiskBytes,
}

#[derive(Default, Clone, Copy)]
struct Usage {
    requests: u32,
    cpu_ms: u64,
    network_bytes: u64,
    disk_bytes: u64,
}

struct WindowEntry {
    window_start: Instant,
    usage: Usage,
}

pub struct AbuseDetector {
    thresholds: AbuseThresholds,
    per_caller: Mutex<HashMap<String, WindowEntry>>,
}

impl AbuseDetector {
    pub fn new(thresholds: AbuseThresholds) -> Self {
        Self {
            thresholds,
            per_caller: Mutex::new(HashMap::new()),
        }
    }

    /// Records one execution's reported resource usage for `caller`
    /// and returns whether it (cumulatively, within the current
    /// window) crosses any threshold. Call this AFTER the execution
    /// completes and its real usage is known — this is a detection
    /// mechanism (react to what happened), not a pre-execution
    /// admission check (that would need estimated/worst-case usage,
    /// which isn't available here).
    pub fn record_execution(
        &self,
        caller: &str,
        cpu_ms: u64,
        network_bytes: u64,
        disk_bytes: u64,
    ) -> AbuseVerdict {
        let now = Instant::now();
        let mut callers = self.per_caller.lock().unwrap();

        let entry = callers.entry(caller.to_string()).or_insert_with(|| WindowEntry {
            window_start: now,
            usage: Usage::default(),
        });

        if now.duration_since(entry.window_start) >= self.thresholds.window {
            entry.window_start = now;
            entry.usage = Usage::default();
        }

        entry.usage.requests = entry.usage.requests.saturating_add(1);
        entry.usage.cpu_ms = entry.usage.cpu_ms.saturating_add(cpu_ms);
        entry.usage.network_bytes = entry.usage.network_bytes.saturating_add(network_bytes);
        entry.usage.disk_bytes = entry.usage.disk_bytes.saturating_add(disk_bytes);

        if entry.usage.requests > self.thresholds.max_requests_per_window {
            return AbuseVerdict::Blocked(AbuseDimension::RequestRate);
        }
        if entry.usage.cpu_ms > self.thresholds.max_cpu_ms_per_window {
            return AbuseVerdict::Blocked(AbuseDimension::CpuTime);
        }
        if entry.usage.network_bytes > self.thresholds.max_network_bytes_per_window {
            return AbuseVerdict::Blocked(AbuseDimension::NetworkBytes);
        }
        if entry.usage.disk_bytes > self.thresholds.max_disk_bytes_per_window {
            return AbuseVerdict::Blocked(AbuseDimension::DiskBytes);
        }

        AbuseVerdict::Allowed
    }

    /// Opportunistic cleanup — same pattern as everywhere else this
    /// kind of windowed map shows up in this codebase.
    pub fn sweep(&self) {
        let now = Instant::now();
        let max_age = self.thresholds.window * 2;
        self.per_caller
            .lock()
            .unwrap()
            .retain(|_, entry| now.duration_since(entry.window_start) < max_age);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_usage_under_every_threshold() {
        let detector = AbuseDetector::new(AbuseThresholds::default());
        let verdict = detector.record_execution("caller-a", 10, 1000, 100);
        assert_eq!(verdict, AbuseVerdict::Allowed);
    }

    #[test]
    fn blocks_on_request_rate() {
        let thresholds = AbuseThresholds {
            max_requests_per_window: 3,
            ..AbuseThresholds::default()
        };
        let detector = AbuseDetector::new(thresholds);
        for _ in 0..3 {
            assert_eq!(
                detector.record_execution("spammer", 1, 1, 1),
                AbuseVerdict::Allowed
            );
        }
        assert_eq!(
            detector.record_execution("spammer", 1, 1, 1),
            AbuseVerdict::Blocked(AbuseDimension::RequestRate)
        );
    }

    #[test]
    fn blocks_on_cpu_time_even_under_request_rate_limit() {
        let thresholds = AbuseThresholds {
            max_requests_per_window: 1000,
            max_cpu_ms_per_window: 500,
            ..AbuseThresholds::default()
        };
        let detector = AbuseDetector::new(thresholds);
        let verdict = detector.record_execution("heavy-caller", 600, 1, 1);
        assert_eq!(verdict, AbuseVerdict::Blocked(AbuseDimension::CpuTime));
    }

    #[test]
    fn cumulative_usage_saturates_instead_of_wrapping() {
        let thresholds = AbuseThresholds {
            max_requests_per_window: u32::MAX,
            max_cpu_ms_per_window: u64::MAX - 1,
            max_network_bytes_per_window: u64::MAX,
            max_disk_bytes_per_window: u64::MAX,
            ..AbuseThresholds::default()
        };
        let detector = AbuseDetector::new(thresholds);
        assert_eq!(
            detector.record_execution("overflow", u64::MAX - 1, 0, 0),
            AbuseVerdict::Allowed
        );
        assert_eq!(
            detector.record_execution("overflow", 1, 0, 0),
            AbuseVerdict::Blocked(AbuseDimension::CpuTime)
        );
        assert_eq!(
            detector.record_execution("overflow", 1, 0, 0),
            AbuseVerdict::Blocked(AbuseDimension::CpuTime)
        );
    }

    #[test]
    fn different_callers_are_tracked_independently() {
        let thresholds = AbuseThresholds {
            max_requests_per_window: 1,
            ..AbuseThresholds::default()
        };
        let detector = AbuseDetector::new(thresholds);
        assert_eq!(
            detector.record_execution("a", 1, 1, 1),
            AbuseVerdict::Allowed
        );
        assert_eq!(
            detector.record_execution("b", 1, 1, 1),
            AbuseVerdict::Allowed,
            "a different caller should not be affected by a's usage"
        );
    }
}
