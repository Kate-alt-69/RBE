//! `/api/maintenance` — recycle cleanup, cache clearing, service restart,
//! diagnostics. Phase 5+, built out alongside the task engine (§4's
//! Administration row) since most maintenance operations are really
//! task-engine jobs with a trigger endpoint in front of them.

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
