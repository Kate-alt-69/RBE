//! Durable project-wide install session and atomic lock activation planning.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rbe_project_package::{
    ProjectCacheLayout, ProjectPackageError, ProjectPackageLock, PROJECT_PACKAGE_LOCK,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ActivationGate;

pub const INSTALL_SESSION_FORMAT: u32 = 1;
pub const INSTALL_JOURNAL_FILE: &str = "journal.rbe.json";
pub const INSTALL_LEASE_FILE: &str = "install.lease";
const PRIVATE_SCOPE_SEPARATOR: &str = "::";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallSessionPhase {
    Preparing,
    ReadyToCommit,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPackageState {
    Pending,
    ArtifactVerified,
    Prepared,
    Attested,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPackage {
    pub root: String,
    pub package: String,
    pub private: bool,
    pub version: String,
    pub artifact_sha256: String,
    pub state: SessionPackageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSession {
    pub format: u32,
    pub session_id: String,
    pub manifest_sha256: String,
    pub target_lock_sha256: String,
    pub phase: InstallSessionPhase,
    /// Root instances keep their ordinary package name as the key. Private
    /// instances use `<root>::<package>` only inside the install journal.
    pub packages: BTreeMap<String, SessionPackage>,
}

impl InstallSession {
    pub fn new(
        session_id: impl Into<String>,
        manifest_sha256: impl Into<String>,
        target_lock: &ProjectPackageLock,
    ) -> Result<Self, SessionError> {
        target_lock.validate()?;
        if target_lock.packages.is_empty() {
            return Err(SessionError::EmptySession);
        }
        for root in target_lock.packages.keys() {
            if !target_lock.root_graph_complete(root) {
                return Err(SessionError::IncompletePrivateGraph(root.clone()));
            }
        }

        let session_id = session_id.into();
        validate_session_id(&session_id)?;
        let manifest_sha256 = canonical_sha256(&manifest_sha256.into())?;
        let target_lock_sha256 = canonical_lock_sha256(target_lock)?;
        let mut packages = BTreeMap::new();
        for instance in target_lock.instances() {
            let key = instance_key(instance.root, instance.package, !instance.is_root)?;
            let previous = packages.insert(
                key.clone(),
                SessionPackage {
                    root: instance.root.to_string(),
                    package: instance.package.to_string(),
                    private: !instance.is_root,
                    version: instance.locked.version.clone(),
                    artifact_sha256: canonical_sha256(&instance.locked.artifact_sha256)?,
                    state: SessionPackageState::Pending,
                    build_id: None,
                },
            );
            if previous.is_some() {
                return Err(SessionError::DuplicateInstance(key));
            }
        }

        Ok(Self {
            format: INSTALL_SESSION_FORMAT,
            session_id,
            manifest_sha256,
            target_lock_sha256,
            phase: InstallSessionPhase::Preparing,
            packages,
        })
    }

    pub fn parse_json(input: &str) -> Result<Self, SessionError> {
        let session: Self = serde_json::from_str(input)?;
        session.validate()?;
        Ok(session)
    }

    pub fn render_json(&self) -> Result<String, SessionError> {
        self.validate()?;
        let mut output = serde_json::to_string_pretty(self)?;
        output.push('\n');
        Ok(output)
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        if self.format != INSTALL_SESSION_FORMAT {
            return Err(SessionError::UnsupportedFormat(self.format));
        }
        validate_session_id(&self.session_id)?;
        canonical_sha256(&self.manifest_sha256)?;
        canonical_sha256(&self.target_lock_sha256)?;
        if self.packages.is_empty() {
            return Err(SessionError::EmptySession);
        }

        let mut all_ready = true;
        for (key, package) in &self.packages {
            validate_package_name(&package.root)?;
            validate_package_name(&package.package)?;
            let expected_key = instance_key(&package.root, &package.package, package.private)?;
            if key != &expected_key {
                return Err(SessionError::InvalidInstanceKey {
                    expected: expected_key,
                    actual: key.clone(),
                });
            }
            if !package.private && package.root != package.package {
                return Err(SessionError::InvalidRootInstance(key.clone()));
            }
            validate_text("package version", &package.version)?;
            canonical_sha256(&package.artifact_sha256)?;
            if let Some(build_id) = &package.build_id {
                validate_text("build id", build_id)?;
            }
            if matches!(
                package.state,
                SessionPackageState::Pending | SessionPackageState::ArtifactVerified
            ) && package.build_id.is_some()
            {
                return Err(SessionError::InvalidJournalState(key.clone()));
            }
            all_ready &= package.state == SessionPackageState::Ready;
        }

        match self.phase {
            InstallSessionPhase::Preparing if all_ready => {
                return Err(SessionError::InvalidPhaseState)
            }
            InstallSessionPhase::ReadyToCommit | InstallSessionPhase::Committed if !all_ready => {
                return Err(SessionError::InvalidPhaseState)
            }
            _ => {}
        }
        Ok(())
    }

    pub fn private_instance_id(root: &str, package: &str) -> Result<String, SessionError> {
        instance_key(root, package, true)
    }

    pub fn mark_artifact_verified(
        &mut self,
        instance: &str,
        observed_sha256: &str,
    ) -> Result<(), SessionError> {
        let observed = canonical_sha256(observed_sha256)?;
        let entry = self.package_mut(instance)?;
        require_state(instance, entry.state, SessionPackageState::Pending)?;
        if observed != entry.artifact_sha256 {
            return Err(SessionError::ArtifactHashMismatch {
                package: instance.to_string(),
                expected: entry.artifact_sha256.clone(),
                actual: observed,
            });
        }
        entry.state = SessionPackageState::ArtifactVerified;
        Ok(())
    }

    /// Marks source/build preparation complete. `None` means the package had no
    /// build step; otherwise the build identifier is persisted for diagnostics.
    pub fn mark_prepared(
        &mut self,
        instance: &str,
        build_id: Option<String>,
    ) -> Result<(), SessionError> {
        if let Some(build_id) = &build_id {
            validate_text("build id", build_id)?;
        }
        let entry = self.package_mut(instance)?;
        require_state(instance, entry.state, SessionPackageState::ArtifactVerified)?;
        entry.build_id = build_id;
        entry.state = SessionPackageState::Prepared;
        Ok(())
    }

    pub fn mark_attested(
        &mut self,
        instance: &str,
        gate: &ActivationGate,
    ) -> Result<(), SessionError> {
        if !gate.can_activate() {
            return Err(SessionError::PackageQuarantined(instance.to_string()));
        }
        let entry = self.package_mut(instance)?;
        require_state(instance, entry.state, SessionPackageState::Prepared)?;
        entry.state = SessionPackageState::Attested;
        Ok(())
    }

    pub fn mark_ready(&mut self, instance: &str) -> Result<(), SessionError> {
        let entry = self.package_mut(instance)?;
        require_state(instance, entry.state, SessionPackageState::Attested)?;
        entry.state = SessionPackageState::Ready;
        self.refresh_phase();
        Ok(())
    }

    pub fn mark_committed(&mut self) -> Result<(), SessionError> {
        if self.phase != InstallSessionPhase::ReadyToCommit {
            return Err(SessionError::SessionNotReady);
        }
        self.phase = InstallSessionPhase::Committed;
        Ok(())
    }

    pub fn journal_write_plan(
        &self,
        layout: &ProjectCacheLayout,
    ) -> Result<JournalWritePlan, SessionError> {
        let install_root = install_state_root(layout);
        Ok(JournalWritePlan {
            contents: self.render_json()?,
            temporary_path: install_root.join(format!("{INSTALL_JOURNAL_FILE}.next")),
            final_path: install_root.join(INSTALL_JOURNAL_FILE),
            fsync_before_publish: true,
            atomic_replace: true,
            fsync_parent_after_publish: true,
        })
    }

    pub fn lease_plan(&self, layout: &ProjectCacheLayout) -> ProjectInstallLeasePlan {
        ProjectInstallLeasePlan {
            path: install_state_root(layout).join(INSTALL_LEASE_FILE),
            exclusive: true,
            stale_recovery_requires_journal_check: true,
        }
    }

    pub fn lock_commit_plan(
        &self,
        target_lock: &ProjectPackageLock,
        layout: &ProjectCacheLayout,
    ) -> Result<LockCommitPlan, SessionError> {
        if self.phase != InstallSessionPhase::ReadyToCommit {
            return Err(SessionError::SessionNotReady);
        }

        let contents = target_lock.render_yaml()?;
        let actual = sha256_text(&contents);
        if actual != self.target_lock_sha256 {
            return Err(SessionError::TargetLockChanged {
                expected: self.target_lock_sha256.clone(),
                actual,
            });
        }

        Ok(LockCommitPlan {
            contents,
            temporary_path: layout.root().join(format!("{PROJECT_PACKAGE_LOCK}.next")),
            final_path: layout.lock_path(),
            fsync_before_publish: true,
            atomic_replace: true,
            fsync_parent_after_publish: true,
            activation_is_lock_swap: true,
        })
    }

    pub fn recovery_plan(
        &self,
        current_lock: Option<&ProjectPackageLock>,
        layout: &ProjectCacheLayout,
    ) -> Result<RecoveryPlan, SessionError> {
        self.validate()?;
        let current_lock_sha256 = current_lock.map(canonical_lock_sha256).transpose()?;
        let disposition = if current_lock_sha256.as_deref() == Some(&self.target_lock_sha256) {
            RecoveryDisposition::AlreadyCommitted
        } else {
            RecoveryDisposition::RollbackUncommitted
        };

        let mut remove_paths = self
            .packages
            .values()
            .map(|package| {
                layout
                    .library_cache_root()
                    .join(".staging")
                    .join(&package.artifact_sha256)
            })
            .collect::<Vec<_>>();
        remove_paths.sort();
        remove_paths.dedup();
        remove_paths.push(
            install_state_root(layout)
                .join("build")
                .join(&self.session_id),
        );

        Ok(RecoveryPlan {
            disposition,
            remove_paths,
            preserve_library_cache_root: layout.library_cache_root(),
            preserve_current_lock: true,
            journal_path: install_state_root(layout).join(INSTALL_JOURNAL_FILE),
            lease_path: install_state_root(layout).join(INSTALL_LEASE_FILE),
            delete_journal_after_cleanup: true,
            release_lease_after_cleanup: true,
        })
    }

    fn package_mut(&mut self, instance: &str) -> Result<&mut SessionPackage, SessionError> {
        if self.phase != InstallSessionPhase::Preparing {
            return Err(SessionError::SessionNotPreparing);
        }
        self.packages
            .get_mut(instance)
            .ok_or_else(|| SessionError::UnknownPackage(instance.to_string()))
    }

    fn refresh_phase(&mut self) {
        if self
            .packages
            .values()
            .all(|package| package.state == SessionPackageState::Ready)
        {
            self.phase = InstallSessionPhase::ReadyToCommit;
        }
    }
}

pub fn canonical_lock_sha256(lock: &ProjectPackageLock) -> Result<String, SessionError> {
    Ok(sha256_text(&lock.render_yaml()?))
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn install_state_root(layout: &ProjectCacheLayout) -> PathBuf {
    layout.rbe_system_cache_root().join("install")
}

fn instance_key(root: &str, package: &str, private: bool) -> Result<String, SessionError> {
    validate_package_name(root)?;
    validate_package_name(package)?;
    if private {
        if root == package {
            return Err(SessionError::InvalidPrivateInstance {
                root: root.to_string(),
                package: package.to_string(),
            });
        }
        Ok(format!("{root}{PRIVATE_SCOPE_SEPARATOR}{package}"))
    } else {
        if root != package {
            return Err(SessionError::InvalidRootInstance(package.to_string()));
        }
        Ok(package.to_string())
    }
}

fn require_state(
    package: &str,
    actual: SessionPackageState,
    expected: SessionPackageState,
) -> Result<(), SessionError> {
    if actual != expected {
        return Err(SessionError::InvalidPackageTransition {
            package: package.to_string(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn canonical_sha256(value: &str) -> Result<String, SessionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SessionError::InvalidSha256(value.to_string()));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_session_id(value: &str) -> Result<(), SessionError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SessionError::InvalidSessionId(value.to_string()));
    }
    Ok(())
}

fn validate_package_name(value: &str) -> Result<(), SessionError> {
    if value.is_empty()
        || value.len() > 192
        || value.split('.').any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
    {
        return Err(SessionError::InvalidPackageName(value.to_string()));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str) -> Result<(), SessionError> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(SessionError::InvalidText(field));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalWritePlan {
    pub contents: String,
    pub temporary_path: PathBuf,
    pub final_path: PathBuf,
    pub fsync_before_publish: bool,
    pub atomic_replace: bool,
    pub fsync_parent_after_publish: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstallLeasePlan {
    pub path: PathBuf,
    pub exclusive: bool,
    pub stale_recovery_requires_journal_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCommitPlan {
    pub contents: String,
    pub temporary_path: PathBuf,
    pub final_path: PathBuf,
    pub fsync_before_publish: bool,
    pub atomic_replace: bool,
    pub fsync_parent_after_publish: bool,
    pub activation_is_lock_swap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDisposition {
    RollbackUncommitted,
    AlreadyCommitted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub disposition: RecoveryDisposition,
    pub remove_paths: Vec<PathBuf>,
    pub preserve_library_cache_root: PathBuf,
    pub preserve_current_lock: bool,
    pub journal_path: PathBuf,
    pub lease_path: PathBuf,
    pub delete_journal_after_cleanup: bool,
    pub release_lease_after_cleanup: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    ProjectPackage(#[from] ProjectPackageError),
    #[error("invalid install-session JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported install-session format {0}")]
    UnsupportedFormat(u32),
    #[error("install session must contain at least one package")]
    EmptySession,
    #[error("invalid install session id {0:?}")]
    InvalidSessionId(String),
    #[error("invalid package name {0:?}")]
    InvalidPackageName(String),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("{0} must be non-empty, bounded, and free of control characters")]
    InvalidText(&'static str),
    #[error("invalid persisted session state for package {0:?}")]
    InvalidJournalState(String),
    #[error("session phase does not match package states")]
    InvalidPhaseState,
    #[error("unknown package instance {0:?} in install session")]
    UnknownPackage(String),
    #[error("duplicate package instance {0:?} in install session")]
    DuplicateInstance(String),
    #[error("private dependency graph for root {0:?} is incomplete or cyclic")]
    IncompletePrivateGraph(String),
    #[error("invalid package instance key: expected {expected:?}, got {actual:?}")]
    InvalidInstanceKey { expected: String, actual: String },
    #[error("invalid root package instance {0:?}")]
    InvalidRootInstance(String),
    #[error("private package instance cannot point a root to itself: {root:?} -> {package:?}")]
    InvalidPrivateInstance { root: String, package: String },
    #[error("install session is no longer in preparing state")]
    SessionNotPreparing,
    #[error("install session is not ready for atomic commit")]
    SessionNotReady,
    #[error("package {package:?} expected state {expected:?}, got {actual:?}")]
    InvalidPackageTransition {
        package: String,
        expected: SessionPackageState,
        actual: SessionPackageState,
    },
    #[error("artifact hash mismatch for {package:?}: expected {expected}, got {actual}")]
    ArtifactHashMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    #[error("package {0:?} failed attestation and remains quarantined")]
    PackageQuarantined(String),
    #[error("target lock changed during install: expected {expected}, got {actual}")]
    TargetLockChanged { expected: String, actual: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_package_attestation::{AttestationPolicy, PackageAttestationInput, ReportScope};
    use rbe_project_package::LockedProjectPackage;

    fn package(version: &str, artifact: char) -> LockedProjectPackage {
        LockedProjectPackage {
            version: version.into(),
            resolved_from: format!("registry:pkg-{artifact}"),
            artifact_url: format!("https://cdn.kastrick.invalid/pkg-{artifact}.zip"),
            artifact_sha256: artifact.to_string().repeat(64),
            manifest_sha256: "f".repeat(64),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: None,
            sdk: None,
        }
    }

    fn lock() -> ProjectPackageLock {
        ProjectPackageLock {
            format: 1,
            packages: BTreeMap::from([
                ("alpha".into(), package("1.0.0", 'a')),
                ("beta".into(), package("2.0.0", 'b')),
            ]),
            private: BTreeMap::new(),
        }
    }

    fn lock_with_private_conflict() -> ProjectPackageLock {
        let mut alpha = package("1.0.0", 'a');
        alpha.dependencies.insert("shared".into(), "^1.5".into());
        let mut beta = package("2.0.0", 'b');
        beta.dependencies.insert("shared".into(), "~1.3".into());
        ProjectPackageLock {
            format: 1,
            packages: BTreeMap::from([("alpha".into(), alpha), ("beta".into(), beta)]),
            private: BTreeMap::from([
                (
                    "alpha".into(),
                    BTreeMap::from([("shared".into(), package("1.5.2", 'c'))]),
                ),
                (
                    "beta".into(),
                    BTreeMap::from([("shared".into(), package("1.3.9", 'd'))]),
                ),
            ]),
        }
    }

    fn good_gate(package_name: &str, artifact: char) -> ActivationGate {
        let sha = artifact.to_string().repeat(64);
        let input = PackageAttestationInput {
            package: package_name.into(),
            version: "1.0.0".into(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: sha.clone(),
            downloaded_artifact_sha256: sha,
            local_source_sha256: None,
            remote_source_sha256: None,
            publisher_signature_declared: false,
            publisher_signature_verified: false,
            reproducible_build: false,
            shipped_binary_sha256: None,
            rebuilt_binary_sha256: None,
            build_id: "no-build".into(),
            host: "linux-x86_64".into(),
        };
        crate::gate_activation(&input, AttestationPolicy::default()).unwrap()
    }

    fn ready_session() -> (InstallSession, ProjectPackageLock) {
        let lock = lock();
        let mut session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        for (name, artifact) in [("alpha", 'a'), ("beta", 'b')] {
            session
                .mark_artifact_verified(name, &artifact.to_string().repeat(64))
                .unwrap();
            session.mark_prepared(name, None).unwrap();
            session
                .mark_attested(name, &good_gate(name, artifact))
                .unwrap();
            session.mark_ready(name).unwrap();
        }
        (session, lock)
    }

    #[test]
    fn session_roundtrips_and_tracks_exact_target_lock() {
        let lock = lock();
        let session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        let encoded = session.render_json().unwrap();
        let decoded = InstallSession::parse_json(&encoded).unwrap();
        assert_eq!(decoded, session);
        assert_eq!(
            decoded.target_lock_sha256,
            canonical_lock_sha256(&lock).unwrap()
        );
    }

    #[test]
    fn private_dependency_versions_are_distinct_session_instances() {
        let lock = lock_with_private_conflict();
        let session = InstallSession::new("session-private", "e".repeat(64), &lock).unwrap();
        assert_eq!(session.packages.len(), 4);
        assert_eq!(session.packages["alpha::shared"].version, "1.5.2");
        assert_eq!(session.packages["beta::shared"].version, "1.3.9");
        assert!(session.packages["alpha::shared"].private);
        assert!(session.packages["beta::shared"].private);
    }

    #[test]
    fn invalid_package_transition_is_rejected() {
        let lock = lock();
        let mut session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        assert!(matches!(
            session.mark_prepared("alpha", None),
            Err(SessionError::InvalidPackageTransition { .. })
        ));
    }

    #[test]
    fn quarantined_package_cannot_reach_ready_state() {
        let lock = lock();
        let mut session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        session
            .mark_artifact_verified("alpha", &"a".repeat(64))
            .unwrap();
        session.mark_prepared("alpha", None).unwrap();
        let input = PackageAttestationInput {
            package: "alpha".into(),
            version: "1.0.0".into(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: "a".repeat(64),
            downloaded_artifact_sha256: "d".repeat(64),
            local_source_sha256: None,
            remote_source_sha256: None,
            publisher_signature_declared: false,
            publisher_signature_verified: false,
            reproducible_build: false,
            shipped_binary_sha256: None,
            rebuilt_binary_sha256: None,
            build_id: "no-build".into(),
            host: "linux-x86_64".into(),
        };
        let gate = crate::gate_activation(&input, AttestationPolicy::default()).unwrap();
        assert!(matches!(
            session.mark_attested("alpha", &gate),
            Err(SessionError::PackageQuarantined(_))
        ));
    }

    #[test]
    fn all_packages_must_be_ready_before_atomic_lock_swap() {
        let (session, lock) = ready_session();
        assert_eq!(session.phase, InstallSessionPhase::ReadyToCommit);
        let layout = ProjectCacheLayout::new("/project");
        let commit = session.lock_commit_plan(&lock, &layout).unwrap();
        assert_eq!(
            commit.final_path,
            PathBuf::from("/project/package.lock.rbe.yaml")
        );
        assert_eq!(
            commit.temporary_path,
            PathBuf::from("/project/package.lock.rbe.yaml.next")
        );
        assert!(commit.atomic_replace);
        assert!(commit.activation_is_lock_swap);
    }

    #[test]
    fn changed_target_lock_is_rejected_at_commit_boundary() {
        let (session, mut lock) = ready_session();
        lock.packages.get_mut("alpha").unwrap().version = "9.0.0".into();
        let error = session
            .lock_commit_plan(&lock, &ProjectCacheLayout::new("/project"))
            .unwrap_err();
        assert!(matches!(error, SessionError::TargetLockChanged { .. }));
    }

    #[test]
    fn journal_and_exclusive_lease_live_under_rbe_cache() {
        let lock = lock();
        let session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        let layout = ProjectCacheLayout::new("/project");
        let journal = session.journal_write_plan(&layout).unwrap();
        let lease = session.lease_plan(&layout);
        assert_eq!(
            journal.final_path,
            PathBuf::from("/project/.cache/rbe/install/journal.rbe.json")
        );
        assert_eq!(
            lease.path,
            PathBuf::from("/project/.cache/rbe/install/install.lease")
        );
        assert!(lease.exclusive);
    }

    #[test]
    fn recovery_rolls_back_visibility_but_preserves_verified_cache() {
        let lock = lock();
        let session = InstallSession::new("session-1", "c".repeat(64), &lock).unwrap();
        let layout = ProjectCacheLayout::new("/project");
        let recovery = session.recovery_plan(None, &layout).unwrap();
        assert_eq!(
            recovery.disposition,
            RecoveryDisposition::RollbackUncommitted
        );
        assert!(recovery.preserve_current_lock);
        assert_eq!(
            recovery.preserve_library_cache_root,
            PathBuf::from("/project/.cache/library")
        );
    }

    #[test]
    fn recovery_detects_lock_swap_that_completed_before_crash() {
        let (session, lock) = ready_session();
        let recovery = session
            .recovery_plan(Some(&lock), &ProjectCacheLayout::new("/project"))
            .unwrap();
        assert_eq!(recovery.disposition, RecoveryDisposition::AlreadyCommitted);
        assert!(recovery.delete_journal_after_cleanup);
    }
}
