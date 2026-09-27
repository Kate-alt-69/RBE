use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_io::AtomicIo;
use rbe_install_orchestrator::{
    delta_install_session, ActivationGate, DeltaSessionError, InstallSession, InstallSessionPhase,
    SessionError, INSTALL_JOURNAL_FILE, INSTALL_LEASE_FILE,
};
use rbe_project_package::{
    ProjectCacheLayout, ProjectPackageError, ProjectPackageLock, ProjectPackageManifest,
    PROJECT_PACKAGE_MANIFEST,
};
use sha2::{Digest, Sha256};

use crate::NamedInstallTarget;

const MAX_INSTALL_JOURNAL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_STAGED_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallActivationProof {
    pub observed_artifact_sha256: String,
    pub build_id: Option<String>,
    pub gate: ActivationGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectActivationResult {
    pub session_id: String,
    pub activated_packages: usize,
    pub manifest_sha256: String,
    pub lock_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectInstallRecovery {
    Clean,
    RolledBackUncommitted,
    CompletedManifestPublication,
    CleanedCommittedJournal,
}

/// Activate one already-prepared root graph as a crash-recoverable project
/// transaction.
///
/// The caller supplies one proof for every package instance in `work_lock`.
/// Unrelated roots in the final target lock are intentionally not journaled or
/// re-prepared. The target manifest is staged first, the lock is atomically
/// replaced as the activation point, and the manifest is published second. A
/// durable journal lets boot recovery finish that second publication if the
/// process dies in between.
pub fn activate_project_target(
    project_root: &Path,
    target: &NamedInstallTarget,
    work_lock: &ProjectPackageLock,
    proofs: &BTreeMap<String, InstallActivationProof>,
) -> Result<ProjectActivationResult, ActivationRuntimeError> {
    target.manifest.validate()?;
    target.lock.validate()?;
    work_lock.validate()?;
    verify_target_rendering(target)?;

    let layout = ProjectCacheLayout::new(project_root);
    let atomic = AtomicIo::new();
    let _lease = acquire_install_lease(&layout)?;
    recover_project_activation_locked(&layout, &atomic)?;

    let session_id = new_session_id();
    let mut session = delta_install_session(
        session_id.clone(),
        target.manifest_sha256.clone(),
        &target.lock,
        work_lock,
    )?;

    for key in proofs.keys() {
        if !session.packages.contains_key(key) {
            return Err(ActivationRuntimeError::UnexpectedProof(key.clone()));
        }
    }
    for key in session.packages.keys() {
        if !proofs.contains_key(key) {
            return Err(ActivationRuntimeError::MissingProof(key.clone()));
        }
    }

    persist_session(&atomic, &session, &layout)?;
    let instances = session.packages.keys().cloned().collect::<Vec<_>>();
    for instance in instances {
        let proof = proofs
            .get(&instance)
            .ok_or_else(|| ActivationRuntimeError::MissingProof(instance.clone()))?;
        session.mark_artifact_verified(&instance, &proof.observed_artifact_sha256)?;
        persist_session(&atomic, &session, &layout)?;
        session.mark_prepared(&instance, proof.build_id.clone())?;
        persist_session(&atomic, &session, &layout)?;
        session.mark_attested(&instance, &proof.gate)?;
        persist_session(&atomic, &session, &layout)?;
        session.mark_ready(&instance)?;
        persist_session(&atomic, &session, &layout)?;
    }

    if session.phase != InstallSessionPhase::ReadyToCommit {
        return Err(ActivationRuntimeError::SessionNotReady);
    }

    let staged_manifest = staged_manifest_path(&layout);
    atomic.write_atomic(&staged_manifest, target.manifest_yaml.as_bytes())?;
    persist_session(&atomic, &session, &layout)?;

    // The lock is the activation point. AtomicIo guarantees readers see either
    // the previous complete lock or this complete target lock, never partial YAML.
    atomic.write_atomic(&layout.lock_path(), target.lock_yaml.as_bytes())?;

    // If this write fails after the lock swap, keep the journal + staged
    // manifest intact. Boot recovery will finish publication before package
    // linking reads the active lock.
    atomic.write_atomic(&layout.manifest_path(), target.manifest_yaml.as_bytes())?;
    remove_file_if_exists(&staged_manifest)?;

    session.mark_committed()?;
    persist_session(&atomic, &session, &layout)?;
    remove_file_if_exists(&journal_path(&layout))?;

    Ok(ProjectActivationResult {
        session_id,
        activated_packages: session.packages.len(),
        manifest_sha256: target.manifest_sha256.clone(),
        lock_sha256: target.lock_sha256.clone(),
    })
}

/// Recover a stale install journal before normal package boot reads the active
/// lock. This function is safe to call on every boot.
pub fn recover_project_activation(
    project_root: &Path,
) -> Result<ProjectInstallRecovery, ActivationRuntimeError> {
    let layout = ProjectCacheLayout::new(project_root);
    let atomic = AtomicIo::new();
    let _lease = acquire_install_lease(&layout)?;
    recover_project_activation_locked(&layout, &atomic)
}

fn recover_project_activation_locked(
    layout: &ProjectCacheLayout,
    atomic: &AtomicIo,
) -> Result<ProjectInstallRecovery, ActivationRuntimeError> {
    let journal = journal_path(layout);
    let Some(journal_json) = read_optional_bounded_text(&journal, MAX_INSTALL_JOURNAL_BYTES)?
    else {
        // A staged manifest without a journal can never be authoritative.
        remove_file_if_exists(&staged_manifest_path(layout))?;
        return Ok(ProjectInstallRecovery::Clean);
    };
    let session = InstallSession::parse_json(&journal_json)?;

    let current_lock = read_optional_project_lock(&layout.lock_path())?;
    let lock_is_target = current_lock
        .as_ref()
        .map(rbe_install_orchestrator::canonical_lock_sha256)
        .transpose()?
        .as_deref()
        == Some(session.target_lock_sha256.as_str());

    let staged_manifest = staged_manifest_path(layout);
    if !lock_is_target {
        remove_file_if_exists(&staged_manifest)?;
        remove_file_if_exists(&journal)?;
        cleanup_session_build_dir(layout, &session.session_id)?;
        return Ok(ProjectInstallRecovery::RolledBackUncommitted);
    }

    let current_manifest =
        read_optional_bounded_text(&layout.manifest_path(), MAX_STAGED_MANIFEST_BYTES)?;
    if current_manifest.as_deref().map(sha256_text).as_deref()
        == Some(session.manifest_sha256.as_str())
    {
        remove_file_if_exists(&staged_manifest)?;
        remove_file_if_exists(&journal)?;
        cleanup_session_build_dir(layout, &session.session_id)?;
        return Ok(ProjectInstallRecovery::CleanedCommittedJournal);
    }

    let staged = read_optional_bounded_text(&staged_manifest, MAX_STAGED_MANIFEST_BYTES)?
        .ok_or(ActivationRuntimeError::CommittedLockMissingStagedManifest)?;
    if sha256_text(&staged) != session.manifest_sha256 {
        return Err(ActivationRuntimeError::StagedManifestHashMismatch);
    }
    ProjectPackageManifest::parse_yaml(&staged)?;
    atomic.write_atomic(&layout.manifest_path(), staged.as_bytes())?;
    remove_file_if_exists(&staged_manifest)?;
    remove_file_if_exists(&journal)?;
    cleanup_session_build_dir(layout, &session.session_id)?;
    Ok(ProjectInstallRecovery::CompletedManifestPublication)
}

fn verify_target_rendering(target: &NamedInstallTarget) -> Result<(), ActivationRuntimeError> {
    if target.manifest.render_yaml()? != target.manifest_yaml
        || sha256_text(&target.manifest_yaml) != target.manifest_sha256
    {
        return Err(ActivationRuntimeError::TargetManifestChanged);
    }
    if target.lock.render_yaml()? != target.lock_yaml
        || sha256_text(&target.lock_yaml) != target.lock_sha256
    {
        return Err(ActivationRuntimeError::TargetLockChanged);
    }
    Ok(())
}

fn persist_session(
    atomic: &AtomicIo,
    session: &InstallSession,
    layout: &ProjectCacheLayout,
) -> Result<(), ActivationRuntimeError> {
    let plan = session.journal_write_plan(layout)?;
    atomic.write_atomic(&plan.final_path, plan.contents.as_bytes())?;
    Ok(())
}

fn acquire_install_lease(
    layout: &ProjectCacheLayout,
) -> Result<InstallLease, ActivationRuntimeError> {
    let root = install_state_root(layout);
    ensure_safe_directory(&root)?;
    let path = root.join(INSTALL_LEASE_FILE);
    reject_symlink_if_present(&path)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    fs2::FileExt::try_lock_exclusive(&file)
        .map_err(|_| ActivationRuntimeError::InstallAlreadyRunning)?;
    Ok(InstallLease { file })
}

struct InstallLease {
    file: File,
}

impl Drop for InstallLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn install_state_root(layout: &ProjectCacheLayout) -> PathBuf {
    layout.rbe_system_cache_root().join("install")
}

fn journal_path(layout: &ProjectCacheLayout) -> PathBuf {
    install_state_root(layout).join(INSTALL_JOURNAL_FILE)
}

fn staged_manifest_path(layout: &ProjectCacheLayout) -> PathBuf {
    layout
        .root()
        .join(format!("{PROJECT_PACKAGE_MANIFEST}.next"))
}

fn cleanup_session_build_dir(
    layout: &ProjectCacheLayout,
    session_id: &str,
) -> Result<(), ActivationRuntimeError> {
    let path = install_state_root(layout).join("build").join(session_id);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ActivationRuntimeError::UnsafeInstallState(path))
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path)?;
            Ok(())
        }
        Ok(_) => Err(ActivationRuntimeError::UnsafeInstallState(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_safe_directory(path: &Path) -> Result<(), ActivationRuntimeError> {
    reject_symlink_components(path)?;
    fs::create_dir_all(path)?;
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ActivationRuntimeError::UnsafeInstallState(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), ActivationRuntimeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ActivationRuntimeError::UnsafeInstallState(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> Result<(), ActivationRuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(
            ActivationRuntimeError::UnsafeInstallState(path.to_path_buf()),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_project_lock(
    path: &Path,
) -> Result<Option<ProjectPackageLock>, ActivationRuntimeError> {
    read_optional_bounded_text(path, MAX_STAGED_MANIFEST_BYTES)?
        .map(|text| ProjectPackageLock::parse_yaml(&text))
        .transpose()
        .map_err(Into::into)
}

fn read_optional_bounded_text(
    path: &Path,
    maximum_bytes: u64,
) -> Result<Option<String>, ActivationRuntimeError> {
    reject_symlink_components(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ActivationRuntimeError::UnsafeInstallState(
            path.to_path_buf(),
        ));
    }
    if metadata.len() > maximum_bytes {
        return Err(ActivationRuntimeError::InstallStateTooLarge {
            path: path.to_path_buf(),
            limit: maximum_bytes,
            observed: metadata.len(),
        });
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.by_ref()
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(ActivationRuntimeError::InstallStateTooLarge {
            path: path.to_path_buf(),
            limit: maximum_bytes,
            observed: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| ActivationRuntimeError::InstallStateNotUtf8(path.to_path_buf()))
}

fn remove_file_if_exists(path: &Path) -> Result<(), ActivationRuntimeError> {
    reject_symlink_if_present(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("install-{}-{nanos}", std::process::id())
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Debug, thiserror::Error)]
pub enum ActivationRuntimeError {
    #[error(transparent)]
    ProjectPackage(#[from] ProjectPackageError),
    #[error(transparent)]
    DeltaSession(#[from] DeltaSessionError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("another project install is already running")]
    InstallAlreadyRunning,
    #[error("activation proof is missing for package instance {0:?}")]
    MissingProof(String),
    #[error("activation proof was supplied for unrelated package instance {0:?}")]
    UnexpectedProof(String),
    #[error("install session did not reach ready-to-commit state")]
    SessionNotReady,
    #[error("target package manifest changed after install planning")]
    TargetManifestChanged,
    #[error("target package lock changed after install planning")]
    TargetLockChanged,
    #[error("active target lock was committed but the staged project manifest is missing")]
    CommittedLockMissingStagedManifest,
    #[error("staged project manifest does not match the install journal")]
    StagedManifestHashMismatch,
    #[error("unsafe install state path: {0}")]
    UnsafeInstallState(PathBuf),
    #[error("install state file {path} exceeds {limit} bytes (observed {observed})")]
    InstallStateTooLarge {
        path: PathBuf,
        limit: u64,
        observed: u64,
    },
    #[error("install state file is not UTF-8: {0}")]
    InstallStateNotUtf8(PathBuf),
    #[error("install activation I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_install_orchestrator::gate_activation;
    use rbe_package_attestation::{AttestationPolicy, PackageAttestationInput, ReportScope};
    use rbe_project_package::{LockedProjectPackage, PackageRequirement};

    fn locked() -> LockedProjectPackage {
        LockedProjectPackage {
            version: "1.0.0".into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: "a".repeat(64),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: None,
            sdk: None,
        }
    }

    fn target() -> (NamedInstallTarget, ProjectPackageLock) {
        let mut manifest = ProjectPackageManifest::default();
        manifest
            .packages
            .insert("demo".into(), PackageRequirement::Version("1.0.0".into()));
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert("demo".into(), locked());
        let manifest_yaml = manifest.render_yaml().unwrap();
        let lock_yaml = lock.render_yaml().unwrap();
        (
            NamedInstallTarget {
                manifest,
                lock: lock.clone(),
                manifest_sha256: sha256_text(&manifest_yaml),
                lock_sha256: sha256_text(&lock_yaml),
                manifest_yaml,
                lock_yaml,
            },
            lock,
        )
    }

    fn proof() -> InstallActivationProof {
        let input = PackageAttestationInput {
            package: "demo".into(),
            version: "1.0.0".into(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: "a".repeat(64),
            downloaded_artifact_sha256: "a".repeat(64),
            local_source_sha256: None,
            remote_source_sha256: None,
            publisher_signature_declared: false,
            publisher_signature_verified: false,
            reproducible_build: false,
            shipped_binary_sha256: None,
            rebuilt_binary_sha256: None,
            build_id: "no-build".into(),
            host: "test-host".into(),
        };
        InstallActivationProof {
            observed_artifact_sha256: "a".repeat(64),
            build_id: None,
            gate: gate_activation(&input, AttestationPolicy::default()).unwrap(),
        }
    }

    #[test]
    fn activation_commits_manifest_and_lock_and_cleans_journal() {
        let temp = tempfile::tempdir().unwrap();
        let (target, work) = target();
        let proofs = BTreeMap::from([("demo".into(), proof())]);

        let result = activate_project_target(temp.path(), &target, &work, &proofs).unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        assert_eq!(
            fs::read_to_string(layout.manifest_path()).unwrap(),
            target.manifest_yaml
        );
        assert_eq!(
            fs::read_to_string(layout.lock_path()).unwrap(),
            target.lock_yaml
        );
        assert!(!journal_path(&layout).exists());
        assert!(!staged_manifest_path(&layout).exists());
        assert_eq!(result.activated_packages, 1);
    }

    #[test]
    fn recovery_completes_manifest_after_lock_activation() {
        let temp = tempfile::tempdir().unwrap();
        let (target, work) = target();
        let layout = ProjectCacheLayout::new(temp.path());
        let atomic = AtomicIo::new();
        let mut session = delta_install_session(
            "recover-1",
            target.manifest_sha256.clone(),
            &target.lock,
            &work,
        )
        .unwrap();
        let proof = proof();
        session
            .mark_artifact_verified("demo", &proof.observed_artifact_sha256)
            .unwrap();
        session.mark_prepared("demo", None).unwrap();
        session.mark_attested("demo", &proof.gate).unwrap();
        session.mark_ready("demo").unwrap();
        persist_session(&atomic, &session, &layout).unwrap();
        atomic
            .write_atomic(
                &staged_manifest_path(&layout),
                target.manifest_yaml.as_bytes(),
            )
            .unwrap();
        atomic
            .write_atomic(&layout.lock_path(), target.lock_yaml.as_bytes())
            .unwrap();

        let recovered = recover_project_activation(temp.path()).unwrap();
        assert_eq!(
            recovered,
            ProjectInstallRecovery::CompletedManifestPublication
        );
        assert_eq!(
            fs::read_to_string(layout.manifest_path()).unwrap(),
            target.manifest_yaml
        );
        assert!(!journal_path(&layout).exists());
    }

    #[test]
    fn recovery_rolls_back_uncommitted_manifest_stage() {
        let temp = tempfile::tempdir().unwrap();
        let (target, work) = target();
        let layout = ProjectCacheLayout::new(temp.path());
        let atomic = AtomicIo::new();
        let session = delta_install_session(
            "recover-2",
            target.manifest_sha256.clone(),
            &target.lock,
            &work,
        )
        .unwrap();
        persist_session(&atomic, &session, &layout).unwrap();
        atomic
            .write_atomic(
                &staged_manifest_path(&layout),
                target.manifest_yaml.as_bytes(),
            )
            .unwrap();

        let recovered = recover_project_activation(temp.path()).unwrap();
        assert_eq!(recovered, ProjectInstallRecovery::RolledBackUncommitted);
        assert!(!staged_manifest_path(&layout).exists());
        assert!(!journal_path(&layout).exists());
        assert!(!layout.lock_path().exists());
    }

    #[test]
    fn activation_requires_exact_proof_set() {
        let temp = tempfile::tempdir().unwrap();
        let (target, work) = target();
        let error =
            activate_project_target(temp.path(), &target, &work, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, ActivationRuntimeError::MissingProof(name) if name == "demo"));
    }
}
