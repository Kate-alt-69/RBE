//! Rust equivalent of the Node backend's `helmet` middleware: a small,
//! fixed set of response security headers. `helmet` itself is a big
//! grab-bag of defenses for a Node/Express-shaped threat model; this
//! is the subset that's still meaningful for a JSON API backend.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use config::Config;
use std::sync::Arc;

/// Requires the app's `Config` (for `security.csp_policy`) via
/// `axum::middleware::from_fn_with_state`. `S` just needs to produce a
/// `Config` reference — see `api::build_router` for how this is wired
/// against `AppState`.
pub async fn security_headers(
    State(config): State<Arc<Config>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    if let Ok(csp) = config.security.csp_policy.parse() {
        headers.insert(axum::http::header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-frame-options"),
        axum::http::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );

    response
}
