use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{PinnedManagedTool, PinnedManagedToolchain, PinnedToolchainError, SourceFileDigest};

pub const DEFAULT_WORKER_STARTUP_TIMEOUT_SECONDS: u64 = 30;

/// Source-only worker launch contract.
///
/// This does not spawn a process. Backend must first create the private inherited
/// Library Host IPC endpoint, then call [`verify_before_spawn`] immediately before
/// constructing the child process. The returned invocation never grants direct
/// networking, never uses a shell, and starts from an empty environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLaunchPlan {
    runtime_tool: String,
    program: PinnedManagedTool,
    source_root: PathBuf,
    entrypoint: PathBuf,
    source_files: Vec<SourceFileDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorkerInvocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub working_directory: PathBuf,
    pub clear_environment: bool,
    pub environment: BTreeMap<String, String>,
    pub direct_network_allowed: bool,
    pub use_shell: bool,
    pub startup_timeout_seconds: u64,
}

impl WorkerLaunchPlan {
    /// Prepare an interpreted worker such as Bun, Node.js, or Python.
    ///
    /// `source_files` must describe the complete materialized package tree.
    /// The whole tree and the managed interpreter are verified now and again
    /// immediately before Backend is allowed to spawn the process.
    pub fn managed_interpreter(
        runtime_tool: impl Into<String>,
        toolchain: &PinnedManagedToolchain,
        source_root: impl AsRef<Path>,
        entrypoint: impl AsRef<Path>,
        source_files: Vec<SourceFileDigest>,
    ) -> Result<Self, WorkerLaunchError> {
        let runtime_tool = runtime_tool.into();
        let source_root = source_root.as_ref().to_path_buf();
        let entrypoint = entrypoint.as_ref().to_path_buf();
        validate_source_paths(&source_root, &entrypoint)?;
        verify_source_tree(&source_root, &source_files)?;
        let program = toolchain.verified_tool(&runtime_tool)?.clone();

        Ok(Self {
            runtime_tool,
            program,
            source_root,
            entrypoint,
            source_files,
        })
    }

    /// Re-verify every executable/source byte at the last source-only boundary
    /// before process creation.
    pub fn verify_before_spawn(&self) -> Result<VerifiedWorkerInvocation, WorkerLaunchError> {
        self.program.verify(&self.runtime_tool)?;
        validate_source_paths(&self.source_root, &self.entrypoint)?;
        verify_source_tree(&self.source_root, &self.source_files)?;

        Ok(VerifiedWorkerInvocation {
            program: self.program.path.clone(),
            args: vec![self.entrypoint.as_os_str().to_os_string()],
            working_directory: self.source_root.clone(),
            clear_environment: true,
            environment: BTreeMap::new(),
            direct_network_allowed: false,
            use_shell: false,
            startup_timeout_seconds: DEFAULT_WORKER_STARTUP_TIMEOUT_SECONDS,
        })
    }
}

fn validate_source_paths(source_root: &Path, entrypoint: &Path) -> Result<(), WorkerLaunchError> {
    if !source_root.is_absolute() {
        return Err(WorkerLaunchError::SourceRootMustBeAbsolute(
            source_root.to_path_buf(),
        ));
    }
    if !entrypoint.is_absolute() {
        return Err(WorkerLaunchError::EntrypointMustBeAbsolute(
            entrypoint.to_path_buf(),
        ));
    }
    if entrypoint == source_root || !entrypoint.starts_with(source_root) {
        return Err(WorkerLaunchError::EntrypointOutsideSourceRoot {
            root: source_root.to_path_buf(),
            entrypoint: entrypoint.to_path_buf(),
        });
    }
    ensure_no_symlink_components(source_root)?;
    let root_metadata = std::fs::symlink_metadata(source_root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(WorkerLaunchError::UnsafeSourceRoot(
            source_root.to_path_buf(),
        ));
    }
    ensure_no_symlink_components(entrypoint)?;
    let entry_metadata = std::fs::symlink_metadata(entrypoint)?;
    if entry_metadata.file_type().is_symlink() || !entry_metadata.is_file() {
        return Err(WorkerLaunchError::UnsafeEntrypoint(entrypoint.to_path_buf()));
    }
    Ok(())
}

