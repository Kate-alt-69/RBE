//! Two-tier rate limiting, matching the Node backend's split: a broad
//! global net (catches generic spam, skips `/health` and `/api/*`
//! since the API tier handles those specifically) and a tighter
//! API-specific guard. Both use [`crate::counter::WindowedCounter`]
//! keyed by the normalized real IP (§`real_ip`).

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use config::{RateLimitTierConfig, SecurityConfig};

use crate::counter::WindowedCounter;
use crate::real_ip::{extract_real_ip, normalize_key};

pub struct RateLimiters {
    global: WindowedCounter,
    api: WindowedCounter,
    global_config: RateLimitTierConfig,
    api_config: RateLimitTierConfig,
    trust_proxy_headers: bool,
}

impl RateLimiters {
    pub fn new(config: &SecurityConfig) -> Self {
        Self {
            global: WindowedCounter::new(),
            api: WindowedCounter::new(),
            global_config: config.global_rate_limit,
            api_config: config.api_rate_limit,
            trust_proxy_headers: config.trusted_proxy_headers,
        }
    }

    /// Call periodically from a supervised task (§7) — not per-request.
    pub fn sweep(&self) {
        let max_age = Duration::from_secs(
            self.global_config
                .window_secs
                .max(self.api_config.window_secs)
                * 2,
        );
        self.global.sweep(max_age);
        self.api.sweep(max_age);
    }
}

/// Callers must agree on what is actually inside the API namespace.
/// A raw `starts_with("/api")` also matches unrelated paths such as
/// `/apiary` and `/apix`, incorrectly moving them from the global tier
/// into the API-specific tier.
fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Anything implementing this can hand back the shared rate-limiter
/// state — implemented for `core_lib::AppState` so this crate doesn't
/// need a hard dependency on that specific type. (Kept generic rather
/// than importing `core_lib` here to avoid security <-> core_lib
/// depending on each other in both directions — `core_lib` depends on
/// `security`, not the reverse.)
pub trait HasRateLimiters {
    fn rate_limiters(&self) -> &RateLimiters;
}

fn real_ip_from_request(req: &Request, trust_proxy_headers: bool) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let ip = extract_real_ip(req.headers(), peer, trust_proxy_headers);
    normalize_key(ip)
}

fn too_many_requests() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response()
}

pub async fn global_rate_limit<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasRateLimiters + Clone + Send + Sync + 'static,
{
    let path = req.uri().path();
    if path == "/health" || is_api_path(path) {
        return next.run(req).await;
    }

    let limiters = state.rate_limiters();
    let key = real_ip_from_request(&req, limiters.trust_proxy_headers);
    let count = limiters.global.hit(
        &key,
        Duration::from_secs(limiters.global_config.window_secs),
    );

    if count > limiters.global_config.max_requests {
        return too_many_requests();
    }

    next.run(req).await
}

pub async fn api_rate_limit<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasRateLimiters + Clone + Send + Sync + 'static,
{
    let path = req.uri().path();
    if !is_api_path(path) {
        return next.run(req).await;
    }

    let limiters = state.rate_limiters();
    let key = real_ip_from_request(&req, limiters.trust_proxy_headers);
    let count = limiters
        .api
        .hit(&key, Duration::from_secs(limiters.api_config.window_secs));

    if count > limiters.api_config.max_requests {
        return too_many_requests();
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::is_api_path;

    #[test]
    fn api_path_matching_respects_segment_boundaries() {
        assert!(is_api_path("/api"));
        assert!(is_api_path("/api/"));
        assert!(is_api_path("/api/users"));

        assert!(!is_api_path("/"));
        assert!(!is_api_path("/apiary"));
        assert!(!is_api_path("/apix"));
        assert!(!is_api_path("/v1/api"));
    }
}
