use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde_json::{json, Value};
use service_runtime::ServiceRuntimeState;
use supervisor::BackendState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let lifecycle = state.backend_state();
    let services = state.services.snapshot().await;
    let running_services = services
        .iter()
        .filter(|service| service.state == ServiceRuntimeState::Running)
        .count();
    let dormant_services = services
        .iter()
        .filter(|service| service.state == ServiceRuntimeState::Dormant)
        .count();
    let restarting_services = services
        .iter()
        .filter(|service| service.state == ServiceRuntimeState::Restarting)
        .count();
    let stopped_services = services
        .iter()
        .filter(|service| service.state == ServiceRuntimeState::Stopped)
        .count();
    let unknown_services = services
        .iter()
        .filter(|service| service.state == ServiceRuntimeState::Unknown)
        .count();
    let ready_services = services.iter().filter(|service| service.ready).count();
    let unhealthy_services = services
        .iter()
        .filter(|service| service.health_checked && !service.ready)
        .count();
    let services_ok = ready_services == services.len();

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
    "ready": ready_services,
    "running": running_services,
    "dormant": dormant_services,
    "unhealthy": unhealthy_services,
    "restarting": restarting_services,
    "stopped": stopped_services,
    "unknown": unknown_services,
    "total": services.len(),
    "entries": services
          },
          "videoManager": video_status
      }))
}
