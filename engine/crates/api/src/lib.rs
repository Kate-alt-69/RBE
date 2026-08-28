//! Route registration for the Rust backend and its backend-owned control room.

mod dashboard;
mod health;
mod routes;

use std::path::Path;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use core_lib::AppState;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub fn build_router(
    state: AppState,
    api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
) -> anyhow::Result<Router> {
    let cors = build_cors_layer(&state);
    let dot_route_routes = route_engine::build_routes(api_dir, service_interfaces)?;

    // Backend metrics and request timing are outermost so rejected requests are
    // visible too, not just requests that made it through the security stack.
    let middleware = ServiceBuilder::new()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_metrics,
        ))
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

    let mut router = Router::new()
        .merge(health::routes())
        .merge(dot_route_routes)
        .nest("/api/account", routes::account::routes())
        .nest("/api/auth", routes::auth::routes())
        .nest("/api/broadcast", routes::broadcast::routes())
        .nest("/api/contact", routes::contact::routes())
        .nest("/api/streaming", routes::streaming::routes())
        .nest("/api/admin", routes::admin::routes())
        .nest("/api/maintenance", routes::maintenance::routes());

    if state.config.dashboards.enabled {
        router = router.nest(
            &state.config.dashboards.admin_path_prefix,
            dashboard::routes(),
        );
    }

    Ok(router.layer(middleware).with_state(state))
}

async fn request_metrics(State(state): State<AppState>, request: Request, next: Next) -> Response {
    state.backend_metrics.request_started();
    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    state
        .backend_metrics
        .request_finished(response.status().as_u16(), elapsed);
    response
}

fn build_cors_layer(state: &AppState) -> CorsLayer {
    use axum::http::HeaderValue;

    let mut configured_origins = state.config.security.cors_allowed_origins.clone();
    if state.config.runtime.environment != "production" {
        configured_origins.extend(state.config.security.debug_cors_origins.iter().cloned());
    }
    let origins: Vec<HeaderValue> = configured_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    if origins.is_empty() {
        CorsLayer::new()
    } else {
        CorsLayer::new().allow_origin(origins)
    }
}
