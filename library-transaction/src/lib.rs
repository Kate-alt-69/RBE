//! Pure state machine for one RBE external-library installation transaction.
//!
//! This crate performs no network I/O, filesystem mutation, process execution, or
//! binary production. Trusted orchestration code supplies results for each phase;
//! the transaction verifies ordering and cross-phase identity before activation.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use rbe_library_installer::{
    HostTarget, InstallPlan, InstallerError, PackageRequest, ResolvedEnvironment, VersionCatalog,
};
use rbe_library_lock::{LockedPackage, ProjectLayout};
use rbe_library_package::LibraryManifest;
use rbe_sdk::LIBRARY_ABI_VERSION;
use semver::{Version, VersionReq};
use url::Url;

pub const LIBRARY_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Requested,
    SourceResolved,
    ArtifactVerified,
    PackageInspected,
    EnvironmentResolved,
    CapabilitiesAdmitted,
    BuildCompleted,
    HandshakeVerified,
    ReadyToActivate,
    Activated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityOrigin {
    RegistryMetadata,
    UserPin,
    ContentPinOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactLocation {
    Remote(Url),
    Local(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub expected_name: Option<String>,
    pub expected_version: Option<String>,
    pub location: ArtifactLocation,
    pub expected_sha256: Option<String>,
    pub integrity_origin: IntegrityOrigin,
    pub publisher: Option<String>,
    pub signature: Option<String>,
}

impl ResolvedSource {
    pub fn registry(
        name: impl Into<String>,
        version: impl Into<String>,
        artifact_url: impl AsRef<str>,
        sha256: impl Into<String>,
        publisher: Option<String>,
        signature: Option<String>,
    ) -> Result<Self, TransactionError> {
        let name = name.into();
        PackageRequest::registry(name.clone(), None::<String>)?;
        let version = version.into();
        Version::parse(&version).map_err(|source| TransactionError::InvalidVersion {
            value: version.clone(),
            source,
        })?;
        let location = validated_remote(artifact_url.as_ref())?;
        let sha256 = canonical_sha256(sha256.into())?;
        Ok(Self {
            expected_name: Some(name),
            expected_version: Some(version),
            location,
            expected_sha256: Some(sha256),
            integrity_origin: IntegrityOrigin::RegistryMetadata,
            publisher,
            signature,
        })
    }

    pub fn direct_url(
        artifact_url: impl AsRef<str>,
        expected_sha256: Option<String>,
    ) -> Result<Self, TransactionError> {
        let location = validated_remote(artifact_url.as_ref())?;
        let expected_sha256 = expected_sha256.map(canonical_sha256).transpose()?;
        Ok(Self {
            expected_name: None,
            expected_version: None,
            location,
            integrity_origin: if expected_sha256.is_some() {
                IntegrityOrigin::UserPin
            } else {
                IntegrityOrigin::ContentPinOnly
            },
            expected_sha256,
            publisher: None,
            signature: None,
        })
    }

    pub fn local(
        path: impl Into<PathBuf>,
        expected_sha256: Option<String>,
    ) -> Result<Self, TransactionError> {
        let path = path.into();
        PackageRequest::local(path.clone())?;
        let expected_sha256 = expected_sha256.map(canonical_sha256).transpose()?;
        Ok(Self {
            expected_name: None,
            expected_version: None,
            location: ArtifactLocation::Local(path),
            integrity_origin: if expected_sha256.is_some() {
                IntegrityOrigin::UserPin
            } else {
                IntegrityOrigin::ContentPinOnly
            },
            expected_sha256,
            publisher: None,
            signature: None,
        })
    }
}

fn validated_remote(value: &str) -> Result<ArtifactLocation, TransactionError> {
    match PackageRequest::url(value)? {
        PackageRequest::Url(url) => Ok(ArtifactLocation::Remote(url)),
        _ => unreachable!("PackageRequest::url always creates Url"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArtifact {
    pub sha256: String,
    pub size_bytes: u64,
    pub integrity_origin: IntegrityOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAdmission {
    pub runtime: Vec<String>,
    pub build: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHello {
    pub protocol: u32,
    pub abi: u32,
    pub package_name: String,
    pub package_version: String,
    pub content_sha256: String,
    pub sdk_family: String,
    pub sdk_package: String,
    pub sdk_version: String,
    pub runtime_kind: String,
    pub runtime_version: String,
}

#[derive(Debug, Clone)]
pub struct InstallTransaction {
    phase: InstallPhase,
    request: PackageRequest,
    host: HostTarget,
    source: Option<ResolvedSource>,
    artifact: Option<VerifiedArtifact>,
    manifest: Option<LibraryManifest>,
    plan: Option<InstallPlan>,
    environment: Option<ResolvedEnvironment>,
    admission: Option<CapabilityAdmission>,
    build_id: Option<String>,
    hello: Option<WorkerHello>,
}

impl InstallTransaction {
    pub fn new(request: PackageRequest, host: HostTarget) -> Self {
        Self {
            phase: InstallPhase::Requested,
            request,
            host,
            source: None,
            artifact: None,
            manifest: None,
            plan: None,
            environment: None,
            admission: None,
            build_id: None,
            hello: None,
        }
    }

    pub const fn phase(&self) -> InstallPhase {
        self.phase
    }

    pub fn request(&self) -> &PackageRequest {
        &self.request
    }

    pub fn source(&self) -> Option<&ResolvedSource> {
        self.source.as_ref()
    }

    pub fn artifact(&self) -> Option<&VerifiedArtifact> {
        self.artifact.as_ref()
    }

    pub fn manifest(&self) -> Option<&LibraryManifest> {
        self.manifest.as_ref()
    }

    pub fn plan(&self) -> Option<&InstallPlan> {
        self.plan.as_ref()
    }

    pub fn environment(&self) -> Option<&ResolvedEnvironment> {
        self.environment.as_ref()
    }

    pub fn admission(&self) -> Option<&CapabilityAdmission> {
        self.admission.as_ref()
    }

    pub fn resolve_source(&mut self, source: ResolvedSource) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::Requested)?;
        validate_source_for_request(&self.request, &source)?;
        self.source = Some(source);
        self.phase = InstallPhase::SourceResolved;
        Ok(())
    }

    pub fn verify_artifact(
        &mut self,
        actual_sha256: impl Into<String>,
        size_bytes: u64,
    ) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::SourceResolved)?;
        let actual_sha256 = canonical_sha256(actual_sha256.into())?;
        let source = self
            .source
            .as_ref()
            .ok_or(TransactionError::MissingState("source"))?;
        if let Some(expected) = &source.expected_sha256 {
            if expected != &actual_sha256 {
                return Err(TransactionError::HashMismatch {
                    expected: expected.clone(),
                    actual: actual_sha256,
                });
            }
        }
        self.artifact = Some(VerifiedArtifact {
            sha256: actual_sha256,
            size_bytes,
            integrity_origin: source.integrity_origin,
        });
        self.phase = InstallPhase::ArtifactVerified;
        Ok(())
    }

    pub fn inspect_package(&mut self, manifest: LibraryManifest) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::ArtifactVerified)?;
        manifest
            .validate()
            .map_err(|error| TransactionError::Manifest(error.to_string()))?;
        if !manifest.supports_abi(LIBRARY_ABI_VERSION) {
            return Err(TransactionError::UnsupportedAbi {
                package_min: manifest.rbe_abi_min,
                package_max: manifest.rbe_abi_max,
                host: LIBRARY_ABI_VERSION,
            });
        }
        validate_manifest_identity(&self.request, self.source.as_ref(), &manifest)?;
        let plan = InstallPlan::from_manifest(&manifest, self.host.clone())?;
        self.plan = Some(plan);
        self.manifest = Some(manifest);
        self.phase = InstallPhase::PackageInspected;
        Ok(())
    }

    pub fn resolve_environment(
        &mut self,
        layout: &ProjectLayout,
        catalog: &VersionCatalog,
    ) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::PackageInspected)?;
        let plan = self
            .plan
            .as_ref()
            .ok_or(TransactionError::MissingState("plan"))?;
        let environment = plan.resolve_environment(layout, catalog)?;
        self.environment = Some(environment);
        self.phase = InstallPhase::EnvironmentResolved;
        Ok(())
    }

    pub fn admit_capabilities(
        &mut self,
        runtime: impl IntoIterator<Item = String>,
        build: impl IntoIterator<Item = String>,
    ) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::EnvironmentResolved)?;
        let plan = self
            .plan
            .as_ref()
            .ok_or(TransactionError::MissingState("plan"))?;
        let runtime = canonical_admission(runtime, &plan.requested_capabilities, "runtime")?;
        let build = canonical_admission(build, &plan.requested_build_capabilities, "build")?;
        self.admission = Some(CapabilityAdmission { runtime, build });
        self.phase = InstallPhase::CapabilitiesAdmitted;
        Ok(())
    }

    pub fn complete_build(&mut self, build_id: impl Into<String>) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::CapabilitiesAdmitted)?;
        let build_id = build_id.into();
        if build_id.trim().is_empty() || build_id.len() > 512 {
            return Err(TransactionError::InvalidBuildId);
        }
        self.build_id = Some(build_id);
        self.phase = InstallPhase::BuildCompleted;
        Ok(())
    }

    pub fn verify_handshake(&mut self, hello: WorkerHello) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::BuildCompleted)?;
        let manifest = self
            .manifest
            .as_ref()
            .ok_or(TransactionError::MissingState("manifest"))?;
        let artifact = self
            .artifact
            .as_ref()
            .ok_or(TransactionError::MissingState("artifact"))?;
        let environment = self
            .environment
            .as_ref()
            .ok_or(TransactionError::MissingState("environment"))?;

        if hello.protocol != LIBRARY_PROTOCOL_VERSION {
            return Err(TransactionError::ProtocolMismatch {
                expected: LIBRARY_PROTOCOL_VERSION,
                actual: hello.protocol,
            });
        }
        if hello.abi != LIBRARY_ABI_VERSION || !manifest.supports_abi(hello.abi) {
            return Err(TransactionError::HandshakeMismatch("ABI"));
        }
        let hello_hash = canonical_sha256(hello.content_sha256.clone())?;
        if hello.package_name != manifest.name
            || hello.package_version != manifest.version
            || hello_hash != artifact.sha256
            || hello.sdk_family != manifest.sdk.family
            || hello.sdk_package != manifest.sdk.package
            || hello.sdk_version != environment.sdk.version
            || hello.runtime_kind != environment.runtime.kind
            || hello.runtime_version != environment.runtime.version
        {
            return Err(TransactionError::HandshakeMismatch("identity"));
        }
        self.hello = Some(hello);
        self.phase = InstallPhase::HandshakeVerified;
        Ok(())
    }

    pub fn prepare_activation(&mut self) -> Result<LockedPackage, TransactionError> {
        self.require_phase(InstallPhase::HandshakeVerified)?;
        let manifest = self
            .manifest
            .as_ref()
            .ok_or(TransactionError::MissingState("manifest"))?;
        let artifact = self
            .artifact
            .as_ref()
            .ok_or(TransactionError::MissingState("artifact"))?;
        let environment = self
            .environment
            .as_ref()
            .ok_or(TransactionError::MissingState("environment"))?;
        let admission = self
            .admission
            .as_ref()
            .ok_or(TransactionError::MissingState("admission"))?;
        let source = self
            .source
            .as_ref()
            .ok_or(TransactionError::MissingState("source"))?;
        let build_id = self
            .build_id
            .as_ref()
            .ok_or(TransactionError::MissingState("build id"))?;

        let package = LockedPackage::from_resolved(
            manifest,
            artifact.sha256.clone(),
            environment.sdk.version.clone(),
            environment.runtime.version.clone(),
            build_id.clone(),
            admission.runtime.clone(),
            admission.build.clone(),
            source.publisher.clone(),
            source.signature.clone(),
        )
        .map_err(|error| TransactionError::Lock(error.to_string()))?;
        self.phase = InstallPhase::ReadyToActivate;
        Ok(package)
    }

    /// Marks completion only after the caller atomically activates package state
    /// and persists the prepared lock entry.
    pub fn mark_activated(&mut self) -> Result<(), TransactionError> {
        self.require_phase(InstallPhase::ReadyToActivate)?;
        self.phase = InstallPhase::Activated;
        Ok(())
    }

    fn require_phase(&self, expected: InstallPhase) -> Result<(), TransactionError> {
        if self.phase != expected {
            return Err(TransactionError::InvalidPhase {
                expected,
                actual: self.phase,
            });
        }
        Ok(())
    }
}

