//! Multi-bucket IP strike tracking + ban enforcement.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use config::IpBanConfig;

use crate::real_ip::normalize_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrikeCategory {
    Api,
    AdminAuth,
    CpuAbuse,
    LogicAbuse,
}

impl StrikeCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::AdminAuth => "admin_auth",
            Self::CpuAbuse => "cpu_abuse",
            Self::LogicAbuse => "logic_abuse",
        }
    }
}

struct StrikeEntry {
    window_start: Instant,
    count: u32,
}

struct BanEntry {
    banned_at: Instant,
}

#[derive(Debug, Clone)]
pub struct BanSnapshot {
    pub ip: String,
    pub age_secs: u64,
    pub remaining_secs: u64,
}

#[derive(Debug, Clone)]
pub struct StrikeSnapshot {
    pub ip: String,
    pub category: &'static str,
    pub count: u32,
    pub age_secs: u64,
    pub remaining_window_secs: u64,
}

pub struct IpStrikeTracker {
    config: IpBanConfig,
    strikes: Mutex<HashMap<(String, StrikeCategory), StrikeEntry>>,
    bans: Mutex<HashMap<String, BanEntry>>,
}

impl IpStrikeTracker {
    pub fn new(config: &IpBanConfig) -> Self {
        Self {
            config: *config,
            strikes: Mutex::new(HashMap::new()),
            bans: Mutex::new(HashMap::new()),
        }
    }

    pub fn record_strike(&self, ip: IpAddr, category: StrikeCategory) -> bool {
        let key = normalize_key(ip);
        let window = Duration::from_secs(self.config.strike_window_secs);
        let now = Instant::now();

        let count = {
            let mut strikes = self.strikes.lock().unwrap();
            let entry = strikes.entry((key.clone(), category)).or_insert(StrikeEntry {
                window_start: now,
                count: 0,
            });
            if now.duration_since(entry.window_start) >= window {
                entry.window_start = now;
                entry.count = 0;
            }
            entry.count += 1;
            entry.count
        };

        if count >= self.config.strike_threshold {
            self.bans.lock().unwrap().insert(key, BanEntry { banned_at: now });
            tracing::warn!(ip = %ip, ?category, strikes = count, "IP banned");
            true
        } else {
            false
        }
    }

    pub fn is_banned(&self, ip: IpAddr) -> bool {
        let key = normalize_key(ip);
        let ban_duration = Duration::from_secs(self.config.ban_duration_secs);
        let now = Instant::now();
        let mut bans = self.bans.lock().unwrap();
        match bans.get(&key) {
            Some(entry) if now.duration_since(entry.banned_at) < ban_duration => true,
            Some(_) => {
                bans.remove(&key);
                false
            }
            None => false,
        }
    }

    pub fn ban_snapshots(&self) -> Vec<BanSnapshot> {
        let now = Instant::now();
        let duration = Duration::from_secs(self.config.ban_duration_secs);
        let mut bans = self.bans.lock().unwrap();
        bans.retain(|_, entry| now.duration_since(entry.banned_at) < duration);
        let mut output = bans.iter().map(|(ip, entry)| {
            let age = now.duration_since(entry.banned_at);
            BanSnapshot {
                ip: ip.clone(),
                age_secs: age.as_secs(),
                remaining_secs: duration.saturating_sub(age).as_secs(),
            }
        }).collect::<Vec<_>>();
        output.sort_by(|a, b| a.ip.cmp(&b.ip));
        output
    }

    pub fn strike_snapshots(&self) -> Vec<StrikeSnapshot> {
        let now = Instant::now();
        let window = Duration::from_secs(self.config.strike_window_secs);
        let mut strikes = self.strikes.lock().unwrap();
        strikes.retain(|_, entry| now.duration_since(entry.window_start) < window);
        let mut output = strikes.iter().map(|((ip, category), entry)| {
            let age = now.duration_since(entry.window_start);
            StrikeSnapshot {
                ip: ip.clone(),
                category: category.label(),
                count: entry.count,
                age_secs: age.as_secs(),
                remaining_window_secs: window.saturating_sub(age).as_secs(),
            }
        }).collect::<Vec<_>>();
        output.sort_by(|a, b| a.ip.cmp(&b.ip).then_with(|| a.category.cmp(b.category)));
        output
    }

    pub fn sweep(&self) {
        let now = Instant::now();
        let strike_window = Duration::from_secs(self.config.strike_window_secs);
        let ban_duration = Duration::from_secs(self.config.ban_duration_secs);
        self.strikes.lock().unwrap().retain(|_, e| now.duration_since(e.window_start) < strike_window);
        self.bans.lock().unwrap().retain(|_, e| now.duration_since(e.banned_at) < ban_duration);
    }
}

pub trait HasIpStrikes {
    fn ip_strikes(&self) -> &IpStrikeTracker;
    fn trust_proxy_headers(&self) -> bool;
}

pub async fn ban_check<S>(
    axum::extract::State(state): axum::extract::State<S>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response
where
    S: HasIpStrikes + Clone + Send + Sync + 'static,
{
    use axum::response::IntoResponse;

    let peer = req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    let ip = crate::real_ip::extract_real_ip(req.headers(), peer, state.trust_proxy_headers());

    if state.ip_strikes().is_banned(ip) {
        return (axum::http::StatusCode::FORBIDDEN, "banned").into_response();
    }

    next.run(req).await
}
