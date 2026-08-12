//! `/api/streaming` — Phase 6, gated on Phase −1's RTMP/WHIP spike (§11).

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
