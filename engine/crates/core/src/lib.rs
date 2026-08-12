//! Home for backend infrastructure/business logic — migration-plan §3.1's
//! `core/` equivalent. Deliberately near-empty in Phase 0: this crate
//! should never depend on `axum` types (matches the handbook's
//! "Infrastructure First" principle — business logic shouldn't know
//! about the transport layer), so route handlers in `api` borrow from
//! [`AppState`] rather than the other way around.
//!
//! What's coming, by phase (see rust-migration-plan.md §12):
//! - Phase 1: vault is wired (see `vault` crate + `AppState::vault`);
//!   still TODO here: a `storage` module wrapping the `sqlx` pool.
//! - Phase 2: a `container_client` module — the IPC client talking to
//!   the *separate* container-runtime process (§5). Never merges that
//!   process in-process; this module is a client, not the runtime.
//! - Phase 3+: `email`, `task_engine`, `uac`, etc., each as their own
//!   module here, registered with the supervisor from `main.rs`.

use std::sync::Arc;

use config::Config;
use security::{HasIpStrikes, HasRateLimiters, IpStrikeTracker, RateLimiters};
use supervisor::BackendState;
use tokio::sync::watch;

/// Shared application state, cloned cheaply (an `Arc` clone) into every
/// `axum` handler via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    state_rx: watch::Receiver<BackendState>,
    pub rate_limiters: Arc<RateLimiters>,
    pub ip_strikes: Arc<IpStrikeTracker>,
    pub vault: Arc<vault::Vault>,
    // TODO(phase 1): pub storage: sqlx::AnyPool  (or SqlitePool/PgPool per config.storage.driver)
    // TODO(phase 2): pub container_client: Arc<container_client::Client>
}

impl AppState {
    /// `vault` is constructed separately in `main.rs`'s boot sequence
    /// (§3.2 step 3, before this — step 9) rather than built inside
    /// this constructor, since vault construction is fallible
    /// (filesystem/OS-keyring I/O) and that failure needs to surface
    /// at its own boot step, not be swallowed into an otherwise-
    /// infallible `AppState::new`.
    pub fn new(
        config: Arc<Config>,
        state_rx: watch::Receiver<BackendState>,
        vault: Arc<vault::Vault>,
    ) -> Self {
        let rate_limiters = Arc::new(RateLimiters::new(&config.security));
        let ip_strikes = Arc::new(IpStrikeTracker::new(&config.security.ip_ban));
        Self {
            config,
            state_rx,
            rate_limiters,
            ip_strikes,
            vault,
        }
    }

    /// Current backend lifecycle state (§3.2) — used by the health
    /// endpoint. Cheap: `watch::Receiver::borrow()` doesn't block or
    /// clone the whole channel.
    pub fn backend_state(&self) -> BackendState {
        *self.state_rx.borrow()
    }
}

impl HasRateLimiters for AppState {
    fn rate_limiters(&self) -> &RateLimiters {
        &self.rate_limiters
    }
}

impl HasIpStrikes for AppState {
    fn ip_strikes(&self) -> &IpStrikeTracker {
        &self.ip_strikes
    }

    fn trust_proxy_headers(&self) -> bool {
        self.config.security.trusted_proxy_headers
    }
}