fn validate_source_for_request(
    request: &PackageRequest,
    source: &ResolvedSource,
) -> Result<(), TransactionError> {
    match request {
        PackageRequest::Registry { name, requirement } => {
            if source.expected_name.as_deref() != Some(name.as_str()) {
                return Err(TransactionError::SourceRequestMismatch);
            }
            let version = source
                .expected_version
                .as_deref()
                .ok_or(TransactionError::SourceRequestMismatch)?;
            let parsed =
                Version::parse(version).map_err(|source| TransactionError::InvalidVersion {
                    value: version.to_string(),
                    source,
                })?;
            if let Some(requirement) = requirement {
                let requirement = VersionReq::parse(requirement).map_err(|source| {
                    TransactionError::InvalidRequirement {
                        value: requirement.clone(),
                        source,
                    }
                })?;
                if !requirement.matches(&parsed) {
                    return Err(TransactionError::SourceRequestMismatch);
                }
            }
            if source.expected_sha256.is_none()
                || source.integrity_origin != IntegrityOrigin::RegistryMetadata
            {
                return Err(TransactionError::RegistrySourceMissingIntegrity);
            }
        }
        PackageRequest::Url(requested) => {
            if source.location != ArtifactLocation::Remote(requested.clone()) {
                return Err(TransactionError::SourceRequestMismatch);
            }
        }
        PackageRequest::LocalArchive(requested) => {
            if source.location != ArtifactLocation::Local(requested.clone()) {
                return Err(TransactionError::SourceRequestMismatch);
            }
        }
    }
    Ok(())
}