fn verify_source_tree(
    source_root: &Path,
    source_files: &[SourceFileDigest],
) -> Result<(), WorkerLaunchError> {
    if source_files.is_empty() {
        return Err(WorkerLaunchError::EmptySourceTree);
    }

    let mut expected = BTreeMap::new();
    for digest in source_files {
        validate_relative_source_path(&digest.path)?;
        validate_sha256(&digest.sha256)?;
        if expected.insert(digest.path.clone(), digest).is_some() {
            return Err(WorkerLaunchError::DuplicateSourceFile(digest.path.clone()));
        }

        let path = join_source_path(source_root, &digest.path);
        ensure_no_symlink_components(&path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| WorkerLaunchError::MissingSourceFile(digest.path.clone()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorkerLaunchError::UnsafeSourceFile(path));
        }
        if metadata.len() != digest.size {
            return Err(WorkerLaunchError::SourceFileSizeMismatch {
                path: digest.path.clone(),
                expected: digest.size,
                actual: metadata.len(),
            });
        }
        let actual = hash_file(&path)?;
        if !actual.eq_ignore_ascii_case(&digest.sha256) {
            return Err(WorkerLaunchError::SourceFileHashMismatch {
                path: digest.path.clone(),
                expected: digest.sha256.to_ascii_lowercase(),
                actual,
            });
        }
    }

    let mut observed = BTreeSet::new();
    collect_source_files(source_root, source_root, &mut observed)?;
    let expected_paths = expected.keys().cloned().collect::<BTreeSet<_>>();
    if observed != expected_paths {
        if let Some(path) = observed.difference(&expected_paths).next() {
            return Err(WorkerLaunchError::UnexpectedSourceFile(path.clone()));
        }
        if let Some(path) = expected_paths.difference(&observed).next() {
            return Err(WorkerLaunchError::MissingSourceFile(path.clone()));
        }
        return Err(WorkerLaunchError::SourceTreeDrift);
    }
    Ok(())
}

fn collect_source_files(
    source_root: &Path,
    directory: &Path,
    observed: &mut BTreeSet<String>,
) -> Result<(), WorkerLaunchError> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkerLaunchError::SymlinkedSourcePath(path));
        }
        if metadata.is_dir() {
            collect_source_files(source_root, &path, observed)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(WorkerLaunchError::UnsafeSourceFile(path));
        }
        let relative = path
            .strip_prefix(source_root)
            .map_err(|_| WorkerLaunchError::SourceTreeDrift)?;
        let relative = relative_path_string(relative)?;
        if !observed.insert(relative.clone()) {
            return Err(WorkerLaunchError::DuplicateSourceFile(relative));
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> Result<String, WorkerLaunchError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let part = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| WorkerLaunchError::NonUtf8SourcePath(path.to_path_buf()))?;
        if part.is_empty() || matches!(part, "." | "..") {
            return Err(WorkerLaunchError::UnsafeRelativeSourcePath(
                path.display().to_string(),
            ));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(WorkerLaunchError::UnsafeRelativeSourcePath(String::new()));
    }
    Ok(parts.join("/"))
}

