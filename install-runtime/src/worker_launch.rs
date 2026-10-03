use std::path::Path;

use rbe_install_executor::{
    PinnedManagedToolchain, VerifiedWorkerInvocation, WorkerLaunchError, WorkerLaunchPlan,
};
use rbe_install_request::SystemRuntimeKind;

use crate::{
    load_admitted_system_runtime, prepare_verified_worker_source, MaterializedWorkerSource,
    SystemRuntimeAdmissionError, VerifiedPackageWorkerIdentity, VerifiedRpxRootSnapshot,
    WorkerSourceError,
};

/// Exact, freshly-materialized inputs for one interpreted Library Host worker.
///
/// The source tree must be retained for the lifetime of the worker because the
/// sealed invocation points at files inside it. Backend owns that lifetime and
/// passes only `invocation()` across the Container worker-proxy boundary.
#[derive(Debug)]
pub struct PreparedLibraryWorkerLaunch {
    source: MaterializedWorkerSource,
    invocation: VerifiedWorkerInvocation,
}

impl PreparedLibraryWorkerLaunch {
    pub fn source(&self) -> &MaterializedWorkerSource {
        &self.source
    }

    pub fn invocation(&self) -> &VerifiedWorkerInvocation {
        &self.invocation
    }

    pub fn into_parts(self) -> (MaterializedWorkerSource, VerifiedWorkerInvocation) {
        (self.source, self.invocation)
    }
}

/// Join a verified RPX worker snapshot with the exact admitted managed runtime
/// and a fresh materialized package tree.
///
/// This is the final install-runtime preparation boundary before Backend turns
/// the opaque invocation into the strict Container Library Worker Proxy
/// bootstrap. It never searches PATH and it never redefines runtime identity
/// from whatever bytes happen to exist on disk.
pub fn prepare_verified_library_worker_launch(
    project_root: impl AsRef<Path>,
    snapshot: &VerifiedRpxRootSnapshot,
    source_root: impl AsRef<Path>,
) -> Result<PreparedLibraryWorkerLaunch, LibraryWorkerLaunchPreparationError> {
    let project_root = project_root.as_ref();
    let source_root = source_root.as_ref();
    let runtime = managed_system_runtime(&snapshot.worker)?;
    let admitted = load_admitted_system_runtime(project_root, runtime)?;

    if admitted.version != snapshot.worker.runtime_version {
        return Err(
            LibraryWorkerLaunchPreparationError::RuntimeVersionMismatch {
                package: snapshot.package.clone(),
                runtime: snapshot.worker.runtime_kind.clone(),
                locked: snapshot.worker.runtime_version.clone(),
                admitted: admitted.version,
            },
        );
    }

    let source_plan = prepare_verified_worker_source(project_root, snapshot, source_root)?;
    let source = source_plan.materialize(snapshot)?;

    let prepared = (|| {
        let toolchain = PinnedManagedToolchain::from_pins([(
            snapshot.worker.runtime_kind.clone(),
            admitted.executable,
            admitted.executable_sha256,
        )])?;
        let plan = WorkerLaunchPlan::managed_interpreter(
            snapshot.worker.runtime_kind.clone(),
            &toolchain,
            &source.source_root,
            &source.entrypoint,
            source.files.clone(),
        )?;
        let invocation = plan.verify_before_spawn()?;
        Ok::<_, LibraryWorkerLaunchPreparationError>(invocation)
    })();

    match prepared {
        Ok(invocation) => Ok(PreparedLibraryWorkerLaunch { source, invocation }),
        Err(error) => {
            cleanup_materialized_source(&source.source_root);
            Err(error)
        }
    }
}

