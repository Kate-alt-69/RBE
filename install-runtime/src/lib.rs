//! Trusted host-side execution for RBE package installation.
//!
//! `install-executor` owns the security contracts. This crate performs the
//! bounded network and staging I/O needed to execute those contracts without
//! exposing package-controlled sockets, shell execution, or unbounded bodies.

#![forbid(unsafe_code)]

mod artifact;
mod cache;
mod graph;
mod http;
mod package;
mod promotion;
mod registry;

pub use artifact::ArtifactStage;
pub use cache::stage_artifact_cached as stage_artifact;
pub use graph::{stage_resolved_root, VerifiedRootGraph};
pub use package::{
    inspect_registry_stage, read_verified_rpx_root_indexes, registry_artifact_plan,
    stage_registry_package, VerifiedRegistryPackage, VerifiedRpxRootIndex,
    MAX_RPX_PACKAGE_INDEX_BYTES, RPX_PACKAGE_INDEX,
};
pub use promotion::{
    promote_artifact, promote_verified_graph, ArtifactPromotionResult, ArtifactPromotionState,
    RootGraphPromotion,
};
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
    #[error(transparent)]
    PackageArchive(#[from] rbe_library_package::ArchiveError),
    #[error(transparent)]
    ProjectPackage(#[from] rbe_project_package::ProjectPackageError),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
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
    #[error("registry release {package:?} {version:?} is yanked and cannot be staged")]
    YankedRegistryRelease { package: String, version: String },
    #[error(
        "registry/package manifest mismatch for {package:?} field {field}: registry={registry:?}, manifest={manifest:?}"
    )]
    PackageMetadataMismatch {
        package: String,
        field: &'static str,
        registry: String,
        manifest: String,
    },
    #[error("library.toml is not a bounded regular manifest suitable for hashing")]
    InvalidManifestForHashing,
    #[error("RPX package index for root {package:?} is not a regular file")]
    InvalidRpxPackageIndexEntry { package: String },
    #[error(
        "RPX package index for root {package:?} exceeded {limit} bytes (observed at least {observed})"
    )]
    RpxPackageIndexTooLarge {
        package: String,
        limit: u64,
        observed: u64,
    },
    #[error("RPX package index for root {package:?} is not valid UTF-8")]
    RpxPackageIndexUtf8 { package: String },
    #[error("resolver root graph {root:?} is missing selected package {package:?}")]
    ResolutionPackageMissing { root: String, package: String },
    #[error("resolver install order for root {root:?} repeats package {package:?}")]
    ResolutionOrderDuplicate { root: String, package: String },
    #[error("resolver install order for root {root:?} omits selected package {package:?}")]
    ResolutionOrderIncomplete { root: String, package: String },
    #[error("hydrated registry graph is missing package index {package:?}")]
    RegistryIndexMissing { package: String },
    #[error("registry index for {package:?} is missing resolved release {version:?}")]
    RegistryReleaseMissing { package: String, version: String },
    #[error("verified root graph {root:?} contains duplicate package {package:?}")]
    DuplicateVerifiedPackage { root: String, package: String },
    #[error("verified root graph is missing requested root package {0:?}")]
    VerifiedRootMissing(String),
    #[error("verified root graph for {0:?} does not contain a complete private dependency graph")]
    VerifiedRootGraphIncomplete(String),
    #[error("verified root graph {root:?} is missing staged package {package:?}")]
    VerifiedGraphPackageMissing { root: String, package: String },
    #[error("invalid verified-artifact promotion plan")]
    InvalidPromotionPlan,
    #[error("artifact promotion verification size accounting overflow")]
    ArtifactSizeOverflow,
    #[error("artifact cache entry has an unsafe filesystem type: {0}")]
    UnsafeCacheEntry(String),
    #[error(
        "existing artifact cache entry {path} does not match expected SHA-256 {expected_sha256}"
    )]
    ExistingArtifactMismatch {
        path: String,
        expected_sha256: String,
    },
    #[error("artifact promotion race produced no reusable cache winner at {path}")]
    PromotionRace { path: String },
    #[error("promoted artifact {path} failed verification against SHA-256 {expected_sha256}")]
    PromotedArtifactVerification {
        path: String,
        expected_sha256: String,
    },
    #[error("install-runtime filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}
