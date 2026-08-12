//! `/api/broadcast` — Phase 6, per §11's recommendation that streaming
//! gets prototyped standalone (Phase −1) before it's wired in here.

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
