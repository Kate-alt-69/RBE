use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde::Serialize;
use supervisor::BackendState;

#[derive(Serialize)]
struct HealthResponse {
    state: BackendState,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        state: state.backend_state(),
    })
}
