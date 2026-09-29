//! Sealed source-only contracts for managed JavaScript web builds.
//!
//! Web builds deliberately split first-resolution dependency locking from the
//! final build. Lock resolution may talk only to the npm registry with package
//! scripts disabled. The final build is derived from a verified lock, rechecks
//! the managed tool and source bytes, starts from an empty environment, never
//! uses a shell, and declares direct network access disabled.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{
    BuildDependencyCacheLayout, BuildDependencyEcosystem, BuildDependencyLock,
    PinnedManagedTool, PinnedManagedToolchain, PinnedToolchainError, SourceFileDigest,
};

pub const DEFAULT_WEB_LOCK_TIMEOUT_SECONDS: u64 = 10 * 60;
pub const DEFAULT_WEB_BUILD_TIMEOUT_SECONDS: u64 = 15 * 60;
pub const DEFAULT_WEB_LOCK_MAXIMUM_BYTES: u64 = 1024 * 1024 * 1024;
pub const NPM_REGISTRY_ORIGIN: &str = "https://registry.npmjs.org/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedWebTool {
    Bun,
    Npm,
}

impl ManagedWebTool {
    pub const fn managed_tool(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Npm => "npm",
        }
    }

    pub const fn ecosystem(self) -> BuildDependencyEcosystem {
        match self {
            Self::Bun => BuildDependencyEcosystem::Bun,
            Self::Npm => BuildDependencyEcosystem::Npm,
        }
    }

    pub const fn lock_name(self) -> &'static str {
        self.ecosystem().expected_lock_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebLockResolutionPlan {
    tool: ManagedWebTool,
    program: PinnedManagedTool,
    source_root: PathBuf,
    package_json: SourceFileDigest,
    source_files: Vec<SourceFileDigest>,
    cache_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWebLockResolutionInvocation {
    program: PathBuf,
    program_sha256: String,
    args: Vec<String>,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
    expected_lock_path: PathBuf,
    source_files: Vec<SourceFileDigest>,
    allowed_network_origins: Vec<String>,
    maximum_download_bytes: u64,
    timeout_seconds: u64,
    clear_environment: bool,
    use_shell: bool,
}

impl VerifiedWebLockResolutionInvocation {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn program_sha256(&self) -> &str {
        &self.program_sha256
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub fn expected_lock_path(&self) -> &Path {
        &self.expected_lock_path
    }

    pub fn source_files(&self) -> &[SourceFileDigest] {
        &self.source_files
    }

    pub fn allowed_network_origins(&self) -> &[String] {
        &self.allowed_network_origins
    }

    pub const fn maximum_download_bytes(&self) -> u64 {
        self.maximum_download_bytes
    }

    pub const fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    pub const fn clear_environment(&self) -> bool {
        self.clear_environment
    }

    pub const fn use_shell(&self) -> bool {
        self.use_shell
    }
}

impl WebLockResolutionPlan {
    pub fn new(
        tool: ManagedWebTool,
        toolchain: &PinnedManagedToolchain,
        source_root: impl AsRef<Path>,
        project_cache_root: impl AsRef<Path>,
        source_files: Vec<SourceFileDigest>,
    ) -> Result<Self, WebBuildError> {
        let source_root = checked_absolute_directory(source_root.as_ref())?;
        let project_cache_root = checked_absolute_directory(project_cache_root.as_ref())?;
        let package_json = source_files
            .iter()
            .find(|file| file.path == "package.json")
            .cloned()
            .ok_or(WebBuildError::MissingPackageJson)?;
        verify_expected_source_files(&source_root, &source_files)?;
        let expected_lock = source_root.join(tool.lock_name());
        if std::fs::symlink_metadata(&expected_lock).is_ok() {
            return Err(WebBuildError::DependencyLockAlreadyExists(expected_lock));
        }
        let program = toolchain.verified_tool(tool.managed_tool())?.clone();
        let cache_root = project_cache_root
            .join("rbe")
            .join("web-lock-resolution")
            .join(tool.ecosystem().key())
            .join(&package_json.sha256);
        Ok(Self {
            tool,
            program,
            source_root,
            package_json,
            source_files,
            cache_root,
        })
    }

    pub fn verify_before_spawn(
        &self,
    ) -> Result<VerifiedWebLockResolutionInvocation, WebBuildError> {
        self.program.verify(self.tool.managed_tool())?;
        verify_expected_source_files(&self.source_root, &self.source_files)?;
        verify_one_source_file(&self.source_root, &self.package_json)?;
        let expected_lock_path = self.source_root.join(self.tool.lock_name());
        if std::fs::symlink_metadata(&expected_lock_path).is_ok() {
            return Err(WebBuildError::DependencyLockAlreadyExists(
                expected_lock_path,
            ));
        }

        let cache = path_text(&self.cache_root)?;
        let mut environment = BTreeMap::from([
            ("RBE_HYDRATION_NETWORK".to_string(), "restricted".to_string()),
            ("RBE_HYDRATION_CACHE_ROOT".to_string(), cache.clone()),
        ]);
        let args = match self.tool {
            ManagedWebTool::Bun => {
                environment.insert("BUN_INSTALL_CACHE_DIR".to_string(), format!("{cache}/bun-cache"));
                vec![
                    "install".to_string(),
                    "--lockfile-only".to_string(),
                    "--ignore-scripts".to_string(),
                    "--cache-dir".to_string(),
                    format!("{cache}/bun-cache"),
                ]
            }
            ManagedWebTool::Npm => {
                environment.insert("npm_config_cache".to_string(), format!("{cache}/npm-cache"));
                environment.insert("npm_config_ignore_scripts".to_string(), "true".to_string());
                environment.insert("npm_config_audit".to_string(), "false".to_string());
                environment.insert("npm_config_fund".to_string(), "false".to_string());
                vec![
                    "install".to_string(),
                    "--package-lock-only".to_string(),
                    "--ignore-scripts".to_string(),
                    "--no-audit".to_string(),
                    "--no-fund".to_string(),
                    "--cache".to_string(),
                    format!("{cache}/npm-cache"),
                ]
            }
        };
        Ok(VerifiedWebLockResolutionInvocation {
            program: self.program.path().to_path_buf(),
            program_sha256: self.program.sha256().to_string(),
            args,
            working_directory: self.source_root.clone(),
            environment,
            expected_lock_path,
            source_files: self.source_files.clone(),
            allowed_network_origins: vec![NPM_REGISTRY_ORIGIN.to_string()],
            maximum_download_bytes: DEFAULT_WEB_LOCK_MAXIMUM_BYTES,
            timeout_seconds: DEFAULT_WEB_LOCK_TIMEOUT_SECONDS,
            clear_environment: true,
            use_shell: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBuildPlan {
    tool: ManagedWebTool,
    program: PinnedManagedTool,
    source_root: PathBuf,
    output_root: PathBuf,
    source_files: Vec<SourceFileDigest>,
    dependency_lock: BuildDependencyLock,
    dependency_cache: BuildDependencyCacheLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWebBuildInvocation {
    program: PathBuf,
    program_sha256: String,
    args: Vec<String>,
    working_directory: PathBuf,
    output_root: PathBuf,
    source_files: Vec<SourceFileDigest>,
    dependency_lock_sha256: String,
    environment: BTreeMap<String, String>,
    timeout_seconds: u64,
    clear_environment: bool,
    direct_network_allowed: bool,
    use_shell: bool,
}

impl VerifiedWebBuildInvocation {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn program_sha256(&self) -> &str {
        &self.program_sha256
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn output_root(&self) -> &Path {
        &self.output_root
    }

    pub fn source_files(&self) -> &[SourceFileDigest] {
        &self.source_files
    }

    pub fn dependency_lock_sha256(&self) -> &str {
        &self.dependency_lock_sha256
    }

    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub const fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    pub const fn clear_environment(&self) -> bool {
        self.clear_environment
    }

    pub const fn direct_network_allowed(&self) -> bool {
        self.direct_network_allowed
    }

    pub const fn use_shell(&self) -> bool {
        self.use_shell
    }
}

impl WebBuildPlan {
    pub fn new(
        tool: ManagedWebTool,
        toolchain: &PinnedManagedToolchain,
        source_root: impl AsRef<Path>,
        project_cache_root: impl AsRef<Path>,
        output_relative: impl AsRef<Path>,
        dependency_lock: BuildDependencyLock,
        source_files: Vec<SourceFileDigest>,
    ) -> Result<Self, WebBuildError> {
        let source_root = checked_absolute_directory(source_root.as_ref())?;
        let project_cache_root = checked_absolute_directory(project_cache_root.as_ref())?;
        verify_expected_source_files(&source_root, &source_files)?;
        if dependency_lock.ecosystem != tool.ecosystem()
            || dependency_lock.source_root != source_root
        {
            return Err(WebBuildError::DependencyLockMismatch);
        }
        verify_dependency_lock(&dependency_lock)?;
        let output_root = checked_output_root(&source_root, output_relative.as_ref())?;
        let program = toolchain.verified_tool(tool.managed_tool())?.clone();
        let dependency_cache = BuildDependencyCacheLayout::new(
            &project_cache_root,
            dependency_lock.ecosystem,
            &dependency_lock.sha256,
        )?;
        Ok(Self {
            tool,
            program,
            source_root,
            output_root,
            source_files,
            dependency_lock,
            dependency_cache,
        })
    }

    pub fn verify_before_spawn(&self) -> Result<VerifiedWebBuildInvocation, WebBuildError> {
        self.program.verify(self.tool.managed_tool())?;
        verify_expected_source_files(&self.source_root, &self.source_files)?;
        verify_dependency_lock(&self.dependency_lock)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&self.output_root) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WebBuildError::UnsafeOutputRoot(self.output_root.clone()));
            }
        }

        let cache = path_text(&self.dependency_cache.root)?;
        let mut environment = BTreeMap::from([
            ("RBE_BUILD_NETWORK".to_string(), "disabled".to_string()),
            ("RBE_HYDRATION_CACHE_ROOT".to_string(), cache.clone()),
            ("NEXT_TELEMETRY_DISABLED".to_string(), "1".to_string()),
        ]);
        match self.tool {
            ManagedWebTool::Bun => {
                environment.insert("BUN_INSTALL_CACHE_DIR".to_string(), format!("{cache}/bun-cache"));
            }
            ManagedWebTool::Npm => {
                environment.insert("npm_config_cache".to_string(), format!("{cache}/npm-cache"));
                environment.insert("npm_config_offline".to_string(), "true".to_string());
                environment.insert("npm_config_audit".to_string(), "false".to_string());
                environment.insert("npm_config_fund".to_string(), "false".to_string());
            }
        }
        Ok(VerifiedWebBuildInvocation {
            program: self.program.path().to_path_buf(),
            program_sha256: self.program.sha256().to_string(),
            args: vec!["run".to_string(), "build".to_string()],
            working_directory: self.source_root.clone(),
            output_root: self.output_root.clone(),
            source_files: self.source_files.clone(),
            dependency_lock_sha256: self.dependency_lock.sha256.clone(),
            environment,
            timeout_seconds: DEFAULT_WEB_BUILD_TIMEOUT_SECONDS,
            clear_environment: true,
            direct_network_allowed: false,
            use_shell: false,
        })
    }
}

fn checked_absolute_directory(path: &Path) -> Result<PathBuf, WebBuildError> {
    if !path.is_absolute() {
        return Err(WebBuildError::RootMustBeAbsolute(path.to_path_buf()));
    }
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WebBuildError::UnsafeRoot(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

fn checked_output_root(source_root: &Path, relative: &Path) -> Result<PathBuf, WebBuildError> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WebBuildError::UnsafeOutputRoot(relative.to_path_buf()));
    }
    let output = source_root.join(relative);
    if output == source_root || !output.starts_with(source_root) {
        return Err(WebBuildError::UnsafeOutputRoot(output));
    }
    Ok(output)
}

fn verify_expected_source_files(
    root: &Path,
    files: &[SourceFileDigest],
) -> Result<(), WebBuildError> {
    if files.is_empty() {
        return Err(WebBuildError::EmptySourceTree);
    }
    let mut seen = BTreeSet::new();
    for file in files {
        if !seen.insert(file.path.clone()) {
            return Err(WebBuildError::DuplicateSourceFile(file.path.clone()));
        }
        verify_one_source_file(root, file)?;
    }
    Ok(())
}

fn verify_one_source_file(root: &Path, file: &SourceFileDigest) -> Result<(), WebBuildError> {
    validate_relative_source_path(&file.path)?;
    validate_sha256(&file.sha256)?;
    let path = join_source_path(root, &file.path);
    ensure_no_symlink_components(&path)?;
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| WebBuildError::MissingSourceFile(file.path.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WebBuildError::UnsafeSourceFile(path));
    }
    if metadata.len() != file.size {
        return Err(WebBuildError::SourceFileSizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: metadata.len(),
        });
    }
    let actual = hash_file(&path)?;
    if actual != file.sha256.to_ascii_lowercase() {
        return Err(WebBuildError::SourceFileHashMismatch {
            path: file.path.clone(),
            expected: file.sha256.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

fn verify_dependency_lock(lock: &BuildDependencyLock) -> Result<(), WebBuildError> {
    ensure_no_symlink_components(&lock.absolute_path)?;
    let metadata = std::fs::symlink_metadata(&lock.absolute_path)
        .map_err(|_| WebBuildError::MissingDependencyLock(lock.absolute_path.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WebBuildError::UnsafeDependencyLock(lock.absolute_path.clone()));
    }
    let bytes = std::fs::read(&lock.absolute_path)?;
    lock.verify_bytes(&bytes)?;
    Ok(())
}

fn validate_relative_source_path(path: &str) -> Result<(), WebBuildError> {
    if path.is_empty()
        || path.ends_with('/')
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(WebBuildError::UnsafeRelativeSourcePath(path.to_string()));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), WebBuildError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WebBuildError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn join_source_path(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    path
}

fn hash_file(path: &Path) -> Result<String, WebBuildError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), WebBuildError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WebBuildError::SymlinkedPath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String, WebBuildError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| WebBuildError::NonUtf8Path(path.to_path_buf()))
}

#[derive(Debug, thiserror::Error)]
pub enum WebBuildError {
    #[error("web-build root must be an absolute path: {0}")]
    RootMustBeAbsolute(PathBuf),
    #[error("web-build root is not a safe directory: {0}")]
    UnsafeRoot(PathBuf),
    #[error("web-build path traverses a symbolic link: {0}")]
    SymlinkedPath(PathBuf),
    #[error("web-build source tree is empty")]
    EmptySourceTree,
    #[error("web-build source tree is missing package.json")]
    MissingPackageJson,
    #[error("duplicate web-build source file {0:?}")]
    DuplicateSourceFile(String),
    #[error("unsafe web-build source path {0:?}")]
    UnsafeRelativeSourcePath(String),
    #[error("invalid web-build SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("missing web-build source file {0:?}")]
    MissingSourceFile(String),
    #[error("unsafe web-build source file {0}")]
    UnsafeSourceFile(PathBuf),
    #[error("web-build source file {path:?} size mismatch: expected {expected}, got {actual}")]
    SourceFileSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("web-build source file {path:?} hash mismatch: expected {expected}, got {actual}")]
    SourceFileHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("web-build dependency lock already exists: {0}")]
    DependencyLockAlreadyExists(PathBuf),
    #[error("web-build dependency lock does not match the selected tool/source root")]
    DependencyLockMismatch,
    #[error("web-build dependency lock is missing: {0}")]
    MissingDependencyLock(PathBuf),
    #[error("web-build dependency lock is unsafe: {0}")]
    UnsafeDependencyLock(PathBuf),
    #[error("web-build output root is unsafe: {0}")]
    UnsafeOutputRoot(PathBuf),
    #[error("web-build path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error(transparent)]
    Toolchain(#[from] PinnedToolchainError),
    #[error(transparent)]
    Hydration(#[from] crate::HydrationError),
    #[error("web-build I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::ManagedToolchain;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rbe-web-build-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn digest(path: &str, bytes: &[u8]) -> SourceFileDigest {
        SourceFileDigest {
            path: path.to_string(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    fn pin_tool(root: &Path, name: &str) -> PinnedManagedToolchain {
        let path = root.join(name);
        std::fs::write(&path, format!("managed-{name}")).unwrap();
        let tools = ManagedToolchain::new(BTreeMap::from([(name.to_string(), path)])).unwrap();
        PinnedManagedToolchain::pin(&tools).unwrap()
    }

    #[test]
    fn bun_first_resolution_is_registry_only_and_script_free() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let cache = temp.0.join("cache");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let package = br#"{"private":true}"#;
        std::fs::write(source.join("package.json"), package).unwrap();
        let pinned = pin_tool(&temp.0, "bun");
        let plan = WebLockResolutionPlan::new(
            ManagedWebTool::Bun,
            &pinned,
            &source,
            &cache,
            vec![digest("package.json", package)],
        )
        .unwrap();
        let invocation = plan.verify_before_spawn().unwrap();
        assert_eq!(
            invocation.args(),
            [
                "install",
                "--lockfile-only",
                "--ignore-scripts",
                "--cache-dir",
                &invocation.environment()["BUN_INSTALL_CACHE_DIR"],
            ]
        );
        assert_eq!(invocation.allowed_network_origins(), [NPM_REGISTRY_ORIGIN]);
        assert!(invocation.clear_environment());
        assert!(!invocation.use_shell());
    }

    #[test]
    fn verified_build_is_offline_shell_free_and_rechecks_lock() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let cache = temp.0.join("cache");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let package = br#"{"private":true,"scripts":{"build":"next build"}}"#;
        let lock = b"lock-v1";
        std::fs::write(source.join("package.json"), package).unwrap();
        std::fs::write(source.join("bun.lock"), lock).unwrap();
        let pinned = pin_tool(&temp.0, "bun");
        let dependency_lock = BuildDependencyLock::new(
            BuildDependencyEcosystem::Bun,
            &source,
            "bun.lock",
            format!("{:x}", Sha256::digest(lock)),
        )
        .unwrap();
        let plan = WebBuildPlan::new(
            ManagedWebTool::Bun,
            &pinned,
            &source,
            &cache,
            "out",
            dependency_lock,
            vec![digest("package.json", package)],
        )
        .unwrap();
        let invocation = plan.verify_before_spawn().unwrap();
        assert_eq!(invocation.args(), ["run", "build"]);
        assert_eq!(invocation.output_root(), source.join("out"));
        assert!(invocation.clear_environment());
        assert!(!invocation.direct_network_allowed());
        assert!(!invocation.use_shell());
        assert_eq!(invocation.environment()["RBE_BUILD_NETWORK"], "disabled");

        std::fs::write(source.join("bun.lock"), b"lock-v2").unwrap();
        assert!(matches!(
            plan.verify_before_spawn(),
            Err(WebBuildError::Hydration(crate::HydrationError::LockHashMismatch { .. }))
        ));
    }

    #[test]
    fn source_tampering_is_rejected_before_lock_resolution() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let cache = temp.0.join("cache");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let package = b"{}";
        std::fs::write(source.join("package.json"), package).unwrap();
        let pinned = pin_tool(&temp.0, "npm");
        let plan = WebLockResolutionPlan::new(
            ManagedWebTool::Npm,
            &pinned,
            &source,
            &cache,
            vec![digest("package.json", package)],
        )
        .unwrap();
        std::fs::write(source.join("package.json"), b"tampered").unwrap();
        assert!(matches!(
            plan.verify_before_spawn(),
            Err(WebBuildError::SourceFileSizeMismatch { .. })
                | Err(WebBuildError::SourceFileHashMismatch { .. })
        ));
    }
}
