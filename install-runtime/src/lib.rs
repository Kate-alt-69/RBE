//! Trusted host-side execution for RBE package installation.
//!
//! `install-executor` owns the security contracts. This crate performs the
//! bounded network and staging I/O needed to execute those contracts without
//! exposing package-controlled sockets, shell execution, or unbounded bodies.

#![forbid(unsafe_code)]

// Keep installer tests self-contained instead of adding a dev-only dependency
// edge to the production engine lock graph. `extern crate self as tempfile`
// preserves the existing `tempfile::tempdir()` spelling inside unit tests.
#[cfg(test)]
extern crate self as tempfile;

#[cfg(test)]
mod test_tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    pub fn tempdir() -> std::io::Result<TempDir> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-install-runtime-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(TempDir { path })
    }
}

#[cfg(test)]
pub use test_tempdir::{tempdir, TempDir};

mod activation;
mod artifact;
mod build_plan;
mod cache;
mod graph;
mod http;
mod package;
mod package_service;
mod prepare;
mod promotion;
mod registry;
mod snapshot;
mod system_runtime;
mod target;
mod verified_worker;
mod worker_launch;
mod worker_source;

pub use activation::{
    activate_project_target, recover_project_activation, ActivationRuntimeError,
    InstallActivationProof, ProjectActivationResult, ProjectInstallRecovery,
};
pub use artifact::ArtifactStage;
pub use build_plan::{prepare_managed_build_plans, ManagedBuildPlanError, ManagedPackageBuildPlan};
pub use cache::stage_artifact_cached as stage_artifact;
pub use graph::{stage_resolved_root, VerifiedCapabilityInventory, VerifiedRootGraph};
pub use package::{
    inspect_registry_stage, read_verified_rpx_root_indexes, registry_artifact_plan,
    stage_registry_package, VerifiedRegistryPackage, VerifiedRpxRootIndex,
    MAX_RPX_PACKAGE_INDEX_BYTES, RPX_PACKAGE_INDEX,
};
pub use package_service::{
    read_verified_package_services, PackageServiceError, VerifiedPackageServiceSource,
    MAX_PACKAGE_SERVICES, MAX_PACKAGE_SERVICE_BYTES, PACKAGE_SERVICE_CAPABILITY,
};
pub use prepare::{prepare_prebuilt_activation_proofs, PrebuiltPreparationError};
pub use promotion::{
    promote_artifact, promote_verified_graph, ArtifactPromotionResult, ArtifactPromotionState,
    RootGraphPromotion,
};
pub use rbe_install_executor::{
    library_worker_proxy_bootstrap, GitSourceAcquisitionPlan, GitSourceReceipt,
    PinnedManagedToolchain, SourceFileDigest, VerifiedWorkerInvocation, WorkerProxyBridgeError,
};
pub use registry::{
    RegistryClient, DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES, DEFAULT_MAX_REGISTRY_INDEX_BYTES,
    MAX_REGISTRY_INDEX_BYTES,
};
pub use snapshot::{
    read_verified_rpx_root_snapshots, VerifiedRootSnapshotError, VerifiedRpxRootSnapshot,
};
pub use system_runtime::{
    current_system_runtime_host, load_admitted_system_runtime, AdmittedSystemRuntime,
    SystemRuntimeAdmissionError, SYSTEM_RUNTIME_ADMISSION_FILE, SYSTEM_RUNTIME_ADMISSION_FORMAT,
};
pub use target::{
    load_named_install_target, merge_named_install_target, InstallTargetError, NamedInstallTarget,
};
pub use verified_worker::{read_verified_root_worker_identities, VerifiedPackageWorkerIdentity};
pub use worker_launch::{
    prepare_verified_library_worker_launch, LibraryWorkerLaunchPreparationError,
    PreparedLibraryWorkerLaunch,
};
pub use worker_source::{
    prepare_verified_worker_source, MaterializedWorkerSource, VerifiedWorkerSourcePlan,
    WorkerSourceError,
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
    VersionMatch(#[from] rbe_library_resolver::VersionMatchError),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid install-runtime URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error(
        "install-runtime network URL must use credential-free HTTPS without a fragment: {0:?}"
    )]
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
    #[error(
        "verified package root {package:?} field {field} mismatch: lock={locked:?}, artifact={artifact:?}"
    )]
    VerifiedPackageMetadataMismatch {
        package: String,
        field: &'static str,
        locked: String,
        artifact: String,
    },
    #[error("verified package root {package:?} is missing pinned {toolchain} identity")]
    MissingLockedToolchain {
        package: String,
        toolchain: &'static str,
    },
    #[error(
        "verified package root {package:?} resolved {toolchain} version {resolved:?} does not satisfy artifact requirement {requirement:?}"
    )]
    ToolchainRequirementMismatch {
        package: String,
        toolchain: &'static str,
        resolved: String,
        requirement: String,
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
