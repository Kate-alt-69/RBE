use std::collections::BTreeMap;

use rbe_project_package::{ProjectPackageError, ProjectPackageLock};

use crate::{
    canonical_lock_sha256, InstallSession, InstallSessionPhase, SessionError, SessionPackage,
    SessionPackageState, INSTALL_SESSION_FORMAT,
};

/// Construct an install journal for only the package instances that were
/// prepared by this install while still pinning the hash of the entire target
/// project lock.
///
/// Incremental installs must not force unrelated, already-active roots through
/// verification/build again. `work_lock` is therefore the exact root-scoped
/// graph being changed; every instance in it must exist byte-for-byte in
/// `target_lock` before the session is accepted.
pub fn delta_install_session(
    session_id: impl Into<String>,
    manifest_sha256: impl Into<String>,
    target_lock: &ProjectPackageLock,
    work_lock: &ProjectPackageLock,
) -> Result<InstallSession, DeltaSessionError> {
    target_lock.validate()?;
    work_lock.validate()?;

    if target_lock.packages.is_empty() {
        return Err(DeltaSessionError::EmptyTargetLock);
    }
    if work_lock.packages.is_empty() {
        return Err(DeltaSessionError::EmptyWorkGraph);
    }

    for root in target_lock.packages.keys() {
        if !target_lock.root_graph_complete(root) {
            return Err(DeltaSessionError::IncompleteTargetRoot(root.clone()));
        }
    }
    for root in work_lock.packages.keys() {
        if !work_lock.root_graph_complete(root) {
            return Err(DeltaSessionError::IncompleteWorkRoot(root.clone()));
        }
    }

    let mut packages = BTreeMap::new();
    for instance in work_lock.instances() {
        let target = target_lock
            .locked_for_root(instance.root, instance.package)
            .ok_or_else(|| DeltaSessionError::WorkInstanceMissing {
                root: instance.root.to_string(),
                package: instance.package.to_string(),
            })?;
        if target != instance.locked {
            return Err(DeltaSessionError::WorkInstanceMismatch {
                root: instance.root.to_string(),
                package: instance.package.to_string(),
            });
        }

        let key = if instance.is_root {
            instance.package.to_string()
        } else {
            InstallSession::private_instance_id(instance.root, instance.package)?
        };
        if packages
            .insert(
                key.clone(),
                SessionPackage {
                    root: instance.root.to_string(),
                    package: instance.package.to_string(),
                    private: !instance.is_root,
                    version: instance.locked.version.clone(),
                    artifact_sha256: instance.locked.artifact_sha256.to_ascii_lowercase(),
                    state: SessionPackageState::Pending,
                    build_id: None,
                },
            )
            .is_some()
        {
            return Err(DeltaSessionError::DuplicateWorkInstance(key));
        }
    }

    let session = InstallSession {
        format: INSTALL_SESSION_FORMAT,
        session_id: session_id.into(),
        manifest_sha256: manifest_sha256.into().to_ascii_lowercase(),
        target_lock_sha256: canonical_lock_sha256(target_lock)?,
        phase: InstallSessionPhase::Preparing,
        packages,
    };
    session.validate()?;
    Ok(session)
}

#[derive(Debug, thiserror::Error)]
pub enum DeltaSessionError {
    #[error(transparent)]
    ProjectPackage(#[from] ProjectPackageError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("target project lock cannot be empty")]
    EmptyTargetLock,
    #[error("incremental install work graph cannot be empty")]
    EmptyWorkGraph,
    #[error("target project lock has an incomplete private graph for root {0:?}")]
    IncompleteTargetRoot(String),
    #[error("incremental work graph is incomplete for root {0:?}")]
    IncompleteWorkRoot(String),
    #[error("work package {package:?} for root {root:?} is absent from the target lock")]
    WorkInstanceMissing { root: String, package: String },
    #[error("work package {package:?} for root {root:?} differs from the target lock")]
    WorkInstanceMismatch { root: String, package: String },
    #[error("duplicate work package instance {0:?}")]
    DuplicateWorkInstance(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_project_package::LockedProjectPackage;

    fn locked(version: &str, artifact: char) -> LockedProjectPackage {
        LockedProjectPackage {
            version: version.into(),
            resolved_from: "registry:test".into(),
            artifact_url: "https://example.com/test.rbe".into(),
            artifact_sha256: artifact.to_string().repeat(64),
            manifest_sha256: "f".repeat(64),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: None,
            sdk: None,
        }
    }

    #[test]
    fn delta_session_tracks_only_changed_graph_but_pins_full_lock() {
        let mut target = ProjectPackageLock::default();
        target.packages.insert("alpha".into(), locked("1.0.0", 'a'));
        target.packages.insert("beta".into(), locked("2.0.0", 'b'));

        let mut work = ProjectPackageLock::default();
        work.packages.insert("beta".into(), locked("2.0.0", 'b'));

        let session = delta_install_session("delta-1", "c".repeat(64), &target, &work).unwrap();

        assert_eq!(session.packages.len(), 1);
        assert!(session.packages.contains_key("beta"));
        assert!(!session.packages.contains_key("alpha"));
        assert_eq!(
            session.target_lock_sha256,
            canonical_lock_sha256(&target).unwrap()
        );
    }

    #[test]
    fn delta_session_rejects_work_graph_drift_from_target() {
        let mut target = ProjectPackageLock::default();
        target.packages.insert("beta".into(), locked("2.0.0", 'b'));

        let mut work = ProjectPackageLock::default();
        work.packages.insert("beta".into(), locked("2.1.0", 'c'));

        assert!(matches!(
            delta_install_session("delta-2", "d".repeat(64), &target, &work),
            Err(DeltaSessionError::WorkInstanceMismatch { .. })
        ));
    }
}
