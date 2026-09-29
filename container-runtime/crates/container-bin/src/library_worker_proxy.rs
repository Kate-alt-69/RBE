use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ipc_protocol::{LibraryWorkerProxyBootstrap, LibraryWorkerProxyError};
use sha2::{Digest, Sha256};

/// Opaque Container-side proof that every executable/source byte named by the
/// Library Worker Proxy contract matched its pinned identity at verification
/// time.
///
/// Container execution code must call [`Self::verify_before_spawn`] at the last
/// possible boundary before creating the sandboxed process. The fields remain
/// private so callers cannot construct a "verified" proxy by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLibraryWorkerProxy {
    bootstrap: LibraryWorkerProxyBootstrap,
}

impl VerifiedLibraryWorkerProxy {
    pub fn program(&self) -> &Path {
        Path::new(&self.bootstrap.program)
    }

    pub fn args(&self) -> &[String] {
        &self.bootstrap.args
    }

    pub fn working_directory(&self) -> &Path {
        Path::new(&self.bootstrap.working_directory)
    }

    pub const fn startup_timeout_seconds(&self) -> u64 {
        self.bootstrap.startup_timeout_seconds
    }

    pub fn source_file_count(&self) -> usize {
        self.bootstrap.source_files.len()
    }

    /// Re-hash the managed interpreter and the complete materialized source
    /// tree immediately before sandboxed process creation.
    pub fn verify_before_spawn(&self) -> Result<(), ContainerWorkerProxyError> {
        verify_proxy_filesystem(&self.bootstrap)
    }
}

/// Convert a structurally valid Backend handoff into an opaque Container proof
/// only after independently re-verifying the exact interpreter and full source
/// tree on Container's side of the trust boundary.
pub fn verify_library_worker_proxy(
    bootstrap: LibraryWorkerProxyBootstrap,
) -> Result<VerifiedLibraryWorkerProxy, ContainerWorkerProxyError> {
    verify_proxy_filesystem(&bootstrap)?;
    Ok(VerifiedLibraryWorkerProxy { bootstrap })
}

fn verify_proxy_filesystem(
    bootstrap: &LibraryWorkerProxyBootstrap,
) -> Result<(), ContainerWorkerProxyError> {
    bootstrap
        .validate()
        .map_err(ContainerWorkerProxyError::Contract)?;

    let program = Path::new(&bootstrap.program);
    ensure_no_symlink_components(program)?;
    let program_metadata = std::fs::symlink_metadata(program)?;
    if program_metadata.file_type().is_symlink() || !program_metadata.is_file() {
        return Err(ContainerWorkerProxyError::UnsafeProgram(
            program.to_path_buf(),
        ));
    }
    let actual_program_sha256 = hash_file(program)?;
    if !actual_program_sha256.eq_ignore_ascii_case(&bootstrap.program_sha256) {
        return Err(ContainerWorkerProxyError::ProgramHashMismatch {
            expected: bootstrap.program_sha256.to_ascii_lowercase(),
            actual: actual_program_sha256,
        });
    }

    let root = Path::new(&bootstrap.working_directory);
    ensure_no_symlink_components(root)?;
    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ContainerWorkerProxyError::UnsafeSourceRoot(
            root.to_path_buf(),
        ));
    }

    let mut expected_paths = BTreeSet::new();
    for source in &bootstrap.source_files {
        if !expected_paths.insert(source.path.clone()) {
            return Err(ContainerWorkerProxyError::SourceTreeDrift);
        }
        let path = join_source_path(root, &source.path);
        ensure_no_symlink_components(&path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| ContainerWorkerProxyError::MissingSourceFile(source.path.clone()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ContainerWorkerProxyError::UnsafeSourceFile(path));
        }
        if metadata.len() != source.size {
            return Err(ContainerWorkerProxyError::SourceFileSizeMismatch {
                path: source.path.clone(),
                expected: source.size,
                actual: metadata.len(),
            });
        }
        let actual = hash_file(&path)?;
        if !actual.eq_ignore_ascii_case(&source.sha256) {
            return Err(ContainerWorkerProxyError::SourceFileHashMismatch {
                path: source.path.clone(),
                expected: source.sha256.to_ascii_lowercase(),
                actual,
            });
        }
    }

    let mut observed_paths = BTreeSet::new();
    collect_source_files(root, root, &mut observed_paths)?;
    if observed_paths != expected_paths {
        if let Some(path) = observed_paths.difference(&expected_paths).next() {
            return Err(ContainerWorkerProxyError::UnexpectedSourceFile(
                path.clone(),
            ));
        }
        if let Some(path) = expected_paths.difference(&observed_paths).next() {
            return Err(ContainerWorkerProxyError::MissingSourceFile(path.clone()));
        }
        return Err(ContainerWorkerProxyError::SourceTreeDrift);
    }
    Ok(())
}

