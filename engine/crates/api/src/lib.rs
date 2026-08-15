//! Route registration — migration-plan §4's "Route Indexer" row.
//!
//! Two systems compose here, deliberately:
//! - `routes::*` — hand-written Rust route groups (Rust-native, full
//!   language, for anything performance/security-critical: UAC,
//!   containers, streaming, etc.)
//! - `route_engine::build_routes` — the `.route` file engine: drop a
//!   file in `/api/`, it's discovered, parsed, cached, and served. See
//!   the `route-engine` crate root doc comment for its exact v1 scope.
//!
//! `axum` has no filesystem auto-discovery of its own the way the Node
//! `api/` directory scan did — the `.route` engine *is* the answer to
//! that now, for the subset of routes simple enough to express in it.
//! Anything needing real control flow, direct `AppState` access, or
//! performance-critical code stays a hand-written Rust route group.

mod health;
mod routes;

use std::path::Path;

use axum::Router;
use core_lib::AppState;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Builds the full application router. Called once from `main.rs` after
/// `AppState` is assembled. Fallible: a broken `.route` file should
/// stop the backend from booting (§3.2's fail-fast philosophy), not
/// silently skip that route.
pub fn build_router(state: AppState, api_dir: &Path) -> anyhow::Result<Router> {
    let cors = build_cors_layer(&state);
    let dot_route_routes = route_engine::build_routes(api_dir)?;

    // request_timing is intentionally outermost so even requests rejected
    // by the ban layer are audited in request.queue.log. It still measures
    // the complete request lifecycle, including the security layers and
    // handler below it.
    let middleware = ServiceBuilder::new()
        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            security::request_timing,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::ban_check::<AppState>,
        ))
        .layer(axum::middleware::from_fn(security::correlation_id))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::global_rate_limit::<AppState>,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::api_rate_limit::<AppState>,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.security.max_json_payload_bytes,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            security::security_headers,
        ))
        .layer(cors);

    let router = Router::new()
        .merge(health::routes())
        .merge(dot_route_routes)
        .nest("/api/account", routes::account::routes())
        .nest("/api/auth", routes::auth::routes())
        .nest("/api/broadcast", routes::broadcast::routes())
        .nest("/api/contact", routes::contact::routes())
        .nest("/api/streaming", routes::streaming::routes())
        .nest("/api/admin", routes::admin::routes())
        .nest("/api/maintenance", routes::maintenance::routes())
        .layer(middleware)
        .with_state(state);

    Ok(router)
}

fn build_cors_layer(state: &AppState) -> CorsLayer {
    use axum::http::HeaderValue;

    let mut configured_origins = state.config.security.cors_allowed_origins.clone();
    // Debug origins only apply outside production — see the doc
    // comment on `config::SecurityConfig::debug_cors_origins`.
    if state.config.runtime.environment != "production" {
        configured_origins.extend(state.config.security.debug_cors_origins.iter().cloned());
    }

    let origins: Vec<HeaderValue> = configured_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    if origins.is_empty() {
        // No configured origins: default closed, not default-open.
        // Matches the handbook's "fail securely" principle (§9's
        // Security Philosophy) — an empty allowlist should mean "allow
        // nothing", not "allow everything".
        CorsLayer::new()
    } else {
        CorsLayer::new().allow_origin(origins)
    }
}
