use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde_json::{json, Value};
use supervisor::BackendState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let lifecycle = state.backend_state();
    let services = state.services.snapshot().await;
    let running_services = services.iter().filter(|service| service.pid.is_some()).count();
    let services_ok = running_services == services.len();

    let (video_ok, video_status) = match state.video_manager.as_ref() {
        Some(manager) => match manager.status() {
            Ok(status) => (status.ok, json!(status)),
            Err(error) => (
                false,
                json!({
                    "ok": false,
                    "error": error.to_string()
                }),
            ),
        },
        None => (true, json!({ "enabled": false })),
    };

    let lifecycle_ok = matches!(lifecycle, BackendState::Ready | BackendState::Running);
    Json(json!({
        "ok": lifecycle_ok && services_ok && video_ok,
        "state": lifecycle,
        "services": {
            "ok": services_ok,
            "running": running_services,
            "total": services.len(),
            "entries": services
        },
        "videoManager": video_status
    }))
}