fn collect_source_files(
    root: &Path,
    directory: &Path,
    observed: &mut BTreeSet<String>,
) -> Result<(), ContainerWorkerProxyError> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(ContainerWorkerProxyError::SymlinkedPath(path));
        }
        if metadata.is_dir() {
            collect_source_files(root, &path, observed)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(ContainerWorkerProxyError::UnsafeSourceFile(path));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ContainerWorkerProxyError::SourceTreeDrift)?;
        let relative = relative_path_string(relative)?;
        if !observed.insert(relative) {
            return Err(ContainerWorkerProxyError::SourceTreeDrift);
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> Result<String, ContainerWorkerProxyError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(ContainerWorkerProxyError::SourceTreeDrift);
        };
        let value = value
            .to_str()
            .ok_or_else(|| ContainerWorkerProxyError::NonUtf8SourcePath(path.to_path_buf()))?;
        if value.is_empty() || matches!(value, "." | "..") {
            return Err(ContainerWorkerProxyError::SourceTreeDrift);
        }
        parts.push(value);
    }
    if parts.is_empty() {
        return Err(ContainerWorkerProxyError::SourceTreeDrift);
    }
    Ok(parts.join("/"))
}

fn join_source_path(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
    }
    path
}

fn hash_file(path: &Path) -> Result<String, ContainerWorkerProxyError> {
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

fn ensure_no_symlink_components(path: &Path) -> Result<(), ContainerWorkerProxyError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ContainerWorkerProxyError::SymlinkedPath(current));
            }
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum ContainerWorkerProxyError {
    Contract(LibraryWorkerProxyError),
    SymlinkedPath(PathBuf),
    UnsafeProgram(PathBuf),
    ProgramHashMismatch {
        expected: String,
        actual: String,
    },
    UnsafeSourceRoot(PathBuf),
    MissingSourceFile(String),
    UnexpectedSourceFile(String),
    UnsafeSourceFile(PathBuf),
    SourceFileSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    SourceFileHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    NonUtf8SourcePath(PathBuf),
    SourceTreeDrift,
    Io(std::io::Error),
}

impl fmt::Display for ContainerWorkerProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(formatter, "invalid Library Worker Proxy contract: {error}"),
            Self::SymlinkedPath(path) => write!(
                formatter,
                "Library Worker Proxy path traverses a symbolic link: {}",
                path.display()
            ),
            Self::UnsafeProgram(path) => write!(
                formatter,
                "Library Worker Proxy program is not a regular file: {}",
                path.display()
            ),
            Self::ProgramHashMismatch { expected, actual } => write!(
                formatter,
                "Library Worker Proxy program changed after Backend verification: expected SHA-256 {expected}, got {actual}"
            ),
            Self::UnsafeSourceRoot(path) => write!(
                formatter,
                "Library Worker Proxy source root is not a regular directory: {}",
                path.display()
            ),
            Self::MissingSourceFile(path) => {
                write!(formatter, "Library Worker Proxy source file {path:?} is missing")
            }
            Self::UnexpectedSourceFile(path) => write!(
                formatter,
                "Library Worker Proxy source tree contains unexpected file {path:?}"
            ),
            Self::UnsafeSourceFile(path) => write!(
                formatter,
                "Library Worker Proxy source path is not a regular file: {}",
                path.display()
            ),
            Self::SourceFileSizeMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "Library Worker Proxy source file {path:?} size changed: expected {expected}, got {actual}"
            ),
            Self::SourceFileHashMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "Library Worker Proxy source file {path:?} changed: expected SHA-256 {expected}, got {actual}"
            ),
            Self::NonUtf8SourcePath(path) => write!(
                formatter,
                "Library Worker Proxy source path is not UTF-8: {}",
                path.display()
            ),
            Self::SourceTreeDrift => {
                formatter.write_str("Library Worker Proxy source tree changed after verification")
            }
            Self::Io(error) => write!(formatter, "Library Worker Proxy filesystem verification failed: {error}"),
        }
    }
}

