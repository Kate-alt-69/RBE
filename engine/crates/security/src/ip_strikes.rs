//! Multi-bucket IP strike tracking + ban enforcement. Matches the Node
//! backend's distinction between abuse categories (API abuse, admin
//! auth failures, CPU-cost abuse, logic/state-machine abuse) — each
//! category accumulates strikes independently, but **v1 applies one
//! configured threshold/window/ban-duration uniformly across all of
//! them** (`config.security.ip_ban`), not per-category tuning. Real
//! CPU-abuse and logic-abuse detection (actually measuring request
//! cost or bypass attempts, not just counting a category label) is a
//! separate, larger piece of work — not implemented; callers can
//! record strikes in those categories today, but nothing yet decides
//! *when* a request counts as CPU/logic abuse in the first place.

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

struct StrikeEntry {
    window_start: Instant,
    count: u32,
}

struct BanEntry {
    banned_at: Instant,
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

    /// Records a strike for `ip` in `category`. If it crosses the
    /// threshold within the configured window, bans the IP (across ALL
    /// categories — a ban is a ban, not scoped back down to the
    /// category that triggered it) and returns `true`.
    pub fn record_strike(&self, ip: IpAddr, category: StrikeCategory) -> bool {
        let key = normalize_key(ip);
        let window = Duration::from_secs(self.config.strike_window_secs);
        let now = Instant::now();

        let count = {
            let mut strikes = self.strikes.lock().unwrap();
            let entry = strikes
                .entry((key.clone(), category))
                .or_insert(StrikeEntry {
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
            self.bans
                .lock()
                .unwrap()
                .insert(key, BanEntry { banned_at: now });
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
                // Ban expired — clean it up so `bans` doesn't grow
                // unbounded with stale entries.
                bans.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Opportunistic cleanup — same reasoning as `WindowedCounter::sweep`.
    pub fn sweep(&self) {
        let now = Instant::now();
        let strike_window = Duration::from_secs(self.config.strike_window_secs);
        let ban_duration = Duration::from_secs(self.config.ban_duration_secs);

        self.strikes
            .lock()
            .unwrap()
            .retain(|_, e| now.duration_since(e.window_start) < strike_window);
        self.bans
            .lock()
            .unwrap()
            .retain(|_, e| now.duration_since(e.banned_at) < ban_duration);
    }
}

/// Same pattern as `rate_limit::HasRateLimiters` — implemented for
/// `core_lib::AppState`, kept generic here so `security` doesn't need
/// to depend back on `core_lib`.
pub trait HasIpStrikes {
    fn ip_strikes(&self) -> &IpStrikeTracker;
    fn trust_proxy_headers(&self) -> bool;
}

/// Checks ban status before anything else runs — register this
/// **outermost** in the layer stack (even before correlation ID),
/// same reasoning as the Node backend's rate limiter escalating to an
/// outright 403 rather than paying the cost of running the rest of
/// the pipeline for a request from a banned IP.
pub async fn ban_check<S>(
    axum::extract::State(state): axum::extract::State<S>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response
where
    S: HasIpStrikes + Clone + Send + Sync + 'static,
{
    use axum::response::IntoResponse;

    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    let ip = crate::real_ip::extract_real_ip(req.headers(), peer, state.trust_proxy_headers());

    if state.ip_strikes().is_banned(ip) {
        return (axum::http::StatusCode::FORBIDDEN, "banned").into_response();
    }

    next.run(req).await
}
