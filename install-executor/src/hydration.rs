//! Controlled dependency hydration for network-dead package builds.
//!
//! Dependency managers get one bounded, origin-restricted network phase before
//! compilation. The resulting cache is addressed by the pinned dependency-lock
//! SHA-256. Actual package build invocations remain shell-free and network-dead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::execution::{BuildInvocation, ManagedToolchain};

pub const HYDRATION_RECEIPT_FORMAT: u32 = 1;
pub const HYDRATION_RECEIPT_FILE: &str = "hydration.rbe.json";
pub const DEFAULT_HYDRATION_TIMEOUT_SECONDS: u64 = 10 * 60;
pub const DEFAULT_MAX_HYDRATION_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildDependencyEcosystem {
    Cargo,
    Npm,
    Bun,
    Python,
}

impl BuildDependencyEcosystem {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Bun => "bun",
            Self::Python => "python",
        }
    }

    pub const fn expected_lock_name(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo.lock",
            Self::Npm => "package-lock.json",
            Self::Bun => "bun.lock",
            Self::Python => "requirements.lock",
        }
    }

    pub const fn managed_tool(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Bun => "bun",
            Self::Python => "python",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDependencyLock {
    pub ecosystem: BuildDependencyEcosystem,
    pub source_root: PathBuf,
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub sha256: String,
}

impl BuildDependencyLock {
    pub fn new(
        ecosystem: BuildDependencyEcosystem,
        source_root: impl Into<PathBuf>,
        relative_path: impl Into<PathBuf>,
        sha256: impl Into<String>,
    ) -> Result<Self, HydrationError> {
        let source_root = source_root.into();
        if source_root.as_os_str().is_empty() {
            return Err(HydrationError::EmptySourceRoot);
        }
        if !source_root.is_absolute() {
            return Err(HydrationError::SourceRootMustBeAbsolute(source_root));
        }

        let relative_path = relative_path.into();
        validate_relative_lock_path(&relative_path)?;
        let actual_name = relative_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| HydrationError::InvalidLockPath(relative_path.clone()))?;
        let expected_name = ecosystem.expected_lock_name();
        if actual_name != expected_name {
            return Err(HydrationError::UnexpectedLockName {
                ecosystem,
                expected: expected_name.to_string(),
                actual: actual_name.to_string(),
            });
        }

        let sha256 = canonical_sha256(&sha256.into())?;
        let absolute_path = source_root.join(&relative_path);
        path_text(&absolute_path)?;

        Ok(Self {
            ecosystem,
            source_root,
            relative_path,
            absolute_path,
            sha256,
        })
    }

    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), HydrationError> {
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != self.sha256 {
            return Err(HydrationError::LockHashMismatch {
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn working_directory(&self) -> Result<PathBuf, HydrationError> {
        self.absolute_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| HydrationError::InvalidLockPath(self.relative_path.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryOrigin {
    value: Url,
}

impl RegistryOrigin {
    pub fn parse(value: &str) -> Result<Self, HydrationError> {
        let url = Url::parse(value).map_err(|source| HydrationError::InvalidRegistryOrigin {
            value: value.to_string(),
            source,
        })?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(HydrationError::RegistryOriginMustUseHttps(
                value.to_string(),
            ));
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(HydrationError::RegistryOriginMustBeOrigin(
                value.to_string(),
            ));
        }
        Ok(Self { value: url })
    }

    pub fn as_str(&self) -> &str {
        self.value.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationPolicy {
    pub allowed_registry_origins: Vec<RegistryOrigin>,
    pub maximum_download_bytes: u64,
    pub timeout_seconds: u64,
}

impl HydrationPolicy {
    pub fn new(
        allowed_registry_origins: Vec<RegistryOrigin>,
        maximum_download_bytes: u64,
        timeout_seconds: u64,
    ) -> Result<Self, HydrationError> {
        if allowed_registry_origins.is_empty() {
            return Err(HydrationError::EmptyRegistryAllowlist);
        }
        let unique_origins: BTreeSet<_> = allowed_registry_origins
            .iter()
            .map(RegistryOrigin::as_str)
            .collect();
        if unique_origins.len() != allowed_registry_origins.len() {
            return Err(HydrationError::DuplicateRegistryOrigin);
        }
        if maximum_download_bytes == 0 {
            return Err(HydrationError::InvalidMaximumDownloadBytes);
        }
        if timeout_seconds == 0 {
            return Err(HydrationError::InvalidHydrationTimeout);
        }
        Ok(Self {
            allowed_registry_origins,
            maximum_download_bytes,
            timeout_seconds,
        })
    }

    pub fn official(ecosystem: BuildDependencyEcosystem) -> Result<Self, HydrationError> {
        let origins = match ecosystem {
            BuildDependencyEcosystem::Cargo => vec![
                RegistryOrigin::parse("https://index.crates.io/")?,
                RegistryOrigin::parse("https://static.crates.io/")?,
            ],
            BuildDependencyEcosystem::Npm | BuildDependencyEcosystem::Bun => {
                vec![RegistryOrigin::parse("https://registry.npmjs.org/")?]
            }
            BuildDependencyEcosystem::Python => vec![
                RegistryOrigin::parse("https://pypi.org/")?,
                RegistryOrigin::parse("https://files.pythonhosted.org/")?,
            ],
        };
        Self::new(
            origins,
            DEFAULT_MAX_HYDRATION_BYTES,
            DEFAULT_HYDRATION_TIMEOUT_SECONDS,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDependencyCacheLayout {
    pub root: PathBuf,
    pub receipt_path: PathBuf,
}

impl BuildDependencyCacheLayout {
    pub fn new(
        project_cache_root: impl AsRef<Path>,
        ecosystem: BuildDependencyEcosystem,
        lock_sha256: &str,
    ) -> Result<Self, HydrationError> {
        let project_cache_root = project_cache_root.as_ref();
        if project_cache_root.as_os_str().is_empty() {
            return Err(HydrationError::EmptyProjectCacheRoot);
        }
        if !project_cache_root.is_absolute() {
            return Err(HydrationError::ProjectCacheRootMustBeAbsolute(
                project_cache_root.to_path_buf(),
            ));
        }
        path_text(project_cache_root)?;
        let lock_sha256 = canonical_sha256(lock_sha256)?;
        let root = project_cache_root
            .join("rbe")
            .join("build-deps")
            .join(ecosystem.key())
            .join(lock_sha256);
        let receipt_path = root.join(HYDRATION_RECEIPT_FILE);
        Ok(Self { root, receipt_path })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationInvocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub clear_environment: bool,
    pub environment: BTreeMap<String, String>,
    pub network_allowed: bool,
    pub allowed_network_origins: Vec<String>,
    pub maximum_download_bytes: u64,
    pub timeout_seconds: u64,
    pub use_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyHydrationPlan {
    pub lock: BuildDependencyLock,
    pub cache: BuildDependencyCacheLayout,
    pub invocation: HydrationInvocation,
}

impl DependencyHydrationPlan {
    pub fn new(
        lock: BuildDependencyLock,
        project_cache_root: impl AsRef<Path>,
        toolchain: &ManagedToolchain,
        policy: HydrationPolicy,
    ) -> Result<Self, HydrationError> {
        let cache =
            BuildDependencyCacheLayout::new(project_cache_root, lock.ecosystem, &lock.sha256)?;
        let program = toolchain
            .tools
            .get(lock.ecosystem.managed_tool())
            .cloned()
            .ok_or_else(|| {
                HydrationError::MissingManagedTool(lock.ecosystem.managed_tool().to_string())
            })?;
        if !program.is_absolute() {
            return Err(HydrationError::ManagedToolMustBeAbsolute(program));
        }

        let working_directory = lock.working_directory()?;
        let allowed_network_origins = policy
            .allowed_registry_origins
            .iter()
            .map(|origin| origin.as_str().to_string())
            .collect();
        let (args, environment) = hydration_command(&lock, &cache)?;
        let invocation = HydrationInvocation {
            program,
            args,
            working_directory,
            clear_environment: true,
            environment,
            network_allowed: true,
            allowed_network_origins,
            maximum_download_bytes: policy.maximum_download_bytes,
            timeout_seconds: policy.timeout_seconds,
            use_shell: false,
        };

        Ok(Self {
            lock,
            cache,
            invocation,
        })
    }

    pub fn offline_environment(&self) -> BTreeMap<String, String> {
        offline_environment(self.lock.ecosystem, &self.cache)
    }

    pub fn apply_offline_build_environment(
        &self,
        invocations: &mut [BuildInvocation],
    ) -> Result<(), HydrationError> {
        let offline = self.offline_environment();
        for invocation in invocations {
            if invocation.network_allowed || invocation.use_shell || !invocation.clear_environment {
                return Err(HydrationError::UnsafeBuildInvocation);
            }
            for (key, value) in &offline {
                invocation.environment.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    }

    pub fn successful_receipt(
        &self,
        hydrated_artifacts: u64,
        observed_bytes: u64,
    ) -> Result<HydrationReceipt, HydrationError> {
        if observed_bytes > self.invocation.maximum_download_bytes {
            return Err(HydrationError::HydrationBytesExceeded {
                limit: self.invocation.maximum_download_bytes,
                observed: observed_bytes,
            });
        }

        let receipt = HydrationReceipt {
            format: HYDRATION_RECEIPT_FORMAT,
            ecosystem: self.lock.ecosystem,
            lock_sha256: self.lock.sha256.clone(),
            cache_root: self.cache.root.clone(),
            managed_program: self.invocation.program.clone(),
            hydration_args: self.invocation.args.clone(),
            allowed_registry_origins: self.invocation.allowed_network_origins.clone(),
            clear_environment: self.invocation.clear_environment,
            shell_disabled: !self.invocation.use_shell,
            scripts_disabled_during_hydration: true,
            network_restricted_to_origins: true,
            hydrated_artifacts,
            observed_bytes,
        };
        receipt.validate()?;
        Ok(receipt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydrationReceipt {
    pub format: u32,
    pub ecosystem: BuildDependencyEcosystem,
    pub lock_sha256: String,
    pub cache_root: PathBuf,
    pub managed_program: PathBuf,
    pub hydration_args: Vec<String>,
    pub allowed_registry_origins: Vec<String>,
    pub clear_environment: bool,
    pub shell_disabled: bool,
    pub scripts_disabled_during_hydration: bool,
    pub network_restricted_to_origins: bool,
    pub hydrated_artifacts: u64,
    pub observed_bytes: u64,
}

impl HydrationReceipt {
    pub fn to_json_pretty(&self) -> Result<String, HydrationError> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn parse_json(value: &str) -> Result<Self, HydrationError> {
        let receipt: Self = serde_json::from_str(value)?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), HydrationError> {
        if self.format != HYDRATION_RECEIPT_FORMAT {
            return Err(HydrationError::UnsupportedReceiptFormat(self.format));
        }
        let lock_sha256 = canonical_sha256(&self.lock_sha256)?;
        if !self.cache_root.is_absolute() {
            return Err(HydrationError::ProjectCacheRootMustBeAbsolute(
                self.cache_root.clone(),
            ));
        }
        let expected_suffix = PathBuf::from("rbe")
            .join("build-deps")
            .join(self.ecosystem.key())
            .join(lock_sha256);
        if !self.cache_root.ends_with(expected_suffix) {
            return Err(HydrationError::ReceiptCacheMismatch);
        }
        if !self.managed_program.is_absolute() {
            return Err(HydrationError::ManagedToolMustBeAbsolute(
                self.managed_program.clone(),
            ));
        }
        if self.hydration_args.is_empty() {
            return Err(HydrationError::EmptyHydrationCommand);
        }
        if self.allowed_registry_origins.is_empty() {
            return Err(HydrationError::EmptyRegistryAllowlist);
        }
        let mut unique_origins = BTreeSet::new();
        for origin in &self.allowed_registry_origins {
            let origin = RegistryOrigin::parse(origin)?;
            if !unique_origins.insert(origin.as_str().to_string()) {
                return Err(HydrationError::DuplicateRegistryOrigin);
            }
        }
        if !self.clear_environment
            || !self.shell_disabled
            || !self.scripts_disabled_during_hydration
            || !self.network_restricted_to_origins
        {
            return Err(HydrationError::UnsafeReceiptPolicy);
        }
        Ok(())
    }
}

fn hydration_command(
    lock: &BuildDependencyLock,
    cache: &BuildDependencyCacheLayout,
) -> Result<(Vec<String>, BTreeMap<String, String>), HydrationError> {
    let mut environment = BTreeMap::new();
    let cache_root = path_text(&cache.root)?;
    environment.insert(
        "RBE_HYDRATION_NETWORK".to_string(),
        "restricted".to_string(),
    );

    let args = match lock.ecosystem {
        BuildDependencyEcosystem::Cargo => {
            let manifest = lock
                .absolute_path
                .parent()
                .ok_or_else(|| HydrationError::InvalidLockPath(lock.relative_path.clone()))?
                .join("Cargo.toml");
            environment.insert(
                "CARGO_HOME".to_string(),
                path_text(&cache.root.join("cargo-home"))?,
            );
            environment.insert("CARGO_NET_OFFLINE".to_string(), "false".to_string());
            environment.insert("CARGO_TERM_COLOR".to_string(), "never".to_string());
            vec![
                "fetch".to_string(),
                "--locked".to_string(),
                "--manifest-path".to_string(),
                path_text(&manifest)?,
            ]
        }
        BuildDependencyEcosystem::Npm => {
            let npm_cache = path_text(&cache.root.join("npm-cache"))?;
            environment.insert("npm_config_cache".to_string(), npm_cache.clone());
            environment.insert("npm_config_ignore_scripts".to_string(), "true".to_string());
            environment.insert("npm_config_audit".to_string(), "false".to_string());
            environment.insert("npm_config_fund".to_string(), "false".to_string());
            vec![
                "ci".to_string(),
                "--ignore-scripts".to_string(),
                "--no-audit".to_string(),
                "--no-fund".to_string(),
                "--cache".to_string(),
                npm_cache,
            ]
        }
        BuildDependencyEcosystem::Bun => {
            let bun_cache = path_text(&cache.root.join("bun-cache"))?;
            environment.insert("BUN_INSTALL_CACHE_DIR".to_string(), bun_cache.clone());
            vec![
                "install".to_string(),
                "--frozen-lockfile".to_string(),
                "--ignore-scripts".to_string(),
                "--cache-dir".to_string(),
                bun_cache,
            ]
        }
        BuildDependencyEcosystem::Python => {
            let wheelhouse = path_text(&cache.root.join("wheelhouse"))?;
            environment.insert(
                "PIP_CACHE_DIR".to_string(),
                path_text(&cache.root.join("pip-cache"))?,
            );
            environment.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
            environment.insert("PIP_NO_INPUT".to_string(), "1".to_string());
            vec![
                "-m".to_string(),
                "pip".to_string(),
                "download".to_string(),
                "--require-hashes".to_string(),
                "--only-binary=:all:".to_string(),
                "--disable-pip-version-check".to_string(),
                "--no-input".to_string(),
                "--dest".to_string(),
                wheelhouse,
                "--requirement".to_string(),
                path_text(&lock.absolute_path)?,
            ]
        }
    };

    environment.insert("RBE_HYDRATION_CACHE_ROOT".to_string(), cache_root);
    Ok((args, environment))
}

fn offline_environment(
    ecosystem: BuildDependencyEcosystem,
    cache: &BuildDependencyCacheLayout,
) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::from([
        ("RBE_BUILD_NETWORK".to_string(), "disabled".to_string()),
        (
            "RBE_HYDRATION_CACHE_ROOT".to_string(),
            cache.root.to_string_lossy().into_owned(),
        ),
    ]);

    match ecosystem {
        BuildDependencyEcosystem::Cargo => {
            environment.insert(
                "CARGO_HOME".to_string(),
                cache.root.join("cargo-home").to_string_lossy().into_owned(),
            );
            environment.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());
        }
        BuildDependencyEcosystem::Npm => {
            environment.insert(
                "npm_config_cache".to_string(),
                cache.root.join("npm-cache").to_string_lossy().into_owned(),
            );
            environment.insert("npm_config_offline".to_string(), "true".to_string());
            environment.insert("npm_config_audit".to_string(), "false".to_string());
            environment.insert("npm_config_fund".to_string(), "false".to_string());
        }
        BuildDependencyEcosystem::Bun => {
            environment.insert(
                "BUN_INSTALL_CACHE_DIR".to_string(),
                cache.root.join("bun-cache").to_string_lossy().into_owned(),
            );
        }
        BuildDependencyEcosystem::Python => {
            environment.insert("PIP_NO_INDEX".to_string(), "1".to_string());
            environment.insert(
                "PIP_FIND_LINKS".to_string(),
                cache.root.join("wheelhouse").to_string_lossy().into_owned(),
            );
            environment.insert("PIP_REQUIRE_HASHES".to_string(), "1".to_string());
            environment.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
            environment.insert("PIP_NO_INPUT".to_string(), "1".to_string());
        }
    }

    environment
}

fn validate_relative_lock_path(path: &Path) -> Result<(), HydrationError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(HydrationError::InvalidLockPath(path.to_path_buf()));
    }
    let text = path.to_string_lossy();
    if text.contains('\\') || text.contains(':') {
        return Err(HydrationError::InvalidLockPath(path.to_path_buf()));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HydrationError::InvalidLockPath(path.to_path_buf()));
    }
    Ok(())
}

fn canonical_sha256(value: &str) -> Result<String, HydrationError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HydrationError::InvalidSha256(value.to_string()));
    }
    Ok(value.to_ascii_lowercase())
}

fn path_text(path: &Path) -> Result<String, HydrationError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| HydrationError::NonUtf8Path(path.to_path_buf()))
}

#[derive(Debug, thiserror::Error)]
pub enum HydrationError {
    #[error("build dependency source root must be non-empty")]
    EmptySourceRoot,
    #[error("build dependency source root must be absolute: {0:?}")]
    SourceRootMustBeAbsolute(PathBuf),
    #[error("invalid dependency lock path {0:?}")]
    InvalidLockPath(PathBuf),
    #[error("{ecosystem:?} dependency lock must be named {expected:?}, got {actual:?}")]
    UnexpectedLockName {
        ecosystem: BuildDependencyEcosystem,
        expected: String,
        actual: String,
    },
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("dependency lock hash mismatch: expected {expected}, got {actual}")]
    LockHashMismatch { expected: String, actual: String },
    #[error("invalid registry origin {value:?}: {source}")]
    InvalidRegistryOrigin {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("registry origin must use credential-free HTTPS: {0:?}")]
    RegistryOriginMustUseHttps(String),
    #[error("registry allowlist entries must be bare origins: {0:?}")]
    RegistryOriginMustBeOrigin(String),
    #[error("dependency hydration requires at least one allowed registry origin")]
    EmptyRegistryAllowlist,
    #[error("dependency hydration registry origins must be unique")]
    DuplicateRegistryOrigin,
    #[error("dependency hydration maximum download bytes must be greater than zero")]
    InvalidMaximumDownloadBytes,
    #[error("dependency hydration timeout must be greater than zero")]
    InvalidHydrationTimeout,
    #[error("project cache root must be non-empty")]
    EmptyProjectCacheRoot,
    #[error("project cache root must be absolute: {0:?}")]
    ProjectCacheRootMustBeAbsolute(PathBuf),
    #[error("managed dependency tool {0:?} is unavailable")]
    MissingManagedTool(String),
    #[error("managed dependency tool path must be absolute: {0:?}")]
    ManagedToolMustBeAbsolute(PathBuf),
    #[error("path is not valid UTF-8: {0:?}")]
    NonUtf8Path(PathBuf),
    #[error("hydration observed {observed} bytes, exceeding limit {limit}")]
    HydrationBytesExceeded { limit: u64, observed: u64 },
    #[error("offline build invocation lost shell/environment/network isolation")]
    UnsafeBuildInvocation,
    #[error("unsupported hydration receipt format {0}")]
    UnsupportedReceiptFormat(u32),
    #[error("hydration receipt cache root does not match ecosystem and lock digest")]
    ReceiptCacheMismatch,
    #[error("hydration receipt command must not be empty")]
    EmptyHydrationCommand,
    #[error("hydration receipt does not prove the required isolation policy")]
    UnsafeReceiptPolicy,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{build_invocations, ManagedToolchain};
    use rbe_library_package::BuildStep;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn toolchain() -> ManagedToolchain {
        let mut tools = BTreeMap::new();
        for name in ["cargo", "npm", "bun", "python"] {
            tools.insert(name.to_string(), PathBuf::from(format!("/managed/{name}")));
        }
        ManagedToolchain::new(tools).unwrap()
    }

    fn lock(ecosystem: BuildDependencyEcosystem) -> BuildDependencyLock {
        BuildDependencyLock::new(
            ecosystem,
            "/project/.cache/library/.build/pkg",
            ecosystem.expected_lock_name(),
            "a".repeat(64),
        )
        .unwrap()
    }

    fn plan(ecosystem: BuildDependencyEcosystem) -> DependencyHydrationPlan {
        DependencyHydrationPlan::new(
            lock(ecosystem),
            "/project/.cache",
            &toolchain(),
            HydrationPolicy::official(ecosystem).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn pinned_lock_bytes_are_verified_before_hydration() {
        let lock = BuildDependencyLock::new(
            BuildDependencyEcosystem::Cargo,
            "/source",
            "Cargo.lock",
            ABC_SHA256,
        )
        .unwrap();
        lock.verify_bytes(b"abc").unwrap();
        assert!(matches!(
            lock.verify_bytes(b"abd"),
            Err(HydrationError::LockHashMismatch { .. })
        ));
    }

    #[test]
    fn lock_path_escape_and_wrong_lock_type_are_rejected() {
        assert!(matches!(
            BuildDependencyLock::new(
                BuildDependencyEcosystem::Cargo,
                "/source",
                "../Cargo.lock",
                "a".repeat(64),
            ),
            Err(HydrationError::InvalidLockPath(_))
        ));
        assert!(matches!(
            BuildDependencyLock::new(
                BuildDependencyEcosystem::Npm,
                "/source",
                "bun.lock",
                "a".repeat(64),
            ),
            Err(HydrationError::UnexpectedLockName { .. })
        ));
    }

    #[test]
    fn registry_allowlist_rejects_paths_credentials_and_duplicates() {
        assert!(matches!(
            RegistryOrigin::parse("https://registry.npmjs.org/pkg"),
            Err(HydrationError::RegistryOriginMustBeOrigin(_))
        ));
        assert!(matches!(
            RegistryOrigin::parse("https://user:pass@registry.npmjs.org/"),
            Err(HydrationError::RegistryOriginMustUseHttps(_))
        ));
        let origin = RegistryOrigin::parse("https://registry.npmjs.org/").unwrap();
        assert!(matches!(
            HydrationPolicy::new(vec![origin.clone(), origin], 1, 1),
            Err(HydrationError::DuplicateRegistryOrigin)
        ));
    }

    #[test]
    fn cargo_hydration_is_locked_and_origin_restricted() {
        let plan = plan(BuildDependencyEcosystem::Cargo);
        assert_eq!(plan.invocation.program, PathBuf::from("/managed/cargo"));
        assert_eq!(
            plan.cache.root,
            PathBuf::from("/project/.cache/rbe/build-deps/cargo").join("a".repeat(64))
        );
        assert!(plan.invocation.args.iter().any(|arg| arg == "--locked"));
        assert!(plan.invocation.network_allowed);
        assert!(!plan.invocation.allowed_network_origins.is_empty());
        assert!(plan.invocation.clear_environment);
        assert!(!plan.invocation.use_shell);
    }

    #[test]
    fn npm_and_bun_disable_install_scripts_and_freeze_locks() {
        let npm = plan(BuildDependencyEcosystem::Npm);
        assert_eq!(npm.invocation.args[0], "ci");
        assert!(npm
            .invocation
            .args
            .iter()
            .any(|arg| arg == "--ignore-scripts"));

        let bun = plan(BuildDependencyEcosystem::Bun);
        assert!(bun
            .invocation
            .args
            .iter()
            .any(|arg| arg == "--frozen-lockfile"));
        assert!(bun
            .invocation
            .args
            .iter()
            .any(|arg| arg == "--ignore-scripts"));
    }

    #[test]
    fn python_hydrates_only_hash_locked_wheels() {
        let python = plan(BuildDependencyEcosystem::Python);
        for required in ["download", "--require-hashes", "--only-binary=:all:"] {
            assert!(python.invocation.args.iter().any(|arg| arg == required));
        }
        assert_eq!(python.invocation.program, PathBuf::from("/managed/python"));
    }

    #[test]
    fn actual_build_remains_network_dead_after_hydration() {
        let plan = plan(BuildDependencyEcosystem::Cargo);
        let mut invocations = build_invocations(
            &[BuildStep {
                program: "cargo".into(),
                args: vec!["build".into(), "--release".into()],
            }],
            &toolchain(),
            "/project/.cache/library/.build/pkg",
        )
        .unwrap();
        plan.apply_offline_build_environment(&mut invocations)
            .unwrap();
        let build = &invocations[0];
        assert!(!build.network_allowed);
        assert!(!build.use_shell);
        assert!(build.clear_environment);
        assert_eq!(
            build
                .environment
                .get("CARGO_NET_OFFLINE")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            build
                .environment
                .get("RBE_BUILD_NETWORK")
                .map(String::as_str),
            Some("disabled")
        );
    }

    #[test]
    fn hydration_receipt_is_strict_and_round_trips() {
        let plan = plan(BuildDependencyEcosystem::Python);
        let receipt = plan.successful_receipt(12, 4096).unwrap();
        let json = receipt.to_json_pretty().unwrap();
        let parsed = HydrationReceipt::parse_json(&json).unwrap();
        assert_eq!(parsed, receipt);
        assert!(parsed.shell_disabled);
        assert!(parsed.clear_environment);
        assert!(parsed.scripts_disabled_during_hydration);
        assert!(parsed.network_restricted_to_origins);
    }

    #[test]
    fn receipt_cache_tampering_is_rejected() {
        let plan = plan(BuildDependencyEcosystem::Npm);
        let mut receipt = plan.successful_receipt(1, 128).unwrap();
        receipt.cache_root = PathBuf::from("/project/.cache/rbe/build-deps/npm/not-the-lock");
        assert!(matches!(
            receipt.validate(),
            Err(HydrationError::ReceiptCacheMismatch)
        ));
    }

    #[test]
    fn hydration_byte_limit_is_enforced() {
        let mut plan = plan(BuildDependencyEcosystem::Bun);
        plan.invocation.maximum_download_bytes = 64;
        assert!(matches!(
            plan.successful_receipt(1, 65),
            Err(HydrationError::HydrationBytesExceeded {
                limit: 64,
                observed: 65
            })
        ));
    }
}