fn managed_system_runtime(
    worker: &VerifiedPackageWorkerIdentity,
) -> Result<SystemRuntimeKind, LibraryWorkerLaunchPreparationError> {
    if !worker.runtime_managed {
        return Err(LibraryWorkerLaunchPreparationError::UnmanagedRuntime {
            package: worker.package.clone(),
            runtime: worker.runtime_kind.clone(),
        });
    }

    match worker.runtime_kind.as_str() {
        "bun" => Ok(SystemRuntimeKind::Bunjs),
        "node" => Ok(SystemRuntimeKind::Nodejs),
        "python" => Ok(SystemRuntimeKind::Python),
        "pypy" => Ok(SystemRuntimeKind::PyPy),
        "rust" => Err(
            LibraryWorkerLaunchPreparationError::CompiledWorkerRequired {
                package: worker.package.clone(),
                runtime: worker.runtime_kind.clone(),
            },
        ),
        runtime => Err(LibraryWorkerLaunchPreparationError::UnsupportedRuntime {
            package: worker.package.clone(),
            runtime: runtime.to_string(),
        }),
    }
}

fn cleanup_materialized_source(root: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return;
    };
    if !metadata.file_type().is_symlink() && metadata.is_dir() {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryWorkerLaunchPreparationError {
    #[error("package {package:?} declares unmanaged worker runtime {runtime:?}; Library Host requires an RBE-admitted managed runtime")]
    UnmanagedRuntime { package: String, runtime: String },
    #[error("package {package:?} uses compiled worker runtime {runtime:?}; a verified compiled-worker artifact contract is required before launch")]
    CompiledWorkerRequired { package: String, runtime: String },
    #[error("package {package:?} declares unsupported Library Host worker runtime {runtime:?}")]
    UnsupportedRuntime { package: String, runtime: String },
    #[error("package {package:?} locked worker runtime {runtime:?} at {locked:?}, but the admitted managed runtime is {admitted:?}")]
    RuntimeVersionMismatch {
        package: String,
        runtime: String,
        locked: String,
        admitted: String,
    },
    #[error(transparent)]
    RuntimeAdmission(#[from] SystemRuntimeAdmissionError),
    #[error(transparent)]
    WorkerSource(#[from] WorkerSourceError),
    #[error(transparent)]
    Toolchain(#[from] rbe_install_executor::PinnedToolchainError),
    #[error(transparent)]
    WorkerLaunch(#[from] WorkerLaunchError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(runtime: &str, managed: bool) -> VerifiedPackageWorkerIdentity {
        VerifiedPackageWorkerIdentity {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            sdk_language: runtime.into(),
            sdk_name: "@rbe/sdk".into(),
            sdk_version: "0.1.9".into(),
            runtime_kind: runtime.into(),
            runtime_version: "1.0.0".into(),
            runtime_entry: "worker.js".into(),
            runtime_managed: managed,
        }
    }

    #[test]
    fn interpreted_worker_runtime_maps_to_rbe_system_runtime() {
        assert_eq!(
            managed_system_runtime(&worker("bun", true)).unwrap(),
            SystemRuntimeKind::Bunjs
        );
        assert_eq!(
            managed_system_runtime(&worker("node", true)).unwrap(),
            SystemRuntimeKind::Nodejs
        );
        assert_eq!(
            managed_system_runtime(&worker("python", true)).unwrap(),
            SystemRuntimeKind::Python
        );
        assert_eq!(
            managed_system_runtime(&worker("pypy", true)).unwrap(),
            SystemRuntimeKind::PyPy
        );
    }

    #[test]
    fn unmanaged_worker_runtime_fails_closed() {
        assert!(matches!(
            managed_system_runtime(&worker("bun", false)),
            Err(LibraryWorkerLaunchPreparationError::UnmanagedRuntime { .. })
        ));
    }

    #[test]
    fn rust_worker_requires_compiled_artifact_contract() {
        assert!(matches!(
            managed_system_runtime(&worker("rust", true)),
            Err(LibraryWorkerLaunchPreparationError::CompiledWorkerRequired { .. })
        ));
    }

    #[test]
    fn unknown_worker_runtime_fails_closed() {
        assert!(matches!(
            managed_system_runtime(&worker("mystery", true)),
            Err(LibraryWorkerLaunchPreparationError::UnsupportedRuntime { .. })
        ));
    }

    #[test]
    fn failed_post_materialization_preparation_can_clean_fresh_tree() {
        let root =
            std::env::temp_dir().join(format!("rbe-worker-launch-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("worker.js"), b"worker").unwrap();
        cleanup_materialized_source(&root);
        assert!(!root.exists());
    }
}
