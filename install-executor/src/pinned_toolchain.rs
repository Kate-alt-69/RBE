use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{ExecutorError, ManagedToolchain};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedManagedTool {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedManagedToolchain {
    pub tools: BTreeMap<String, PinnedManagedTool>,
}

impl PinnedManagedToolchain {
    /// Snapshot the exact bytes behind an already-validated managed toolchain.
    ///
    /// Paths are not authority by themselves: each tool is hashed at admission
    /// and must be re-verified immediately before a caller turns the pinned
    /// toolchain back into executable build/worker inputs.
    pub fn pin(toolchain: &ManagedToolchain) -> Result<Self, PinnedToolchainError> {
        let mut tools = BTreeMap::new();
        for (name, path) in &toolchain.tools {
            tools.insert(
                name.clone(),
                PinnedManagedTool {
                    path: path.clone(),
                    sha256: hash_regular_file(path)?,
                },
            );
        }
        if tools.is_empty() {
            return Err(PinnedToolchainError::EmptyToolchain);
        }
        Ok(Self { tools })
    }

    /// Re-hash one pinned tool and return its exact path only when the bytes
    /// still match the identity captured at admission time.
    pub fn verify_tool(&self, name: &str) -> Result<&Path, PinnedToolchainError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| PinnedToolchainError::UnknownTool(name.to_string()))?;
        let actual = hash_regular_file(&tool.path)?;
        if actual != tool.sha256 {
            return Err(PinnedToolchainError::ToolHashMismatch {
                tool: name.to_string(),
                path: tool.path.clone(),
                expected: tool.sha256.clone(),
                actual,
            });
        }
        Ok(&tool.path)
    }

    /// Re-verify every pinned tool and reconstruct the path-only execution
    /// contract expected by the existing invocation planner.
    ///
    /// The returned `ManagedToolchain` is intentionally short-lived: callers
    /// should create it immediately before deriving process invocations.
    pub fn verify_all(&self) -> Result<ManagedToolchain, PinnedToolchainError> {
        let mut tools = BTreeMap::new();
        for name in self.tools.keys() {
            tools.insert(name.clone(), self.verify_tool(name)?.to_path_buf());
        }
        ManagedToolchain::new(tools).map_err(PinnedToolchainError::Executor)
    }
}

fn hash_regular_file(path: &Path) -> Result<String, PinnedToolchainError> {
    if !path.is_absolute() {
        return Err(PinnedToolchainError::ToolPathMustBeAbsolute(
            path.to_path_buf(),
        ));
    }
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PinnedToolchainError::UnsafeToolFile(path.to_path_buf()));
    }

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

fn ensure_no_symlink_components(path: &Path) -> Result<(), PinnedToolchainError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PinnedToolchainError::SymlinkedToolPath(current));
            }
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PinnedToolchainError {
    #[error("managed toolchain must contain at least one tool")]
    EmptyToolchain,
    #[error("unknown pinned managed tool {0:?}")]
    UnknownTool(String),
    #[error("managed tool path must be absolute: {0}")]
    ToolPathMustBeAbsolute(PathBuf),
    #[error("managed tool path traverses a symbolic link: {0}")]
    SymlinkedToolPath(PathBuf),
    #[error("managed tool is not a regular non-symlink file: {0}")]
    UnsafeToolFile(PathBuf),
    #[error(
        "managed tool {tool:?} changed after admission at {path}: expected SHA-256 {expected}, got {actual}"
    )]
    ToolHashMismatch {
        tool: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Executor(#[from] ExecutorError),
    #[error("managed tool verification I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

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
                "rbe-pinned-toolchain-test-{}-{nonce}-{}",
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

    fn managed_toolchain(path: PathBuf) -> ManagedToolchain {
        ManagedToolchain::new(BTreeMap::from([("runtime".to_string(), path)])).unwrap()
    }

    #[test]
    fn pinned_toolchain_reverifies_exact_tool_bytes() {
        let temp = TestDir::new();
        let tool = temp.path().join("runtime.bin");
        std::fs::write(&tool, b"verified-runtime-v1").unwrap();
        let pinned = PinnedManagedToolchain::pin(&managed_toolchain(tool.clone())).unwrap();

        assert_eq!(pinned.verify_tool("runtime").unwrap(), tool.as_path());
        let verified = pinned.verify_all().unwrap();
        assert_eq!(verified.tools.get("runtime"), Some(&tool));
    }

    #[test]
    fn replaced_tool_is_rejected_before_execution_planning() {
        let temp = TestDir::new();
        let tool = temp.path().join("runtime.bin");
        std::fs::write(&tool, b"verified-runtime-v1").unwrap();
        let pinned = PinnedManagedToolchain::pin(&managed_toolchain(tool.clone())).unwrap();

        std::fs::write(&tool, b"malicious-runtime-v2").unwrap();
        let error = pinned.verify_all().unwrap_err();
        assert!(matches!(
            error,
            PinnedToolchainError::ToolHashMismatch { tool, .. } if tool == "runtime"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_tool_path_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let real = temp.path().join("real.bin");
        let link = temp.path().join("runtime.bin");
        std::fs::write(&real, b"runtime").unwrap();
        symlink(&real, &link).unwrap();

        let error = PinnedManagedToolchain::pin(&managed_toolchain(link)).unwrap_err();
        assert!(matches!(error, PinnedToolchainError::SymlinkedToolPath(_)));
    }
}
