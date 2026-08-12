//! `/api/account` — Phase 5 (§10 UAC / Authentication), migrated last.

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
