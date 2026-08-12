//! `/api/auth` — Phase 5 (§10 UAC / Authentication), migrated last.
//! Sessions, password hashing, 2FA, OAuth all land here — see §10 for
//! the crate choices (`argon2`, `totp-rs`, `oauth2`) and the note on
//! evaluating hand-rolled sessions vs. `tower-sessions` (§2.1).

use axum::Router;
use core_lib::AppState;

use super::not_yet_implemented;

pub fn routes() -> Router<AppState> {
    Router::new().fallback(not_yet_implemented)
}