fn validate_manifest_identity(
    request: &PackageRequest,
    source: Option<&ResolvedSource>,
    manifest: &LibraryManifest,
) -> Result<(), TransactionError> {
    if let Some(source) = source {
        if source
            .expected_name
            .as_ref()
            .is_some_and(|name| name != &manifest.name)
            || source
                .expected_version
                .as_ref()
                .is_some_and(|version| version != &manifest.version)
        {
            return Err(TransactionError::ManifestIdentityMismatch);
        }
    }
    if let PackageRequest::Registry { name, requirement } = request {
        if name != &manifest.name {
            return Err(TransactionError::ManifestIdentityMismatch);
        }
        if let Some(requirement) = requirement {
            let requirement = VersionReq::parse(requirement).map_err(|source| {
                TransactionError::InvalidRequirement {
                    value: requirement.clone(),
                    source,
                }
            })?;
            let version = Version::parse(&manifest.version).map_err(|source| {
                TransactionError::InvalidVersion {
                    value: manifest.version.clone(),
                    source,
                }
            })?;
            if !requirement.matches(&version) {
                return Err(TransactionError::ManifestIdentityMismatch);
            }
        }
    }
    Ok(())
}

fn canonical_admission(
    values: impl IntoIterator<Item = String>,
    requested: &[String],
    kind: &'static str,
) -> Result<Vec<String>, TransactionError> {
    let allowed: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
    let mut result = BTreeSet::new();
    for value in values {
        if !allowed.contains(value.as_str()) {
            return Err(TransactionError::CapabilityOvergrant { kind, value });
        }
        result.insert(value);
    }
    Ok(result.into_iter().collect())
}

