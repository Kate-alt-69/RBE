use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct BackendMetricsSnapshot {
    pub uptime_secs: u64,
    pub total_requests: u64,
    pub active_requests: u64,
    pub responses_2xx: u64,
    pub responses_3xx: u64,
    pub responses_4xx: u64,
    pub responses_5xx: u64,
    pub average_latency_ms: f64,
}

pub struct BackendMetrics {
    started: Instant,
    total_requests: AtomicU64,
    active_requests: AtomicU64,
    responses_2xx: AtomicU64,
    responses_3xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    completed_requests: AtomicU64,
    total_latency_micros: AtomicU64,
}

impl Default for BackendMetrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            total_requests: AtomicU64::new(0),
            active_requests: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_3xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
            completed_requests: AtomicU64::new(0),
            total_latency_micros: AtomicU64::new(0),
        }
    }
}

impl BackendMetrics {
    pub fn request_started(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.active_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request_finished(&self, status: u16, elapsed_micros: u64) {
        self.active_requests.fetch_sub(1, Ordering::Relaxed);
        self.completed_requests.fetch_add(1, Ordering::Relaxed);
        self.total_latency_micros.fetch_add(elapsed_micros, Ordering::Relaxed);
        match status / 100 {
            2 => { self.responses_2xx.fetch_add(1, Ordering::Relaxed); }
            3 => { self.responses_3xx.fetch_add(1, Ordering::Relaxed); }
            4 => { self.responses_4xx.fetch_add(1, Ordering::Relaxed); }
            5 => { self.responses_5xx.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> BackendMetricsSnapshot {
        let completed = self.completed_requests.load(Ordering::Relaxed);
        let latency = self.total_latency_micros.load(Ordering::Relaxed);
        BackendMetricsSnapshot {
            uptime_secs: self.started.elapsed().as_secs(),
            total_requests: self.total_requests.load(Ordering::Relaxed),
            active_requests: self.active_requests.load(Ordering::Relaxed),
            responses_2xx: self.responses_2xx.load(Ordering::Relaxed),
            responses_3xx: self.responses_3xx.load(Ordering::Relaxed),
            responses_4xx: self.responses_4xx.load(Ordering::Relaxed),
            responses_5xx: self.responses_5xx.load(Ordering::Relaxed),
            average_latency_ms: if completed == 0 { 0.0 } else { latency as f64 / completed as f64 / 1000.0 },
        }
    }
}

#[derive(Debug, Clone)]
pub struct MaintenanceSnapshot {
    pub refresh_interval_hours: u64,
    pub container_refreshes: u64,
    pub vault_refreshes: u64,
    pub error_reporter_refreshes: u64,
    pub last_container_refresh_ms: u64,
    pub last_vault_refresh_ms: u64,
    pub last_error_reporter_refresh_ms: u64,
}

pub struct MaintenanceMetrics {
    refresh_interval_hours: u64,
    container_refreshes: AtomicU64,
    vault_refreshes: AtomicU64,
    error_reporter_refreshes: AtomicU64,
    last_container_refresh_ms: AtomicU64,
    last_vault_refresh_ms: AtomicU64,
    last_error_reporter_refresh_ms: AtomicU64,
}

impl MaintenanceMetrics {
    pub fn new(refresh_interval_hours: u64) -> Self {
        Self {
            refresh_interval_hours,
            container_refreshes: AtomicU64::new(0),
            vault_refreshes: AtomicU64::new(0),
            error_reporter_refreshes: AtomicU64::new(0),
            last_container_refresh_ms: AtomicU64::new(0),
            last_vault_refresh_ms: AtomicU64::new(0),
            last_error_reporter_refresh_ms: AtomicU64::new(0),
        }
    }

    pub fn record_container_refresh(&self) {
        self.container_refreshes.fetch_add(1, Ordering::Relaxed);
        self.last_container_refresh_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn record_vault_refresh(&self) {
        self.vault_refreshes.fetch_add(1, Ordering::Relaxed);
        self.last_vault_refresh_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn record_error_reporter_refresh(&self) {
        self.error_reporter_refreshes.fetch_add(1, Ordering::Relaxed);
        self.last_error_reporter_refresh_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MaintenanceSnapshot {
        MaintenanceSnapshot {
            refresh_interval_hours: self.refresh_interval_hours,
            container_refreshes: self.container_refreshes.load(Ordering::Relaxed),
            vault_refreshes: self.vault_refreshes.load(Ordering::Relaxed),
            error_reporter_refreshes: self.error_reporter_refreshes.load(Ordering::Relaxed),
            last_container_refresh_ms: self.last_container_refresh_ms.load(Ordering::Relaxed),
            last_vault_refresh_ms: self.last_vault_refresh_ms.load(Ordering::Relaxed),
            last_error_reporter_refresh_ms: self.last_error_reporter_refresh_ms.load(Ordering::Relaxed),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}
