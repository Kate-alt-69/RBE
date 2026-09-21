//! Unified `backend install` request grammar and external-index discovery plans.
//!
//! Source-only: this crate performs no network I/O, downloads, process execution,
//! PATH mutation, or machine-wide installation.

#![forbid(unsafe_code)]

mod discovery;
mod request;
mod system_runtime;

pub use discovery::*;
pub use request::*;
pub use system_runtime::*;

pub const RBE_SYSTEM_PYTHON: &str = "rbe.sys.python";
pub const RBE_SYSTEM_NODEJS: &str = "rbe.sys.nodejs";
pub const RBE_SYSTEM_BUNJS: &str = "rbe.sys.bunjs";
pub const RBE_SYSTEM_RUST: &str = "rbe.sys.rust";
pub const SYSTEM_PYTHON_SCRAPER_ID: &str = "index-discovery-v1";
pub const RBE_WELL_KNOWN_INDEX: &str = "/.well-known/rbe/index.json";
pub const RBE_ROOT_INDEX: &str = "/rbe-index.json";
pub const RBE_INDEX_LINK_REL: &str = "rbe-index";
pub const KASTRICK_EXTERNAL_LOOKUP_PATH: &str = "/registry/v1/external/resolve";
pub const KASTRICK_INDEX_OBSERVATION_PATH: &str = "/registry/v1/external/observe";
pub const KASTRICK_SYSTEM_RUNTIME_MANIFEST_PREFIX: &str = "/registry/v1/system-runtime";

#[derive(Debug, thiserror::Error)]
pub enum InstallRequestError {
    #[error("backend install requires a target")]
    MissingTarget,
    #[error("invalid install key {0:?}")]
    InvalidInstallKey(String),
    #[error(
        "invalid version selector {0:?}; use numeric major, major.minor, or major.minor.patch"
    )]
    InvalidVersion(String),
    #[error("version was specified more than once")]
    DuplicateVersionFlag,
    #[error("version {inline:?} conflicts with -version={flag}")]
    ConflictingVersion { inline: String, flag: String },
    #[error("flag -{0} requires a value")]
    MissingFlagValue(&'static str),
    #[error("unknown install flag {0:?}")]
    UnknownFlag(String),
    #[error("invalid external locator {0:?}")]
    InvalidExternalLocator(String),
    #[error("invalid external URL {value:?}: {source}")]
    InvalidExternalUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("trusted bootstrap/download URL must use HTTPS: {0:?}")]
    HttpsRequired(String),
    #[error("could not construct discovery URL: {0}")]
    JoinUrl(#[source] url::ParseError),
    #[error("external-index/cache root must not be empty")]
    InvalidCacheRoot,
    #[error("scraper output attempted to escape the requested website origin")]
    ScraperOriginEscape,
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("invalid portable runtime entrypoint {0:?}")]
    InvalidEntrypoint(String),
    #[error("invalid host target id {0:?}")]
    InvalidHostId(String),
    #[error("user RBE root must not be empty")]
    InvalidUserRbeRoot,
    #[error("invalid system-runtime manifest JSON: {0}")]
    SystemRuntimeManifestJson(#[from] serde_json::Error),
    #[error("unsupported system-runtime manifest format {0}")]
    UnsupportedSystemRuntimeManifest(u32),
    #[error("unknown RBE system runtime {0:?}")]
    UnknownSystemRuntime(String),
    #[error("system-runtime manifest expected {expected:?}, got {actual:?}")]
    SystemRuntimeMismatch { expected: String, actual: String },
    #[error("system-runtime manifest expected host {expected:?}, got {actual:?}")]
    SystemRuntimeHostMismatch { expected: String, actual: String },
    #[error("invalid system-runtime version {0:?}")]
    InvalidSystemRuntimeVersion(String),
    #[error("system-runtime artifact size must be greater than zero")]
    InvalidSystemRuntimeSize,
    #[error("{0} must be non-empty and bounded")]
    InvalidSystemRuntimeText(&'static str),
}
