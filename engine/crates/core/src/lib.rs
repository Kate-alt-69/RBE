//! Home for backend infrastructure/business logic.
//!
//! Transport-independent runtime state lives here so HTTP routes can observe
//! backend/container/service/video state without owning those runtimes.

mod container_client;
mod metrics;

use std::sync::Arc;

use config::Config;
use security::{HasIpStrikes, HasRateLimiters, IpStrikeTracker, RateLimiters};
use service_runtime::ServiceManager;
use supervisor::BackendState;
use tokio::sync::watch;
use video_manager::VideoManager;

pub use container_client::{ContainerClient, ContainerEndpointSnapshot};
pub use metrics::{BackendMetrics, BackendMetricsSnapshot, MaintenanceMetrics, MaintenanceSnapshot};

/// Shared application state, cloned cheaply into every Axum handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    state_rx: watch::Receiver<BackendState>,
    pub rate_limiters: Arc<RateLimiters>,
    pub ip_strikes: Arc<IpStrikeTracker>,
    pub vault: Arc<vault_process::VaultClient>,
    pub container: ContainerClient,
    pub services: ServiceManager,
    pub video_manager: Option<Arc<VideoManager>>,
    pub backend_metrics: Arc<BackendMetrics>,
    pub maintenance: Arc<MaintenanceMetrics>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        state_rx: watch::Receiver<BackendState>,
        vault: Arc<vault_process::VaultClient>,
        container: ContainerClient,
        services: ServiceManager,
        video_manager: Option<Arc<VideoManager>>,
        maintenance: Arc<MaintenanceMetrics>,
    ) -> Self {
        let rate_limiters = Arc::new(RateLimiters::new(&config.security));
        let ip_strikes = Arc::new(IpStrikeTracker::new(&config.security.ip_ban));
        Self {
            config,
            state_rx,
            rate_limiters,
            ip_strikes,
            vault,
            container,
            services,
            video_manager,
            backend_metrics: Arc::new(BackendMetrics::default()),
            maintenance,
        }
    }

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