fn validate_relative_source_path(path: &str) -> Result<(), WorkerLaunchError> {
    if path.is_empty()
        || path.ends_with('/')
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(WorkerLaunchError::UnsafeRelativeSourcePath(
            path.to_string(),
        ));
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

fn validate_sha256(value: &str) -> Result<(), WorkerLaunchError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorkerLaunchError::InvalidSourceSha256(value.to_string()));
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, WorkerLaunchError> {
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

fn ensure_no_symlink_components(path: &Path) -> Result<(), WorkerLaunchError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorkerLaunchError::SymlinkedSourcePath(current));
            }
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerLaunchError {
    #[error("worker source root must be absolute: {0}")]
    SourceRootMustBeAbsolute(PathBuf),
    #[error("worker entrypoint must be absolute: {0}")]
    EntrypointMustBeAbsolute(PathBuf),
    #[error("worker entrypoint {entrypoint} must be inside source root {root}")]
    EntrypointOutsideSourceRoot { root: PathBuf, entrypoint: PathBuf },
    #[error("worker source root is not a safe regular directory: {0}")]
    UnsafeSourceRoot(PathBuf),
    #[error("worker entrypoint is not a safe regular file: {0}")]
    UnsafeEntrypoint(PathBuf),
    #[error("worker source tree must contain at least one file")]
    EmptySourceTree,
    #[error("worker source tree contains duplicate file {0:?}")]
    DuplicateSourceFile(String),
    #[error("worker source tree is missing file {0:?}")]
    MissingSourceFile(String),
    #[error("worker source tree contains unexpected file {0:?}")]
    UnexpectedSourceFile(String),
    #[error("worker source path traverses a symbolic link: {0}")]
    SymlinkedSourcePath(PathBuf),
    #[error("worker source path is not a safe regular file: {0}")]
    UnsafeSourceFile(PathBuf),
    #[error("unsafe worker relative source path {0:?}")]
    UnsafeRelativeSourcePath(String),
    #[error("worker source path is not valid UTF-8: {0}")]
    NonUtf8SourcePath(PathBuf),
    #[error("invalid worker source SHA-256 {0:?}")]
    InvalidSourceSha256(String),
    #[error("worker source file {path:?} size changed: expected {expected}, got {actual}")]
    SourceFileSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("worker source file {path:?} changed: expected SHA-256 {expected}, got {actual}")]
    SourceFileHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("worker source tree changed after materialization")]
    SourceTreeDrift,
    #[error(transparent)]
    Toolchain(#[from] PinnedToolchainError),
    #[error("worker launch verification I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::{ManagedToolchain, SourceFileHasher};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rbe-worker-launch-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn digest(path: &str, bytes: &[u8]) -> SourceFileDigest {
        let mut hasher = SourceFileHasher::new(path, bytes.len() as u64).unwrap();
        hasher.update(bytes).unwrap();
        hasher.finish().unwrap()
    }

    fn fixture() -> (
        TestDir,
        PathBuf,
        PathBuf,
        PinnedManagedToolchain,
        Vec<SourceFileDigest>,
    ) {
        let temp = TestDir::new();
        let runtime = temp.path().join("bun");
        std::fs::write(&runtime, b"managed-bun").unwrap();
        let toolchain = ManagedToolchain::new(BTreeMap::from([(
            "bun".to_string(),
            runtime,
        )]))
        .unwrap();
        let pinned = PinnedManagedToolchain::pin(&toolchain).unwrap();

        let root = temp.path().join("worker");
        std::fs::create_dir_all(root.join("internal")).unwrap();
        let entrypoint = root.join("worker.js");
        let helper = root.join("internal/helper.js");
        std::fs::write(&entrypoint, b"import './internal/helper.js';").unwrap();
        std::fs::write(&helper, b"export const ok = true;").unwrap();
        let files = vec![
            digest("worker.js", b"import './internal/helper.js';"),
            digest("internal/helper.js", b"export const ok = true;"),
        ];
        (temp, root, entrypoint, pinned, files)
    }

    #[test]
    fn managed_worker_plan_is_fail_closed() {
        let (_temp, root, entrypoint, pinned, files) = fixture();
        let plan = WorkerLaunchPlan::managed_interpreter(
            "bun",
            &pinned,
            &root,
            &entrypoint,
            files,
        )
        .unwrap();
        let invocation = plan.verify_before_spawn().unwrap();
        assert_eq!(invocation.args, vec![entrypoint.into_os_string()]);
        assert_eq!(invocation.working_directory, root);
        assert!(invocation.clear_environment);
        assert!(invocation.environment.is_empty());
        assert!(!invocation.direct_network_allowed);
        assert!(!invocation.use_shell);
    }

    #[test]
    fn source_mutation_after_planning_is_rejected() {
        let (_temp, root, entrypoint, pinned, files) = fixture();
        let plan = WorkerLaunchPlan::managed_interpreter(
            "bun",
            &pinned,
            &root,
            &entrypoint,
            files,
        )
        .unwrap();
        std::fs::write(root.join("internal/helper.js"), b"export const pwned = true;").unwrap();
        assert!(matches!(
            plan.verify_before_spawn(),
            Err(WorkerLaunchError::SourceFileSizeMismatch { .. })
                | Err(WorkerLaunchError::SourceFileHashMismatch { .. })
        ));
    }

    #[test]
    fn unexpected_source_file_after_planning_is_rejected() {
        let (_temp, root, entrypoint, pinned, files) = fixture();
        let plan = WorkerLaunchPlan::managed_interpreter(
            "bun",
            &pinned,
            &root,
            &entrypoint,
            files,
        )
        .unwrap();
        std::fs::write(root.join("injected.js"), b"pwned").unwrap();
        assert!(matches!(
            plan.verify_before_spawn(),
            Err(WorkerLaunchError::UnexpectedSourceFile(path)) if path == "injected.js"
        ));
    }

    #[test]
    fn managed_runtime_replacement_after_planning_is_rejected() {
        let (temp, root, entrypoint, pinned, files) = fixture();
        let plan = WorkerLaunchPlan::managed_interpreter(
            "bun",
            &pinned,
            &root,
            &entrypoint,
            files,
        )
        .unwrap();
        std::fs::write(temp.path().join("bun"), b"replaced-bun").unwrap();
        assert!(matches!(
            plan.verify_before_spawn(),
            Err(WorkerLaunchError::Toolchain(
                PinnedToolchainError::ToolHashMismatch { .. }
            ))
        ));
    }

    #[test]
    fn entrypoint_outside_materialized_root_is_rejected() {
        let (temp, root, _entrypoint, pinned, files) = fixture();
        let outside = temp.path().join("outside.js");
        std::fs::write(&outside, b"outside").unwrap();
        assert!(matches!(
            WorkerLaunchPlan::managed_interpreter("bun", &pinned, &root, &outside, files),
            Err(WorkerLaunchError::EntrypointOutsideSourceRoot { .. })
        ));
    }
}
