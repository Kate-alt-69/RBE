use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde::Serialize;
use supervisor::BackendState;

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    state: BackendState,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let lifecycle = state.backend_state();
    Json(HealthResponse {
        ok: matches!(lifecycle, BackendState::Ready | BackendState::Running),
        state: lifecycle,
    })
}
