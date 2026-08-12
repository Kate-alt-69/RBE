//! `/api/admin` — Phase 5, alongside whatever it administers. Should
//! nest its own auth-check `tower::Layer` once UAC exists (§4's
//! Administration row), separate from the public-route permission
//! pipeline.

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
