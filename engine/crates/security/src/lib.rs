//! `tower`/`axum` middleware — the request-lifecycle "layers" from
//! migration-plan §4 (Request Lifecycle row) and §9.1.
//!
//! What's here now: correlation IDs, CORS building blocks, security
//! headers (Helmet/CSP equivalent), two-tier rate limiting, multi-
//! bucket IP strikes/bans, and real-IP extraction across common
//! CDN/proxy providers. Still not here (need Phase 1's vault/storage
//! first): permission-manager rulesets, the SDK handshake protocol,
//! cryptographic request signing, geo-filtering, and the admin-specific
//! strike system (v1's IP strikes are a single unified tracker with a
//! `StrikeCategory` tag, not a separate system).

mod counter;
mod headers;
mod ip_strikes;
mod rate_limit;
mod real_ip;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

pub use headers::security_headers;
pub use ip_strikes::{ban_check, HasIpStrikes, IpStrikeTracker, StrikeCategory};
pub use rate_limit::{api_rate_limit, global_rate_limit, HasRateLimiters, RateLimiters};
pub use real_ip::{extract_real_ip, normalize_key};

/// Request header/extension key used to carry the correlation ID.
pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

/// Generates (or forwards, if the caller already sent one) a correlation
/// ID and attaches it to the current `tracing` span as a field, so every
/// log line emitted while handling this request — across every layer
/// and handler downstream — carries it automatically. This is the
/// concrete implementation of migration-plan §4's Logging Architecture
/// row: "correlation IDs as a `tracing::Span` field set once per request
/// and inherited by every log line inside that request automatically".
///
/// Register with:
/// ```ignore
/// Router::new().layer(axum::middleware::from_fn(security::correlation_id));
/// ```
pub async fn correlation_id(mut request: Request, next: Next) -> Response {
    let incoming = request
        .headers()
        .get(CORRELATION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let correlation_id = incoming.unwrap_or_else(|| Uuid::new_v4().to_string());

    // Record on the current span (created by `tower_http::trace::TraceLayer`
    // or an outer `tracing` span in `main.rs`) rather than creating a new
    // one here, so this stays composable with whatever span-creation layer
    // sits above it in the stack.
    tracing::Span::current().record("correlation_id", tracing::field::display(&correlation_id));

    request
        .extensions_mut()
        .insert(CorrelationId(correlation_id.clone()));

    let mut response = next.run(request).await;

    if let Ok(value) = correlation_id.parse::<axum::http::HeaderValue>() {
        response.headers_mut().insert(CORRELATION_ID_HEADER, value);
    }

    response
}

/// Extractor-friendly wrapper so handlers can pull the correlation ID
/// out of request extensions if they need to log it explicitly (usually
/// unnecessary — it's already on the span — but useful for e.g.
/// including it in an error response body).
#[derive(Debug, Clone)]
pub struct CorrelationId(pub String);