impl Error for ContainerWorkerProxyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ContainerWorkerProxyError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use ipc_protocol::{LibraryWorkerProxySourceFile, LIBRARY_WORKER_PROXY_PROTOCOL_VERSION};

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
                "rbe-container-library-proxy-test-{}-{nonce}-{}",
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

    struct Fixture {
        _temp: TestDir,
        program: PathBuf,
        root: PathBuf,
        helper: PathBuf,
        bootstrap: LibraryWorkerProxyBootstrap,
    }

    fn fixture() -> Fixture {
        let temp = TestDir::new();
        let program = temp.path().join("managed-bun");
        std::fs::write(&program, b"managed-bun-v1").unwrap();

        let root = temp.path().join("worker");
        std::fs::create_dir_all(root.join("internal")).unwrap();
        let entrypoint = root.join("worker.js");
        let helper = root.join("internal/helper.js");
        std::fs::write(&entrypoint, b"import './internal/helper.js';").unwrap();
        std::fs::write(&helper, b"export const ok = true;").unwrap();

        let bootstrap = LibraryWorkerProxyBootstrap {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            program: program.to_str().unwrap().to_string(),
            program_sha256: hash_file(&program).unwrap(),
            args: vec![entrypoint.to_str().unwrap().to_string()],
            working_directory: root.to_str().unwrap().to_string(),
            source_files: vec![
                LibraryWorkerProxySourceFile {
                    path: "worker.js".into(),
                    size: std::fs::metadata(&entrypoint).unwrap().len(),
                    sha256: hash_file(&entrypoint).unwrap(),
                },
                LibraryWorkerProxySourceFile {
                    path: "internal/helper.js".into(),
                    size: std::fs::metadata(&helper).unwrap().len(),
                    sha256: hash_file(&helper).unwrap(),
                },
            ],
            clear_environment: true,
            environment: BTreeMap::new(),
            direct_network_allowed: false,
            use_shell: false,
            startup_timeout_seconds: 30,
        };
        Fixture {
            _temp: temp,
            program,
            root,
            helper,
            bootstrap,
        }
    }

    #[test]
    fn container_reverifies_program_and_complete_source_tree() {
        let fixture = fixture();
        let verified = verify_library_worker_proxy(fixture.bootstrap).unwrap();
        assert_eq!(verified.program(), fixture.program.as_path());
        assert_eq!(verified.working_directory(), fixture.root.as_path());
        assert_eq!(verified.source_file_count(), 2);
        assert_eq!(verified.startup_timeout_seconds(), 30);
        verified.verify_before_spawn().unwrap();
    }

    #[test]
    fn source_mutation_after_container_verification_is_rejected() {
        let fixture = fixture();
        let verified = verify_library_worker_proxy(fixture.bootstrap).unwrap();
        std::fs::write(&fixture.helper, b"export const pwned = true;").unwrap();
        assert!(matches!(
            verified.verify_before_spawn(),
            Err(ContainerWorkerProxyError::SourceFileSizeMismatch { .. })
                | Err(ContainerWorkerProxyError::SourceFileHashMismatch { .. })
        ));
    }

    #[test]
    fn injected_source_file_is_rejected() {
        let fixture = fixture();
        let verified = verify_library_worker_proxy(fixture.bootstrap).unwrap();
        std::fs::write(fixture.root.join("injected.js"), b"pwned").unwrap();
        assert!(matches!(
            verified.verify_before_spawn(),
            Err(ContainerWorkerProxyError::UnexpectedSourceFile(path)) if path == "injected.js"
        ));
    }

    #[test]
    fn managed_program_replacement_is_rejected() {
        let fixture = fixture();
        let verified = verify_library_worker_proxy(fixture.bootstrap).unwrap();
        std::fs::write(&fixture.program, b"managed-bun-v2-malicious").unwrap();
        assert!(matches!(
            verified.verify_before_spawn(),
            Err(ContainerWorkerProxyError::ProgramHashMismatch { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_source_tree_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let target = fixture.root.join("target.js");
        std::fs::write(&target, b"target").unwrap();
        let link = fixture.root.join("link.js");
        symlink(&target, &link).unwrap();

        let error = verify_library_worker_proxy(fixture.bootstrap).unwrap_err();
        assert!(matches!(
            error,
            ContainerWorkerProxyError::SymlinkedPath(path) if path == link
        ));
    }

    #[test]
    fn authority_widening_contract_is_rejected_before_filesystem_proof() {
        let mut fixture = fixture();
        fixture.bootstrap.direct_network_allowed = true;
        assert!(matches!(
            verify_library_worker_proxy(fixture.bootstrap),
            Err(ContainerWorkerProxyError::Contract(
                LibraryWorkerProxyError::DirectNetworkForbidden
            ))
        ));
    }
}
