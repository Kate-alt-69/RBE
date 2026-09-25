//! Typed wire contract for the Kastrick public package registry.
//!
//! This module performs no network I/O. It validates registry request URLs and
//! bounded package-index JSON before metadata is handed to the deterministic
//! resolver or trusted installer execution layer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use url::Url;

pub const REGISTRY_PACKAGE_INDEX_FORMAT: u32 = 1;
pub const KASTRICK_PACKAGE_INDEX_PREFIX: &str = "/registry/v1/packages";
pub const MAX_REGISTRY_RELEASES: usize = 4096;
pub const MAX_RELEASE_DEPENDENCIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryPackageIndexRequest {
    pub package: String,
    pub endpoint: Url,
}

impl RegistryPackageIndexRequest {
    pub fn new(registry_base: &str, package: &str) -> Result<Self, RegistryContractError> {
        validate_registry_package_name(package)?;
        let base = parse_https_base(registry_base)?;
        let relative = format!(
            "{}/{}/index.json",
            KASTRICK_PACKAGE_INDEX_PREFIX.trim_matches('/'),
            package
        );
        let endpoint = base
            .join(&relative)
            .map_err(RegistryContractError::JoinUrl)?;
        Ok(Self {
            package: package.to_string(),
            endpoint,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryPackageIndex {
    pub format: u32,
    pub package: String,
    pub releases: Vec<RegistryPackageRelease>,
}

impl RegistryPackageIndex {
    pub fn parse_json(input: &str, expected_package: &str) -> Result<Self, RegistryContractError> {
        let index: Self = serde_json::from_str(input).map_err(RegistryContractError::Json)?;
        index.validate_for(expected_package)?;
        Ok(index)
    }

    pub fn validate_for(&self, expected_package: &str) -> Result<(), RegistryContractError> {
        if self.format != REGISTRY_PACKAGE_INDEX_FORMAT {
            return Err(RegistryContractError::UnsupportedFormat(self.format));
        }
        validate_registry_package_name(expected_package)?;
        validate_registry_package_name(&self.package)?;
        if self.package != expected_package {
            return Err(RegistryContractError::PackageMismatch {
                expected: expected_package.to_string(),
                actual: self.package.clone(),
            });
        }
        if self.releases.len() > MAX_REGISTRY_RELEASES {
            return Err(RegistryContractError::TooManyReleases(self.releases.len()));
        }

        let mut versions = BTreeSet::new();
        for release in &self.releases {
            release.validate()?;
            if !versions.insert(release.version.clone()) {
                return Err(RegistryContractError::DuplicateRelease(
                    release.version.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryPackageRelease {
    pub version: String,
    pub rbe_abi_min: u32,
    pub rbe_abi_max: u32,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    pub artifact: RegistryArtifact,
}

impl RegistryPackageRelease {
    pub fn validate(&self) -> Result<(), RegistryContractError> {
        validate_version_text(&self.version)?;
        if self.rbe_abi_min == 0 || self.rbe_abi_min > self.rbe_abi_max {
            return Err(RegistryContractError::InvalidAbiRange {
                min: self.rbe_abi_min,
                max: self.rbe_abi_max,
            });
        }
        if self.dependencies.len() > MAX_RELEASE_DEPENDENCIES {
            return Err(RegistryContractError::TooManyDependencies(
                self.dependencies.len(),
            ));
        }
        for (package, requirement) in &self.dependencies {
            validate_registry_package_name(package)?;
            validate_requirement(requirement)?;
        }
        self.artifact.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryArtifact {
    pub source: String,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default)]
    pub reproducible_build: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipped_binary_sha256: Option<String>,
}

impl RegistryArtifact {
    pub fn validate(&self) -> Result<(), RegistryContractError> {
        parse_https_url(&self.source)?;
        validate_sha256(&self.sha256)?;
        if self.size_bytes == 0 {
            return Err(RegistryContractError::InvalidArtifactSize);
        }
        if let Some(value) = &self.source_sha256 {
            validate_sha256(value)?;
        }
        if let Some(value) = &self.shipped_binary_sha256 {
            validate_sha256(value)?;
        }
        if let Some(value) = &self.publisher {
            validate_bounded_text("publisher", value, 512)?;
        }
        if let Some(value) = &self.signature {
            validate_bounded_text("signature", value, 4096)?;
        }
        Ok(())
    }

    pub fn source_url(&self) -> Result<Url, RegistryContractError> {
        parse_https_url(&self.source)
    }
}

pub fn validate_registry_package_name(value: &str) -> Result<(), RegistryContractError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(RegistryContractError::InvalidPackageName(value.to_string()));
    };
    let valid = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_');
    if !valid || value.len() > 96 {
        return Err(RegistryContractError::InvalidPackageName(value.to_string()));
    }
    Ok(())
}

fn validate_version_text(value: &str) -> Result<(), RegistryContractError> {
    if value.is_empty()
        || value.len() > 64
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
    {
        return Err(RegistryContractError::InvalidVersion(value.to_string()));
    }
    Ok(())
}

fn validate_requirement(value: &str) -> Result<(), RegistryContractError> {
    if value.trim().is_empty() || value.len() > 192 || value.chars().any(char::is_control) {
        return Err(RegistryContractError::InvalidRequirement(value.to_string()));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), RegistryContractError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RegistryContractError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn validate_bounded_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), RegistryContractError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(RegistryContractError::InvalidText(field));
    }
    Ok(())
}

fn parse_https_base(value: &str) -> Result<Url, RegistryContractError> {
    let mut url = parse_https_url(value)?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn parse_https_url(value: &str) -> Result<Url, RegistryContractError> {
    let url = Url::parse(value).map_err(|source| RegistryContractError::InvalidUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(RegistryContractError::HttpsRequired(value.to_string()));
    }
    Ok(url)
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryContractError {
    #[error("invalid registry package name {0:?}")]
    InvalidPackageName(String),
    #[error("invalid registry URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("registry URLs must use HTTPS without credentials or fragments: {0:?}")]
    HttpsRequired(String),
    #[error("could not construct package-index URL: {0}")]
    JoinUrl(#[source] url::ParseError),
    #[error("invalid registry package-index JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("unsupported registry package-index format {0}")]
    UnsupportedFormat(u32),
    #[error("registry returned package {actual:?}, expected {expected:?}")]
    PackageMismatch { expected: String, actual: String },
    #[error("registry package index contains too many releases: {0}")]
    TooManyReleases(usize),
    #[error("registry package index contains duplicate release {0:?}")]
    DuplicateRelease(String),
    #[error("invalid registry package version {0:?}")]
    InvalidVersion(String),
    #[error("invalid registry dependency requirement {0:?}")]
    InvalidRequirement(String),
    #[error("invalid registry release ABI range {min}..={max}")]
    InvalidAbiRange { min: u32, max: u32 },
    #[error("registry release contains too many dependencies: {0}")]
    TooManyDependencies(usize),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("registry artifact size must be greater than zero")]
    InvalidArtifactSize,
    #[error("registry artifact {0} must be non-empty, bounded, and free of control characters")]
    InvalidText(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_index() -> String {
        format!(
            r#"{{
  "format": 1,
  "package": "advancenet",
  "releases": [{{
    "version": "4.0.1",
    "rbe_abi_min": 1,
    "rbe_abi_max": 1,
    "dependencies": {{"transport": "^2"}},
    "artifact": {{
      "source": "https://cdn.kastrick.invalid/advancenet-4.0.1.rbe-pkg",
      "sha256": "{}",
      "size_bytes": 1024,
      "source_sha256": "{}",
      "publisher": "kastrick"
    }}
  }}]
}}"#,
            "a".repeat(64),
            "b".repeat(64)
        )
    }

    #[test]
    fn request_uses_bounded_package_path_under_registry_v1() {
        let request = RegistryPackageIndexRequest::new(
            "https://registry.kastrick.invalid/api/",
            "advancenet",
        )
        .unwrap();
        assert_eq!(
            request.endpoint.as_str(),
            "https://registry.kastrick.invalid/api/registry/v1/packages/advancenet/index.json"
        );
    }

    #[test]
    fn package_index_accepts_typed_release_and_artifact_metadata() {
        let parsed = RegistryPackageIndex::parse_json(&valid_index(), "advancenet").unwrap();
        assert_eq!(parsed.releases.len(), 1);
        assert_eq!(parsed.releases[0].dependencies["transport"], "^2");
        assert!(parsed.releases[0].artifact.source_url().is_ok());
    }

    #[test]
    fn package_identity_cannot_be_swapped_by_registry_response() {
        let source = valid_index().replace("\"package\": \"advancenet\"", "\"package\": \"evil\"");
        assert!(matches!(
            RegistryPackageIndex::parse_json(&source, "advancenet"),
            Err(RegistryContractError::PackageMismatch { .. })
        ));
    }

    #[test]
    fn package_artifacts_must_be_https() {
        let source = valid_index().replace(
            "https://cdn.kastrick.invalid/advancenet-4.0.1.rbe-pkg",
            "http://cdn.kastrick.invalid/advancenet-4.0.1.rbe-pkg",
        );
        assert!(matches!(
            RegistryPackageIndex::parse_json(&source, "advancenet"),
            Err(RegistryContractError::HttpsRequired(_))
        ));
    }

    #[test]
    fn duplicate_release_versions_are_rejected() {
        let mut parsed = RegistryPackageIndex::parse_json(&valid_index(), "advancenet").unwrap();
        parsed.releases.push(parsed.releases[0].clone());
        assert!(matches!(
            parsed.validate_for("advancenet"),
            Err(RegistryContractError::DuplicateRelease(_))
        ));
    }
}
