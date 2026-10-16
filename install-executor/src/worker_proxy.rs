use std::ffi::OsStr;
use std::path::Path;

use ipc_protocol::{
    LibraryWorkerProxyBootstrap, LibraryWorkerProxyError, LibraryWorkerProxySourceFile,
    LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
};

use crate::VerifiedWorkerInvocation;

/// Convert one sealed worker invocation into the strict Backend -> Container
/// proxy bootstrap without widening any of the verified launch properties.
pub fn library_worker_proxy_bootstrap(
    invocation: &VerifiedWorkerInvocation,
) -> Result<LibraryWorkerProxyBootstrap, WorkerProxyBridgeError> {
    if !invocation.clear_environment() || !invocation.environment().is_empty() {
        return Err(WorkerProxyBridgeError::UnsafeInvocation(
            "verified worker invocation must start with an empty environment",
        ));
    }
    if invocation.direct_network_allowed() {
        return Err(WorkerProxyBridgeError::UnsafeInvocation(
            "verified worker invocation must not allow direct networking",
        ));
    }
    if invocation.use_shell() {
        return Err(WorkerProxyBridgeError::UnsafeInvocation(
            "verified worker invocation must not use a shell",
        ));
    }

    let bootstrap = LibraryWorkerProxyBootstrap {
        protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
        program: utf8_path("program", invocation.program())?,
        program_sha256: invocation.program_sha256().to_string(),
        args: invocation
            .args()
            .iter()
            .enumerate()
            .map(|(index, value)| utf8_os(&format!("arg[{index}]"), value))
            .collect::<Result<Vec<_>, _>>()?,
        working_directory: utf8_path("working_directory", invocation.working_directory())?,
        source_files: invocation
            .source_files()
            .iter()
            .map(|file| LibraryWorkerProxySourceFile {
                path: file.path.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            })
            .collect(),
        clear_environment: invocation.clear_environment(),
        environment: invocation.environment().clone(),
        direct_network_allowed: invocation.direct_network_allowed(),
        use_shell: invocation.use_shell(),
        startup_timeout_seconds: invocation.startup_timeout_seconds(),
    };
    bootstrap.validate()?;
    Ok(bootstrap)
}

fn utf8_path(field: &str, value: &Path) -> Result<String, WorkerProxyBridgeError> {
    utf8_os(field, value.as_os_str())
}

fn utf8_os(field: &str, value: &OsStr) -> Result<String, WorkerProxyBridgeError> {
    value
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| WorkerProxyBridgeError::NonUtf8Field(field.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerProxyBridgeError {
    #[error("verified worker invocation field {0:?} is not valid UTF-8")]
    NonUtf8Field(String),
    #[error("unsafe verified worker invocation: {0}")]
    UnsafeInvocation(&'static str),
    #[error(transparent)]
    ProxyContract(#[from] LibraryWorkerProxyError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{ManagedToolchain, PinnedManagedToolchain, SourceFileHasher, WorkerLaunchPlan};

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
                "rbe-worker-proxy-test-{}-{nonce}-{}",
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

    fn digest(path: &str, bytes: &[u8]) -> crate::SourceFileDigest {
        let mut hasher = SourceFileHasher::new(path, bytes.len() as u64).unwrap();
        hasher.update(bytes).unwrap();
        hasher.finish().unwrap()
    }

    #[test]
    fn sealed_invocation_becomes_strict_container_proxy_bootstrap() {
        let temp = TestDir::new();
        let runtime = temp.path().join("bun");
        std::fs::write(&runtime, b"managed-bun-v1").unwrap();
        let managed =
            ManagedToolchain::new(BTreeMap::from([("bun".to_string(), runtime.clone())])).unwrap();
        let pinned = PinnedManagedToolchain::pin(&managed).unwrap();

        let source_root = temp.path().join("worker");
        std::fs::create_dir_all(&source_root).unwrap();
        let entrypoint = source_root.join("worker.js");
        let worker_bytes = b"export const ok = true;";
        std::fs::write(&entrypoint, worker_bytes).unwrap();
        let source_files = vec![digest("worker.js", worker_bytes)];

        let plan = WorkerLaunchPlan::managed_interpreter(
            "bun",
            &pinned,
            &source_root,
            &entrypoint,
            source_files.clone(),
        )
        .unwrap();
        let invocation = plan.verify_before_spawn().unwrap();
        let bootstrap = library_worker_proxy_bootstrap(&invocation).unwrap();

        assert_eq!(bootstrap.program, runtime.to_str().unwrap());
        assert_eq!(bootstrap.program_sha256, invocation.program_sha256());
        assert_eq!(
            bootstrap.args,
            vec![entrypoint.to_str().unwrap().to_string()]
        );
        assert_eq!(
            bootstrap.working_directory,
            source_root.to_str().unwrap().to_string()
        );
        assert_eq!(bootstrap.source_files.len(), 1);
        assert_eq!(bootstrap.source_files[0].path, source_files[0].path);
        assert_eq!(bootstrap.source_files[0].size, source_files[0].size);
        assert_eq!(bootstrap.source_files[0].sha256, source_files[0].sha256);
        assert!(bootstrap.clear_environment);
        assert!(bootstrap.environment.is_empty());
        assert!(!bootstrap.direct_network_allowed);
        assert!(!bootstrap.use_shell);
        bootstrap.validate().unwrap();
    }
}
