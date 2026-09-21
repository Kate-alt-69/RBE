//! Package/source attestation primitives for RBE installs.
//!
//! A downloaded archive, shipped native executable, or cached build is never
//! trusted because of its path. RBE compares pinned digests, source digests,
//! publisher verification state, and (only for reproducible builds) rebuilt
//! binary digests before activation.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use url::Url;

pub const KASTRICK_PACKAGE_FAILURE_REPORT_PATH: &str = "/registry/v1/package-failures/report";
pub const PACKAGE_FAILURE_REPORT_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttestationPolicy {
    pub compile_from_source_when_available: bool,
    pub verify_archive_digest: bool,
    pub compare_remote_source: bool,
    pub require_verified_publisher_signature_when_present: bool,
    pub compare_rebuilt_binary_only_if_reproducible: bool,
}

impl Default for AttestationPolicy {
    fn default() -> Self {
        Self {
            compile_from_source_when_available: true,
            verify_archive_digest: true,
            compare_remote_source: true,
            require_verified_publisher_signature_when_present: true,
            compare_rebuilt_binary_only_if_reproducible: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportScope {
    PublicRegistry,
    PublicExternal,
    PrivateOrLocal,
}

impl ReportScope {
    pub const fn may_report(self) -> bool {
        !matches!(self, Self::PrivateOrLocal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAttestationInput {
    pub package: String,
    pub version: String,
    pub scope: ReportScope,
    pub locked_artifact_sha256: String,
    pub downloaded_artifact_sha256: String,
    pub local_source_sha256: Option<String>,
    pub remote_source_sha256: Option<String>,
    pub publisher_signature_declared: bool,
    pub publisher_signature_verified: bool,
    pub reproducible_build: bool,
    pub shipped_binary_sha256: Option<String>,
    pub rebuilt_binary_sha256: Option<String>,
    pub build_id: String,
    pub host: String,
}

impl PackageAttestationInput {
    pub fn validate(&self) -> Result<(), AttestationError> {
        validate_name(&self.package)?;
        validate_text("version", &self.version)?;
        validate_sha256(&self.locked_artifact_sha256)?;
        validate_sha256(&self.downloaded_artifact_sha256)?;
        validate_optional_sha(&self.local_source_sha256)?;
        validate_optional_sha(&self.remote_source_sha256)?;
        validate_optional_sha(&self.shipped_binary_sha256)?;
        validate_optional_sha(&self.rebuilt_binary_sha256)?;
        validate_text("build id", &self.build_id)?;
        validate_text("host", &self.host)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationResult {
    pub disposition: ActivationDisposition,
    pub source_state: SourceVerificationState,
    pub binary_state: BinaryVerificationState,
    pub failure: Option<AttestationFailure>,
}

impl AttestationResult {
    pub const fn can_activate(&self) -> bool {
        matches!(self.disposition, ActivationDisposition::Activate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDisposition {
    Activate,
    Quarantine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceVerificationState {
    MatchedRemote,
    LocalOnly,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryVerificationState {
    NotProvided,
    RebuiltMatch,
    RebuiltComparisonNotApplicable,
    RebuiltNotAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    ArtifactDigestMismatch,
    SourceDigestMismatch,
    PublisherSignatureInvalid,
    RebuiltBinaryDigestMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationFailure {
    pub category: FailureCategory,
    pub expected_sha256: Option<String>,
    pub actual_sha256: Option<String>,
}

pub fn attest(
    input: &PackageAttestationInput,
    policy: AttestationPolicy,
) -> Result<AttestationResult, AttestationError> {
    input.validate()?;

    if policy.verify_archive_digest
        && !same_sha(
            &input.locked_artifact_sha256,
            &input.downloaded_artifact_sha256,
        )
    {
        return Ok(quarantine(
            FailureCategory::ArtifactDigestMismatch,
            Some(&input.locked_artifact_sha256),
            Some(&input.downloaded_artifact_sha256),
        ));
    }

    if policy.require_verified_publisher_signature_when_present
        && input.publisher_signature_declared
        && !input.publisher_signature_verified
    {
        return Ok(quarantine(
            FailureCategory::PublisherSignatureInvalid,
            None,
            None,
        ));
    }

    let source_state = match (
        input.local_source_sha256.as_deref(),
        input.remote_source_sha256.as_deref(),
    ) {
        (Some(local), Some(remote)) if policy.compare_remote_source => {
            if !same_sha(local, remote) {
                return Ok(quarantine(
                    FailureCategory::SourceDigestMismatch,
                    Some(remote),
                    Some(local),
                ));
            }
            SourceVerificationState::MatchedRemote
        }
        (Some(_), _) => SourceVerificationState::LocalOnly,
        (None, _) => SourceVerificationState::Unavailable,
    };

    let binary_state = match (
        input.shipped_binary_sha256.as_deref(),
        input.rebuilt_binary_sha256.as_deref(),
    ) {
        (None, _) => BinaryVerificationState::NotProvided,
        (Some(_), None) => BinaryVerificationState::RebuiltNotAvailable,
        (Some(_), Some(_)) if !input.reproducible_build => {
            BinaryVerificationState::RebuiltComparisonNotApplicable
        }
        (Some(shipped), Some(rebuilt)) => {
            if policy.compare_rebuilt_binary_only_if_reproducible && !same_sha(shipped, rebuilt) {
                return Ok(quarantine(
                    FailureCategory::RebuiltBinaryDigestMismatch,
                    Some(shipped),
                    Some(rebuilt),
                ));
            }
            BinaryVerificationState::RebuiltMatch
        }
    };

    Ok(AttestationResult {
        disposition: ActivationDisposition::Activate,
        source_state,
        binary_state,
        failure: None,
    })
}

fn quarantine(
    category: FailureCategory,
    expected: Option<&str>,
    actual: Option<&str>,
) -> AttestationResult {
    AttestationResult {
        disposition: ActivationDisposition::Quarantine,
        source_state: SourceVerificationState::Unavailable,
        binary_state: BinaryVerificationState::NotProvided,
        failure: Some(AttestationFailure {
            category,
            expected_sha256: expected.map(|value| value.to_ascii_lowercase()),
            actual_sha256: actual.map(|value| value.to_ascii_lowercase()),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KastrickPackageFailureReport {
    pub format: u32,
    pub package: String,
    pub version: String,
    pub category: FailureCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_sha256: Option<String>,
    pub build_id: String,
    pub host: String,
}

impl KastrickPackageFailureReport {
    pub fn from_result(
        input: &PackageAttestationInput,
        result: &AttestationResult,
    ) -> Result<Option<Self>, AttestationError> {
        input.validate()?;
        if !input.scope.may_report() {
            return Ok(None);
        }
        let Some(failure) = &result.failure else {
            return Ok(None);
        };
        Ok(Some(Self {
            format: PACKAGE_FAILURE_REPORT_FORMAT,
            package: input.package.clone(),
            version: input.version.clone(),
            category: failure.category,
            expected_sha256: failure.expected_sha256.clone(),
            actual_sha256: failure.actual_sha256.clone(),
            build_id: input.build_id.clone(),
            host: input.host.clone(),
        }))
    }

    pub fn endpoint(kastrick_registry: &str) -> Result<Url, AttestationError> {
        let mut base = parse_https_url(kastrick_registry)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        base.set_query(None);
        base.set_fragment(None);
        base.join(KASTRICK_PACKAGE_FAILURE_REPORT_PATH.trim_start_matches('/'))
            .map_err(AttestationError::JoinUrl)
    }
}

fn parse_https_url(value: &str) -> Result<Url, AttestationError> {
    let url = Url::parse(value).map_err(|source| AttestationError::InvalidUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AttestationError::HttpsRequired(value.to_string()));
    }
    Ok(url)
}

fn validate_optional_sha(value: &Option<String>) -> Result<(), AttestationError> {
    if let Some(value) = value {
        validate_sha256(value)?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), AttestationError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AttestationError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn same_sha(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn validate_name(value: &str) -> Result<(), AttestationError> {
    if value.is_empty()
        || value.len() > 192
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(AttestationError::InvalidPackageName(value.to_string()));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str) -> Result<(), AttestationError> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(AttestationError::InvalidText(field));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AttestationError {
    #[error("invalid package name {0:?}")]
    InvalidPackageName(String),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("{0} must be non-empty, bounded, and free of control characters")]
    InvalidText(&'static str),
    #[error("invalid URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("Kastrick attestation endpoint must use HTTPS: {0:?}")]
    HttpsRequired(String),
    #[error("could not construct Kastrick failure endpoint: {0}")]
    JoinUrl(#[source] url::ParseError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PackageAttestationInput {
        PackageAttestationInput {
            package: "advancenet".into(),
            version: "4.0.1".into(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: "a".repeat(64),
            downloaded_artifact_sha256: "a".repeat(64),
            local_source_sha256: Some("b".repeat(64)),
            remote_source_sha256: Some("b".repeat(64)),
            publisher_signature_declared: true,
            publisher_signature_verified: true,
            reproducible_build: true,
            shipped_binary_sha256: Some("c".repeat(64)),
            rebuilt_binary_sha256: Some("c".repeat(64)),
            build_id: "rust-1.98.1-windows-x86_64".into(),
            host: "windows-x86_64".into(),
        }
    }

    #[test]
    fn verified_source_and_reproducible_binary_can_activate() {
        let result = attest(&input(), AttestationPolicy::default()).unwrap();
        assert!(result.can_activate());
        assert_eq!(result.source_state, SourceVerificationState::MatchedRemote);
        assert_eq!(result.binary_state, BinaryVerificationState::RebuiltMatch);
    }

    #[test]
    fn artifact_mismatch_quarantines_before_source_comparison() {
        let mut input = input();
        input.downloaded_artifact_sha256 = "d".repeat(64);
        let result = attest(&input, AttestationPolicy::default()).unwrap();
        assert!(!result.can_activate());
        assert_eq!(
            result.failure.unwrap().category,
            FailureCategory::ArtifactDigestMismatch
        );
    }

    #[test]
    fn local_remote_source_mismatch_is_reportable_for_public_packages() {
        let mut input = input();
        input.remote_source_sha256 = Some("d".repeat(64));
        let result = attest(&input, AttestationPolicy::default()).unwrap();
        let report = KastrickPackageFailureReport::from_result(&input, &result)
            .unwrap()
            .unwrap();
        assert_eq!(report.category, FailureCategory::SourceDigestMismatch);
        assert_eq!(report.package, "advancenet");
    }

    #[test]
    fn private_or_local_failures_are_never_silently_reported() {
        let mut input = input();
        input.scope = ReportScope::PrivateOrLocal;
        input.downloaded_artifact_sha256 = "d".repeat(64);
        let result = attest(&input, AttestationPolicy::default()).unwrap();
        assert!(KastrickPackageFailureReport::from_result(&input, &result)
            .unwrap()
            .is_none());
    }

    #[test]
    fn non_reproducible_builds_do_not_claim_binary_byte_equality() {
        let mut input = input();
        input.reproducible_build = false;
        input.rebuilt_binary_sha256 = Some("d".repeat(64));
        let result = attest(&input, AttestationPolicy::default()).unwrap();
        assert!(result.can_activate());
        assert_eq!(
            result.binary_state,
            BinaryVerificationState::RebuiltComparisonNotApplicable
        );
    }
}
