//! Source-only orchestration for project package installation.
//!
//! This crate joins the project manifest/lock, verified cache, managed system
//! toolchains, package-manager events, and package attestation into one trusted
//! plan. It intentionally performs no network I/O, process execution, filesystem
//! mutation, or Backend binary integration.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rbe_install_request::{SystemRuntimeKind, SystemRuntimeManifestRequest};
use rbe_library_package::{Language, LibraryManifest};
use rbe_package_attestation::{
    attest, AttestationError, AttestationPolicy, AttestationResult, KastrickPackageFailureReport,
    PackageAttestationInput,
};
use rbe_package_manager::InstallEvent;
use rbe_project_package::{
    LockedProjectPackage, PackageRequirement, ProjectCacheLayout, ProjectPackageError,
    ProjectPackageLock, ProjectPackageManifest,
};
use semver::{Version, VersionReq};
use url::Url;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifiedCacheInventory {
    hashes: BTreeSet<String>,
}

impl VerifiedCacheInventory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an artifact only after the caller has verified its bytes against
    /// this SHA-256. Merely finding a directory in `.cache/library` is not enough.
    pub fn admit_verified(&mut self, sha256: impl Into<String>) -> Result<(), OrchestratorError> {
        self.hashes.insert(canonical_sha256(sha256.into())?);
        Ok(())
    }

    pub fn contains(&self, sha256: &str) -> bool {
        self.hashes.contains(&sha256.to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapAction {
    UseVerifiedCache {
        package: String,
        version: String,
        artifact_sha256: String,
        path: PathBuf,
    },
    FetchLockedArtifact {
        package: String,
        version: String,
        artifact_url: Url,
        artifact_sha256: String,
        destination: PathBuf,
    },
    Resolve {
        package: String,
        requirement: Option<String>,
        source: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBootstrapPlan {
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub actions: Vec<BootstrapAction>,
    pub events: Vec<InstallEvent>,
}

impl ProjectBootstrapPlan {
    pub fn from_yaml(
        project_root: impl Into<PathBuf>,
        manifest_yaml: &str,
        lock_yaml: Option<&str>,
        verified_cache: &VerifiedCacheInventory,
    ) -> Result<Self, OrchestratorError> {
        let layout = ProjectCacheLayout::new(project_root.into());
        let manifest = ProjectPackageManifest::parse_yaml(manifest_yaml)?;
        let lock = lock_yaml.map(ProjectPackageLock::parse_yaml).transpose()?;

        let mut actions = Vec::with_capacity(manifest.packages.len());
        let mut events = Vec::new();
        let mut unresolved_roots = Vec::new();

        for (package, requirement) in &manifest.packages {
            let locked = lock
                .as_ref()
                .and_then(|lock| lock.packages.get(package))
                .filter(|locked| lock_satisfies_requirement(requirement, locked).unwrap_or(false));

            if let Some(locked) = locked {
                let destination = layout.library_artifact_dir(&locked.artifact_sha256)?;
                if verified_cache.contains(&locked.artifact_sha256) {
                    actions.push(BootstrapAction::UseVerifiedCache {
                        package: package.clone(),
                        version: locked.version.clone(),
                        artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
                        path: destination,
                    });
                    events.push(InstallEvent::CacheHit {
                        package: package.clone(),
                        version: locked.version.clone(),
                    });
                } else {
                    actions.push(BootstrapAction::FetchLockedArtifact {
                        package: package.clone(),
                        version: locked.version.clone(),
                        artifact_url: parse_https_url(&locked.artifact_url)?,
                        artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
                        destination,
                    });
                }
                continue;
            }

            unresolved_roots.push(package.clone());
            actions.push(BootstrapAction::Resolve {
                package: package.clone(),
                requirement: requirement.version().map(ToOwned::to_owned),
                source: requirement.source().map(ToOwned::to_owned),
            });
        }

        if !unresolved_roots.is_empty() {
            events.insert(
                0,
                InstallEvent::Resolving {
                    roots: unresolved_roots,
                },
            );
        }

        Ok(Self {
            manifest_path: layout.manifest_path(),
            lock_path: layout.lock_path(),
            actions,
            events,
        })
    }

    pub fn requires_resolution(&self) -> bool {
        self.actions
            .iter()
            .any(|action| matches!(action, BootstrapAction::Resolve { .. }))
    }
}

fn lock_satisfies_requirement(
    requirement: &PackageRequirement,
    locked: &LockedProjectPackage,
) -> Result<bool, OrchestratorError> {
    if let Some(source) = requirement.source() {
        if locked.resolved_from != source {
            return Ok(false);
        }
    }

    let Some(requirement) = requirement.version() else {
        return Ok(true);
    };
    let requirement =
        VersionReq::parse(requirement).map_err(|source| OrchestratorError::InvalidRequirement {
            value: requirement.to_string(),
            source,
        })?;
    let version =
        Version::parse(&locked.version).map_err(|source| OrchestratorError::InvalidVersion {
            value: locked.version.clone(),
            source,
        })?;
    Ok(requirement.matches(&version))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIsolationPolicy {
    pub compile_from_source_when_available: bool,
    pub allow_shipped_binary_as_input: bool,
    pub network_during_build: bool,
    pub require_attestation_before_activation: bool,
    pub mutate_system_path: bool,
}

impl Default for BuildIsolationPolicy {
    fn default() -> Self {
        Self {
            compile_from_source_when_available: true,
            allow_shipped_binary_as_input: true,
            network_during_build: false,
            require_attestation_before_activation: true,
            mutate_system_path: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemToolchainNeed {
    pub runtime: SystemRuntimeKind,
    pub version_requirement: String,
    pub manifest_request: SystemRuntimeManifestRequest,
    /// Pass this path to `SystemRuntimeManifest::plan` after fetching/validating
    /// the remote manifest. It intentionally points at `<project>/.cache`.
    pub cache_root: PathBuf,
}

pub fn system_toolchain_for_manifest(
    manifest: &LibraryManifest,
    project_root: impl AsRef<Path>,
    kastrick_registry: &str,
    host_id: &str,
) -> Result<Option<SystemToolchainNeed>, OrchestratorError> {
    manifest.validate()?;
    let runtime = match manifest.language {
        Language::Rust => SystemRuntimeKind::Rust,
        Language::Python => SystemRuntimeKind::Python,
        Language::Javascript if manifest.runtime.kind == "bun" => SystemRuntimeKind::Bunjs,
        Language::Javascript if manifest.runtime.kind == "node" => SystemRuntimeKind::Nodejs,
        Language::Javascript | Language::Other => return Ok(None),
    };

    let layout = ProjectCacheLayout::new(project_root.as_ref());
    Ok(Some(SystemToolchainNeed {
        runtime,
        version_requirement: manifest.runtime.version.clone(),
        manifest_request: SystemRuntimeManifestRequest::new(kastrick_registry, runtime, host_id)?,
        cache_root: layout.cache_root(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationGate {
    pub result: AttestationResult,
    pub report: Option<KastrickPackageFailureReport>,
}

impl ActivationGate {
    pub const fn can_activate(&self) -> bool {
        self.result.can_activate()
    }
}

pub fn gate_activation(
    input: &PackageAttestationInput,
    policy: AttestationPolicy,
) -> Result<ActivationGate, OrchestratorError> {
    let result = attest(input, policy)?;
    let report = KastrickPackageFailureReport::from_result(input, &result)?;
    Ok(ActivationGate { result, report })
}

fn parse_https_url(value: &str) -> Result<Url, OrchestratorError> {
    let url = Url::parse(value).map_err(|source| OrchestratorError::InvalidUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(OrchestratorError::HttpsRequired(value.to_string()));
    }
    Ok(url)
}

fn canonical_sha256(value: String) -> Result<String, OrchestratorError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(OrchestratorError::InvalidSha256(value));
    }
    Ok(value.to_ascii_lowercase())
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    ProjectPackage(#[from] ProjectPackageError),
    #[error(transparent)]
    InstallRequest(#[from] rbe_install_request::InstallRequestError),
    #[error(transparent)]
    Manifest(#[from] rbe_library_package::ManifestError),
    #[error(transparent)]
    Attestation(#[from] AttestationError),
    #[error("invalid version requirement {value:?}: {source}")]
    InvalidRequirement {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid locked version {value:?}: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("locked artifact URL must use HTTPS: {0:?}")]
    HttpsRequired(String),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_package_attestation::ReportScope;

    fn manifest_yaml() -> &'static str {
        r#"
format: 1
packages:
  advancenet: 4.0.1
  runtime.python: "3.10"
"#
    }

    fn lock_yaml() -> String {
        format!(
            r#"
format: 1
packages:
  advancenet:
    version: 4.0.1
    resolved_from: registry:advancenet
    artifact_url: https://cdn.kastrick.invalid/advancenet-4.0.1.zip
    artifact_sha256: {a}
    manifest_sha256: {b}
  runtime.python:
    version: 3.10.14
    resolved_from: registry:runtime.python
    artifact_url: https://cdn.kastrick.invalid/python-3.10.14.zip
    artifact_sha256: {c}
    manifest_sha256: {d}
"#,
            a = "a".repeat(64),
            b = "b".repeat(64),
            c = "c".repeat(64),
            d = "d".repeat(64),
        )
    }

    #[test]
    fn verified_cache_is_reused_but_missing_cache_rehydrates_from_lock() {
        let mut cache = VerifiedCacheInventory::new();
        cache.admit_verified("a".repeat(64)).unwrap();
        let plan = ProjectBootstrapPlan::from_yaml(
            "/project",
            manifest_yaml(),
            Some(&lock_yaml()),
            &cache,
        )
        .unwrap();
        assert!(!plan.requires_resolution());
        assert!(matches!(
            &plan.actions[0],
            BootstrapAction::UseVerifiedCache { package, .. } if package == "advancenet"
        ));
        assert!(matches!(
            &plan.actions[1],
            BootstrapAction::FetchLockedArtifact { package, .. } if package == "runtime.python"
        ));
    }

    #[test]
    fn manifest_version_drift_forces_fresh_resolution() {
        let changed = r#"
format: 1
packages:
  advancenet: "^5.0"
"#;
        let plan = ProjectBootstrapPlan::from_yaml(
            "/project",
            changed,
            Some(&lock_yaml()),
            &VerifiedCacheInventory::new(),
        )
        .unwrap();
        assert!(plan.requires_resolution());
        assert!(matches!(
            &plan.actions[0],
            BootstrapAction::Resolve { package, .. } if package == "advancenet"
        ));
    }

    #[test]
    fn system_toolchain_is_derived_from_library_language() {
        let manifest = LibraryManifest::parse(
            r#"
name = "advancenet"
version = "1.0.0"
language = "python"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "python"
package = "rbe-sdk"
version = "0.1"

[runtime]
kind = "python"
version = "3.13"
entry = "worker.py"
"#,
        )
        .unwrap();
        let need = system_toolchain_for_manifest(
            &manifest,
            "/project",
            "https://registry.kastrick.invalid/",
            "windows-x86_64",
        )
        .unwrap()
        .unwrap();
        assert_eq!(need.runtime, SystemRuntimeKind::Python);
        assert_eq!(need.cache_root, PathBuf::from("/project/.cache"));
        assert!(need
            .manifest_request
            .endpoint
            .as_str()
            .contains("rbe.sys.python/windows-x86_64/manifest.json"));
    }

    #[test]
    fn public_attestation_failure_is_quarantined_and_reportable() {
        let input = PackageAttestationInput {
            package: "advancenet".into(),
            version: "4.0.1".into(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: "a".repeat(64),
            downloaded_artifact_sha256: "b".repeat(64),
            local_source_sha256: None,
            remote_source_sha256: None,
            publisher_signature_declared: false,
            publisher_signature_verified: false,
            reproducible_build: false,
            shipped_binary_sha256: None,
            rebuilt_binary_sha256: None,
            build_id: "rust-1.98.1-windows-x86_64".into(),
            host: "windows-x86_64".into(),
        };
        let gate = gate_activation(&input, AttestationPolicy::default()).unwrap();
        assert!(!gate.can_activate());
        assert!(gate.report.is_some());
    }

    #[test]
    fn private_attestation_failure_never_generates_silent_report() {
        let input = PackageAttestationInput {
            package: "internal.pkg".into(),
            version: "1.0.0".into(),
            scope: ReportScope::PrivateOrLocal,
            locked_artifact_sha256: "a".repeat(64),
            downloaded_artifact_sha256: "b".repeat(64),
            local_source_sha256: None,
            remote_source_sha256: None,
            publisher_signature_declared: false,
            publisher_signature_verified: false,
            reproducible_build: false,
            shipped_binary_sha256: None,
            rebuilt_binary_sha256: None,
            build_id: "local-test".into(),
            host: "windows-x86_64".into(),
        };
        let gate = gate_activation(&input, AttestationPolicy::default()).unwrap();
        assert!(!gate.can_activate());
        assert!(gate.report.is_none());
    }

    #[test]
    fn default_build_policy_never_mutates_path_or_builds_with_network() {
        let policy = BuildIsolationPolicy::default();
        assert!(policy.compile_from_source_when_available);
        assert!(policy.require_attestation_before_activation);
        assert!(!policy.network_during_build);
        assert!(!policy.mutate_system_path);
    }
}
