//! Per-request timing and verbose structured logging: every request
//! gets its wall-clock duration measured, a single structured
//! `tracing` log line on completion (method, path, status, duration,
//! client IP — everything you'd want when reading logs back later,
//! not just "GET /ping 200"), and a standard `Server-Timing` response
//! header exposing that same duration to whatever's on the other end
//! of the connection. That last part means a plain `curl -v` or a
//! browser's Network tab can see exactly how long a request took
//! without needing server log access — a self-timing `ping` endpoint,
//! for free, on every route, not just `ping`.
//!
//! `Server-Timing` is a real, standard HTTP response header
//! (https://developer.mozilla.org/docs/Web/HTTP/Headers/Server-Timing)
//! purpose-built for exactly this — not a made-up header name.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use config::Config;

use crate::real_ip::extract_real_ip;

pub async fn request_timing(State(config): State<Arc<Config>>, request: Request, next: Next) -> Response {
    let started_at = Instant::now();

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let client_ip = extract_real_ip(request.headers(), peer, config.security.trusted_proxy_headers);

    let mut response = next.run(request).await;

    let elapsed = started_at.elapsed();
    let duration_ms = elapsed.as_secs_f64() * 1000.0;
    let status = response.status().as_u16();

    // One structured line per request — deliberately a single line
    // (not one per middleware layer) so grepping/filtering logs stays
    // easy; every field a person would actually want when reading logs
    // back after the fact is right here rather than scattered across
    // several partial lines from different layers.
    tracing::info!(
        method = %method,
        path = %path,
        query = %query,
        status = status,
        duration_ms = format_args!("{duration_ms:.3}"),
        client_ip = %client_ip,
        "request completed"
    );

    if let Ok(value) = HeaderValue::from_str(&format!("total;dur={duration_ms:.3}")) {
        response.headers_mut().insert("server-timing", value);
    }

    response
}