fn canonical_sha256(value: String) -> Result<String, TransactionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TransactionError::InvalidSha256(value));
    }
    Ok(value.to_ascii_lowercase())
}

#[derive(Debug, thiserror::Error)]
pub enum TransactionError {
    #[error(transparent)]
    Installer(#[from] InstallerError),
    #[error("invalid install transition: expected {expected:?}, currently {actual:?}")]
    InvalidPhase {
        expected: InstallPhase,
        actual: InstallPhase,
    },
    #[error("install transaction is missing {0}")]
    MissingState(&'static str),
    #[error("resolved package source does not match the original install request")]
    SourceRequestMismatch,
    #[error("registry resolution must include registry-authoritative integrity metadata")]
    RegistrySourceMissingIntegrity,
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("artifact SHA-256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("invalid package version {value:?}: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid package requirement {value:?}: {source}")]
    InvalidRequirement {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("package manifest is invalid: {0}")]
    Manifest(String),
    #[error("package manifest identity does not match resolved package metadata")]
    ManifestIdentityMismatch,
    #[error("package ABI {package_min}..={package_max} does not include host ABI {host}")]
    UnsupportedAbi {
        package_min: u32,
        package_max: u32,
        host: u32,
    },
    #[error("{kind} capability {value:?} was not requested by library.toml")]
    CapabilityOvergrant { kind: &'static str, value: String },
    #[error("build identity must be non-empty and bounded")]
    InvalidBuildId,
    #[error("library protocol mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: u32, actual: u32 },
    #[error("library worker handshake {0} does not match admitted package state")]
    HandshakeMismatch(&'static str),
    #[error("could not prepare rbe.lock entry: {0}")]
    Lock(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "1.2.0"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "javascript"
package = "@rbe/sdk"
version = "0.1"

[runtime]
kind = "bun"
version = "1.x"
entry = "src/index.js"

[capabilities]
"net:http" = true
"net:p2p" = true
"router:register" = false

[build_capabilities]
"network:dependencies" = true

[[build.other]]
program = "bun"
args = ["install", "--frozen-lockfile"]
"#;

    fn catalog() -> VersionCatalog {
        let mut catalog = VersionCatalog::default();
        catalog.add_runtime("bun", "1.2.3").unwrap();
        catalog.add_sdk("@rbe/sdk", "0.1.9").unwrap();
        catalog
    }

    fn hello(hash: &str) -> WorkerHello {
        WorkerHello {
            protocol: LIBRARY_PROTOCOL_VERSION,
            abi: LIBRARY_ABI_VERSION,
            package_name: "advancenet".into(),
            package_version: "1.2.0".into(),
            content_sha256: hash.into(),
            sdk_family: "javascript".into(),
            sdk_package: "@rbe/sdk".into(),
            sdk_version: "0.1.9".into(),
            runtime_kind: "bun".into(),
            runtime_version: "1.2.3".into(),
        }
    }

    fn registry_transaction() -> InstallTransaction {
        let request = PackageRequest::registry("advancenet", Some("^1.0")).unwrap();
        InstallTransaction::new(
            request,
            HostTarget::new(rbe_library_package::HostOs::Linux, "x86_64").unwrap(),
        )
    }

    #[test]
    fn happy_path_produces_canonical_lock_entry() {
        let hash = "a".repeat(64);
        let mut transaction = registry_transaction();
        transaction
            .resolve_source(
                ResolvedSource::registry(
                    "advancenet",
                    "1.2.0",
                    "https://packages.example/advancenet-1.2.0.zip",
                    hash.clone(),
                    Some("kastrick".into()),
                    Some("sig:v1:abc".into()),
                )
                .unwrap(),
            )
            .unwrap();
        transaction.verify_artifact(hash.clone(), 1024).unwrap();
        transaction
            .inspect_package(LibraryManifest::parse(MANIFEST).unwrap())
            .unwrap();
        transaction
            .resolve_environment(&ProjectLayout::new("/project"), &catalog())
            .unwrap();
        transaction
            .admit_capabilities(
                vec!["net:p2p".into(), "net:http".into()],
                vec!["network:dependencies".into()],
            )
            .unwrap();
        transaction.complete_build("build-linux-x86_64").unwrap();
        transaction.verify_handshake(hello(&hash)).unwrap();
        let locked = transaction.prepare_activation().unwrap();
        assert_eq!(transaction.phase(), InstallPhase::ReadyToActivate);
        assert_eq!(locked.content_sha256, hash);
        assert_eq!(locked.runtime.version, "1.2.3");
        assert_eq!(locked.sdk.version, "0.1.9");
        assert_eq!(locked.admitted_capabilities, ["net:http", "net:p2p"]);
        assert_eq!(locked.publisher.as_deref(), Some("kastrick"));
        transaction.mark_activated().unwrap();
        assert_eq!(transaction.phase(), InstallPhase::Activated);
    }

    #[test]
    fn cannot_skip_install_phases() {
        let mut transaction = registry_transaction();
        assert!(matches!(
            transaction.verify_artifact("a".repeat(64), 1),
            Err(TransactionError::InvalidPhase { .. })
        ));
        assert_eq!(transaction.phase(), InstallPhase::Requested);
    }

    #[test]
    fn registry_resolution_requires_hash_and_matching_requirement() {
        let mut transaction = registry_transaction();
        let wrong_version = ResolvedSource::registry(
            "advancenet",
            "2.0.0",
            "https://packages.example/advancenet-2.zip",
            "a".repeat(64),
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            transaction.resolve_source(wrong_version),
            Err(TransactionError::SourceRequestMismatch)
        ));
    }

    #[test]
    fn direct_url_without_expected_hash_is_content_pin_only() {
        let request = PackageRequest::url("https://packages.example/custom.zip").unwrap();
        let mut transaction = InstallTransaction::new(
            request,
            HostTarget::new(rbe_library_package::HostOs::Other, "x86_64").unwrap(),
        );
        transaction
            .resolve_source(
                ResolvedSource::direct_url("https://packages.example/custom.zip", None).unwrap(),
            )
            .unwrap();
        transaction.verify_artifact("b".repeat(64), 42).unwrap();
        assert_eq!(
            transaction.artifact().unwrap().integrity_origin,
            IntegrityOrigin::ContentPinOnly
        );
    }

    #[test]
    fn expected_hash_mismatch_does_not_advance_phase() {
        let request = PackageRequest::url("https://packages.example/custom.zip").unwrap();
        let mut transaction = InstallTransaction::new(
            request,
            HostTarget::new(rbe_library_package::HostOs::Other, "x86_64").unwrap(),
        );
        transaction
            .resolve_source(
                ResolvedSource::direct_url(
                    "https://packages.example/custom.zip",
                    Some("a".repeat(64)),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            transaction.verify_artifact("b".repeat(64), 42),
            Err(TransactionError::HashMismatch { .. })
        ));
        assert_eq!(transaction.phase(), InstallPhase::SourceResolved);
    }

    #[test]
    fn capability_admission_cannot_expand_manifest_authority() {
        let hash = "c".repeat(64);
        let mut transaction = registry_transaction();
        transaction
            .resolve_source(
                ResolvedSource::registry(
                    "advancenet",
                    "1.2.0",
                    "https://packages.example/advancenet.zip",
                    hash.clone(),
                    None,
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        transaction.verify_artifact(hash, 1).unwrap();
        transaction
            .inspect_package(LibraryManifest::parse(MANIFEST).unwrap())
            .unwrap();
        transaction
            .resolve_environment(&ProjectLayout::new("/project"), &catalog())
            .unwrap();
        assert!(matches!(
            transaction.admit_capabilities(vec!["router:register".into()], Vec::new()),
            Err(TransactionError::CapabilityOvergrant { .. })
        ));
        assert_eq!(transaction.phase(), InstallPhase::EnvironmentResolved);
    }

    #[test]
    fn worker_handshake_must_match_resolved_environment() {
        let hash = "d".repeat(64);
        let mut transaction = registry_transaction();
        transaction
            .resolve_source(
                ResolvedSource::registry(
                    "advancenet",
                    "1.2.0",
                    "https://packages.example/advancenet.zip",
                    hash.clone(),
                    None,
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        transaction.verify_artifact(hash.clone(), 1).unwrap();
        transaction
            .inspect_package(LibraryManifest::parse(MANIFEST).unwrap())
            .unwrap();
        transaction
            .resolve_environment(&ProjectLayout::new("/project"), &catalog())
            .unwrap();
        transaction
            .admit_capabilities(Vec::new(), Vec::new())
            .unwrap();
        transaction.complete_build("build-1").unwrap();
        let mut wrong = hello(&hash);
        wrong.runtime_version = "9.9.9".into();
        assert!(matches!(
            transaction.verify_handshake(wrong),
            Err(TransactionError::HandshakeMismatch(_))
        ));
        assert_eq!(transaction.phase(), InstallPhase::BuildCompleted);
    }
}
