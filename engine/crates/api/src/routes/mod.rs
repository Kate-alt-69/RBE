//! One module per route group, matching the handbook's `/api/` layout
//! (§4's Route Organization: account/, auth/, broadcast/, contact/,
//! streaming/, admin/, maintenance/). Every module is a Phase 0 stub —
//! real handlers land alongside their subsystem in later phases:
//!
//! - `auth`, `account` -> Phase 5, last (§10 UAC)
//! - `contact` -> Phase 6 (§4 Contact Management row)
//! - `streaming`, `broadcast` -> Phase 6 (§11)
//! - `admin`, `maintenance` -> Phase 5, alongside whatever they administer

pub mod account;
pub mod admin;
pub mod auth;
pub mod broadcast;
pub mod contact;
pub mod maintenance;
pub mod streaming;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Shared stub handler for every not-yet-implemented route group.
/// Returns 501, not 404 — the route exists and is planned, it's just
/// not built yet, which is a meaningfully different signal for anyone
/// probing the API during the migration.
pub(crate) async fn not_yet_implemented() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        "this route group has not been migrated from the Node backend yet",
    )
        .into_response()
}
