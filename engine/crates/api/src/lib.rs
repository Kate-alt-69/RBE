//! Route registration for the Rust backend and its backend-owned control room.

mod dashboard;
mod health;
mod routes;

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use core_lib::AppState;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub fn build_router(
    state: AppState,
    _api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
    runtime_image: Arc<route_engine::RuntimeImageSlot>,
) -> anyhow::Result<Router> {
    let cors = build_cors_layer(&state);
    let image = runtime_image.snapshot();
    let dot_route_routes =
        route_engine::build_routes_from_image(image.as_ref(), service_interfaces)?;

    // Backend metrics and request timing are outermost so rejected requests are
    // visible too, not just requests that made it through the security stack.
    let middleware = ServiceBuilder::new()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_metrics,
        ))
        .layer(axum::middleware::from_fn_with_state(
            runtime_image.clone(),
            server_status_gate,
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
        .layer(cors)
        .layer(axum::middleware::from_fn_with_state(
            Duration::from_millis(state.config.api.request_timeout_ms),
            request_timeout,
        ));

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
        start_dashboard_server(state.clone())?;
        let prefix = state.config.dashboards.admin_path_prefix.clone();
        // Keep the API-port URL useful for auto-open/bookmarks, but do not
        // serve the control room there. It only redirects to the isolated
        // loopback dashboard listener on 127.0.0.1:5799.
        router = router.nest(&prefix, dashboard::redirect_routes());
    }

    let compression_enabled = runtime_image
        .snapshot()
        .middleware_plan
        .contains("compression");
    let router = router.layer(middleware);
    let router = if compression_enabled {
        router.layer(CompressionLayer::new())
    } else {
        router
    };
    Ok(router
        .layer(axum::Extension(runtime_image))
        .with_state(state))
}

fn start_dashboard_server(state: AppState) -> anyhow::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], dashboard::DASHBOARD_PORT));
    let listener = StdTcpListener::bind(address)
        .map_err(|error| anyhow::anyhow!("failed to bind RBE dashboard to {address}: {error}"))?;
    listener.set_nonblocking(true).map_err(|error| {
        anyhow::anyhow!("failed to set RBE dashboard listener nonblocking: {error}")
    })?;
    let listener = tokio::net::TcpListener::from_std(listener).map_err(|error| {
        anyhow::anyhow!("failed to register RBE dashboard listener with Tokio: {error}")
    })?;

    let prefix = state.config.dashboards.admin_path_prefix.clone();
    let dashboard_router = Router::new()
        .nest(&prefix, dashboard::routes())
        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            security::security_headers,
        ))
        .with_state(state);

    tracing::info!(%address, path = %prefix, "RBE dashboard listener ready");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(
            listener,
            dashboard_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            tracing::error!(%address, error = %error, "RBE dashboard listener stopped unexpectedly");
        }
    });

    Ok(())
}

async fn server_status_gate(
    State(runtime_image): State<std::sync::Arc<route_engine::RuntimeImageSlot>>,
    request: Request,
    next: Next,
) -> Response {
    let image = runtime_image.snapshot();
    let status = image.server_policy.status;
    let Some(rejection) = status_rejection(status, request.method(), request.uri().path()) else {
        return next.run(request).await;
    };

    let mut response = (
        rejection,
        axum::Json(serde_json::json!({
            "error": "server state rejects this request",
            "serverStatus": status.as_str(),
            "runtimeImage": image.image_id,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        "x-rbe-server-status",
        HeaderValue::from_static(status.as_str()),
    );
    if rejection == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("5"));
    }
    response
}

fn status_rejection(
    status: route_engine::ServerStatus,
    method: &Method,
    path: &str,
) -> Option<StatusCode> {
    if is_control_plane_path(path) {
        return None;
    }
    match status {
        route_engine::ServerStatus::Online => None,
        route_engine::ServerStatus::Readonly
            if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) =>
        {
            None
        }
        route_engine::ServerStatus::Readonly => Some(StatusCode::LOCKED),
        route_engine::ServerStatus::Maintenance
        | route_engine::ServerStatus::Draining
        | route_engine::ServerStatus::Offline => Some(StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn is_control_plane_path(path: &str) -> bool {
    path == "/health"
        || path == "/healthz"
        || path == "/api/admin"
        || path.starts_with("/api/admin/")
        || path == "/api/maintenance"
        || path.starts_with("/api/maintenance/")
}

async fn request_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
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

#[cfg(test)]
mod runtime_status_tests {
    use super::*;

    #[test]
    fn online_allows_normal_requests() {
        assert_eq!(
            status_rejection(
                route_engine::ServerStatus::Online,
                &Method::POST,
                "/api/foo"
            ),
            None
        );
    }

    #[test]
    fn readonly_allows_reads_and_blocks_mutations() {
        assert_eq!(
            status_rejection(
                route_engine::ServerStatus::Readonly,
                &Method::GET,
                "/api/foo"
            ),
            None
        );
        assert_eq!(
            status_rejection(
                route_engine::ServerStatus::Readonly,
                &Method::POST,
                "/api/foo"
            ),
            Some(StatusCode::LOCKED)
        );
    }

    #[test]
    fn maintenance_and_offline_keep_control_plane_reachable() {
        for status in [
            route_engine::ServerStatus::Maintenance,
            route_engine::ServerStatus::Draining,
            route_engine::ServerStatus::Offline,
        ] {
            assert_eq!(
                status_rejection(status, &Method::GET, "/api/foo"),
                Some(StatusCode::SERVICE_UNAVAILABLE)
            );
            assert_eq!(
                status_rejection(status, &Method::POST, "/api/admin/runtime"),
                None
            );
            assert_eq!(status_rejection(status, &Method::GET, "/health"), None);
        }
    }
}
