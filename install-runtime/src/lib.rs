//! Trusted host-side execution for RBE package installation.
//!
//! `install-executor` owns the security contracts. This crate performs the
//! bounded network and staging I/O needed to execute those contracts without
//! exposing package-controlled sockets, shell execution, or unbounded bodies.

#![forbid(unsafe_code)]

mod artifact;
mod http;
mod registry;

pub use artifact::{stage_artifact, ArtifactStage};
pub use registry::{
    RegistryClient, DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES, DEFAULT_MAX_REGISTRY_INDEX_BYTES,
    MAX_REGISTRY_INDEX_BYTES,
};

#[derive(Debug, thiserror::Error)]
pub enum InstallRuntimeError {
    #[error(transparent)]
    Executor(#[from] rbe_install_executor::ExecutorError),
    #[error(transparent)]
    RegistryContract(#[from] rbe_install_request::RegistryContractError),
    #[error(transparent)]
    RegistryBridge(#[from] rbe_library_registry::RegistryBridgeError),
    #[error("invalid install-runtime URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("install-runtime network URL must be credential-free HTTPS without a fragment: {0:?}")]
    UnsafeUrl(String),
    #[error("install-runtime destination is not publicly routable: {0}")]
    NonPublicDestination(String),
    #[error("install-runtime DNS resolution failed for {host:?}: {source}")]
    Dns {
        host: String,
        #[source]
        source: std::io::Error,
    },
    #[error("install-runtime DNS resolution returned no usable addresses for {0:?}")]
    EmptyDns(String),
    #[error("install-runtime DNS resolution returned too many addresses for {0:?}")]
    TooManyDnsAddresses(String),
    #[error("initialize install-runtime HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
    #[error("install-runtime HTTP request failed: {0}")]
    HttpRequest(#[source] reqwest::Error),
    #[error("install-runtime HTTP body read failed: {0}")]
    HttpBody(#[source] reqwest::Error),
    #[error("install-runtime HTTP body stalled for more than {0} seconds")]
    IdleTimeout(u64),
    #[error("install-runtime redirect is missing a valid Location header")]
    InvalidRedirect,
    #[error("install-runtime redirect limit exceeded")]
    TooManyRedirects,
    #[error("install-runtime server returned HTTP {0}")]
    HttpStatus(u16),
    #[error("registry package index byte limit {requested} is invalid; maximum is {maximum}")]
    InvalidRegistryIndexLimit { requested: usize, maximum: usize },
    #[error("registry package index exceeded {limit} bytes (observed at least {observed})")]
    RegistryIndexTooLarge { limit: usize, observed: usize },
    #[error("registry package index is not valid UTF-8")]
    RegistryIndexUtf8,
    #[error("registry graph package limit {requested} is invalid; maximum is {maximum}")]
    InvalidRegistryGraphLimit { requested: usize, maximum: usize },
    #[error("registry dependency graph exceeded {maximum} packages")]
    RegistryGraphTooLarge { maximum: usize },
    #[error("artifact server rejected or ignored a resume Range request")]
    ResumeRejected,
    #[error("artifact server returned invalid Content-Range {0:?}")]
    InvalidContentRange(String),
    #[error("artifact response Content-Length is invalid")]
    InvalidContentLength,
    #[error("artifact response length conflicts with the pinned artifact size")]
    ContentLengthMismatch,
    #[error("installer staging path traverses a symbolic link: {0}")]
    SymlinkedPath(String),
    #[error("installer staging entry has an unexpected filesystem type: {0}")]
    UnsafeStagingEntry(String),
    #[error("install-runtime filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}
