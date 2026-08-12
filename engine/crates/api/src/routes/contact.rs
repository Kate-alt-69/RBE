//! `/api/contact` — Phase 6 (§4 Contact Management row: queue, recycle
//! bin, Brotli archive, retention/purging).

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
