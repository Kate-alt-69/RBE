//! Home for backend infrastructure/business logic.
//!
//! Transport-independent runtime state lives here so HTTP routes can observe
//! backend/container/service/video state without owning those runtimes.

mod container_client;
mod library_host;
mod metrics;
mod network_broker;
mod video_language;

use std::path::Path;
use std::sync::Arc;

use config::Config;
use security::{HasIpStrikes, HasRateLimiters, IpStrikeTracker, RateLimiters};
use service_runtime::ServiceManager;
use supervisor::BackendState;
use tokio::sync::watch;
use video_manager::VideoManager;

pub use container_client::{
    ContainerAuthorizedExecution, ContainerCapabilityBinding, ContainerClient,
    ContainerEndpointSnapshot, ContainerExecutionIdentity,
};
pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost,
    MAX_CAPABILITY_GRANTS_PER_MANIFEST as CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST,
    MAX_CAPABILITY_OPERATIONS_PER_GRANT as CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT,
    MAX_CAPABILITY_OPERATION_BYTES as CONTAINER_MAX_CAPABILITY_OPERATION_BYTES,
    MAX_CAPABILITY_PAYLOAD_BYTES as CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_CAPABILITY_TARGET_BYTES as CONTAINER_MAX_CAPABILITY_TARGET_BYTES,
    MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};
pub use library_host::{
    read_json_message as read_library_message, write_json_message as write_library_message,
    CapabilityGrant as LibraryCapabilityGrant, ExpectedWorkerIdentity, HostCall as LibraryHostCall,
    LibraryHostError, LibrarySession, PackageIdentity as LibraryPackageIdentity, PackageInvocation,
    PackageReply, RuntimeIdentity as LibraryRuntimeIdentity, SdkIdentity as LibrarySdkIdentity,
    SessionState as LibrarySessionState, WorkerHello as LibraryWorkerHello, LIBRARY_ABI_VERSION,
    LIBRARY_PROTOCOL_VERSION, MAX_LIBRARY_NAME_BYTES, MAX_LIBRARY_OPERATION_BYTES,
    MAX_LIBRARY_PAYLOAD_BYTES, MAX_LIBRARY_TARGET_BYTES,
};
pub use metrics::{
    BackendMetrics, BackendMetricsSnapshot, MaintenanceMetrics, MaintenanceSnapshot,
};
pub use network_broker::{
    call_public_http, PublicHttpError, PUBLIC_HTTP_MAX_TIMEOUT_MS, PUBLIC_HTTP_REQUEST_MAX_BYTES,
    PUBLIC_HTTP_RESPONSE_MAX_BYTES, PUBLIC_HTTP_TARGET,
};
pub use video_language::{
    video_language_operation_allowed, VideoLanguage, VideoLanguageError,
    VIDEO_CAPABILITY_TARGET_PREFIX, VIDEO_LANGUAGE_OPERATIONS,
};

/// Validate a candidate settings file with the exact same typed loader used at
/// backend startup. The dashboard uses this before replacing `settings.json`,
/// so editing cannot bypass schema or semantic validation.
pub fn validate_settings_file(path: impl AsRef<Path>) -> Result<(), String> {
    Config::load(path)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

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
