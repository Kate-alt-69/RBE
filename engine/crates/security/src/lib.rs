//! `tower`/`axum` middleware — the request-lifecycle "layers" from
//! migration-plan §4 (Request Lifecycle row) and §9.1.

mod counter;
mod headers;
mod ip_strikes;
mod rate_limit;
mod real_ip;
mod timing;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

pub use headers::security_headers;
pub use ip_strikes::{ban_check, BanSnapshot, HasIpStrikes, IpStrikeTracker, StrikeCategory, StrikeSnapshot};
pub use rate_limit::{api_rate_limit, global_rate_limit, HasRateLimiters, RateLimiters};
pub use real_ip::{extract_real_ip, normalize_key};
pub use timing::request_timing;

pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

pub async fn correlation_id(mut request: Request, next: Next) -> Response {
    let incoming = request
        .headers()
        .get(CORRELATION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let correlation_id = incoming.unwrap_or_else(|| Uuid::new_v4().to_string());
    tracing::Span::current().record("correlation_id", tracing::field::display(&correlation_id));
    request.extensions_mut().insert(CorrelationId(correlation_id.clone()));

    let mut response = next.run(request).await;
    if let Ok(value) = correlation_id.parse::<axum::http::HeaderValue>() {
        response.headers_mut().insert(CORRELATION_ID_HEADER, value);
    }
    response
}

#[derive(Debug, Clone)]
pub struct CorrelationId(pub String);
