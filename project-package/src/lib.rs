//! Project-level RBE package manifest, lockfile, and cache layout.
//!
//! `package.rbe.yaml` is the human-edited dependency declaration.
//! `package.lock.rbe.yaml` is the deterministic resolver output used to skip
//! unnecessary registry resolution and to rehydrate a deleted package cache from
//! exact artifact URLs and integrity hashes.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

pub const PROJECT_PACKAGE_FORMAT: u32 = 1;
pub const PROJECT_PACKAGE_MANIFEST: &str = "package.rbe.yaml";
pub const PROJECT_PACKAGE_LOCK: &str = "package.lock.rbe.yaml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPackageManifest {
    pub format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectIdentity>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageRequirement>,
}

impl Default for ProjectPackageManifest {
    fn default() -> Self {
        Self {
            format: PROJECT_PACKAGE_FORMAT,
            project: None,
            packages: BTreeMap::new(),
        }
    }
}

impl ProjectPackageManifest {
    pub fn parse_yaml(input: &str) -> Result<Self, ProjectPackageError> {
        let manifest: Self = serde_yaml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn render_yaml(&self) -> Result<String, ProjectPackageError> {
        self.validate()?;
        Ok(serde_yaml::to_string(self)?)
    }

    pub fn validate(&self) -> Result<(), ProjectPackageError> {
        if self.format != PROJECT_PACKAGE_FORMAT {
            return Err(ProjectPackageError::UnsupportedFormat(self.format));
        }
        if let Some(project) = &self.project {
            project.validate()?;
        }
        for (key, requirement) in &self.packages {
            validate_install_key(key)?;
            requirement.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdentity {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl ProjectIdentity {
    fn validate(&self) -> Result<(), ProjectPackageError> {
        validate_component("project name", &self.name)?;
        if let Some(version) = &self.version {
            validate_version_requirement(version)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageRequirement {
    Version(String),
    Detailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl PackageRequirement {
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Version(version) => Some(version),
            Self::Detailed { version, .. } => version.as_deref(),
        }
    }

    pub fn source(&self) -> Option<&str> {
        match self {
            Self::Version(_) => None,
            Self::Detailed { source, .. } => source.as_deref(),
        }
    }

    fn validate(&self) -> Result<(), ProjectPackageError> {
        match self {
            Self::Version(version) => validate_version_requirement(version),
            Self::Detailed { version, source } => {
                if version.is_none() && source.is_none() {
                    return Err(ProjectPackageError::EmptyRequirement);
                }
                if let Some(version) = version {
                    validate_version_requirement(version)?;
                }
                if let Some(source) = source {
                    validate_source(source)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPackageLock {
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LockedProjectPackage>,
}

impl Default for ProjectPackageLock {
    fn default() -> Self {
        Self {
            format: PROJECT_PACKAGE_FORMAT,
            packages: BTreeMap::new(),
        }
    }
}

impl ProjectPackageLock {
    pub fn parse_yaml(input: &str) -> Result<Self, ProjectPackageError> {
        let lock: Self = serde_yaml::from_str(input)?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn render_yaml(&self) -> Result<String, ProjectPackageError> {
        self.validate()?;
        Ok(serde_yaml::to_string(self)?)
    }

    pub fn validate(&self) -> Result<(), ProjectPackageError> {
        if self.format != PROJECT_PACKAGE_FORMAT {
            return Err(ProjectPackageError::UnsupportedFormat(self.format));
        }
        for (key, package) in &self.packages {
            validate_install_key(key)?;
            package.validate()?;
        }
        Ok(())
    }

    pub fn rehydration_plan(
        &self,
        layout: &ProjectCacheLayout,
    ) -> Result<Vec<LockedArtifactFetch>, ProjectPackageError> {
        self.validate()?;
        self.packages
            .iter()
            .map(|(key, package)| {
                Ok(LockedArtifactFetch {
                    package: key.clone(),
                    version: package.version.clone(),
                    artifact_url: parse_https_url(&package.artifact_url)?,
                    artifact_sha256: package.artifact_sha256.clone(),
                    manifest_sha256: package.manifest_sha256.clone(),
                    source_sha256: package.source_sha256.clone(),
                    cache_path: layout.library_artifact_dir(&package.artifact_sha256)?,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedProjectPackage {
    pub version: String,
    pub resolved_from: String,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<LockedToolchain>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<LockedToolchain>,
}

impl LockedProjectPackage {
    fn validate(&self) -> Result<(), ProjectPackageError> {
        validate_version_requirement(&self.version)?;
        validate_source(&self.resolved_from)?;
        parse_https_url(&self.artifact_url)?;
        validate_sha256(&self.artifact_sha256)?;
        validate_sha256(&self.manifest_sha256)?;
        if let Some(source_sha256) = &self.source_sha256 {
            validate_sha256(source_sha256)?;
        }
        for (key, requirement) in &self.dependencies {
            validate_install_key(key)?;
            validate_version_requirement(requirement)?;
        }
        if let Some(runtime) = &self.runtime {
            runtime.validate("runtime")?;
        }
        if let Some(sdk) = &self.sdk {
            sdk.validate("sdk")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedToolchain {
    pub kind: String,
    pub version: String,
}

impl LockedToolchain {
    fn validate(&self, field: &'static str) -> Result<(), ProjectPackageError> {
        validate_component(field, &self.kind)?;
        validate_component(field, &self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedArtifactFetch {
    pub package: String,
    pub version: String,
    pub artifact_url: Url,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    pub source_sha256: Option<String>,
    pub cache_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCacheLayout {
    root: PathBuf,
}

impl ProjectCacheLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(PROJECT_PACKAGE_MANIFEST)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join(PROJECT_PACKAGE_LOCK)
    }

    pub fn cache_root(&self) -> PathBuf {
        self.root.join(".cache")
    }

    pub fn library_cache_root(&self) -> PathBuf {
        self.cache_root().join("library")
    }

    pub fn rbe_system_cache_root(&self) -> PathBuf {
        self.cache_root().join("rbe")
    }

    pub fn library_artifact_dir(&self, sha256: &str) -> Result<PathBuf, ProjectPackageError> {
        validate_sha256(sha256)?;
        Ok(self.library_cache_root().join(sha256.to_ascii_lowercase()))
    }
}

fn validate_install_key(value: &str) -> Result<(), ProjectPackageError> {
    if value.is_empty() || value.len() > 192 {
        return Err(ProjectPackageError::InvalidPackageKey(value.to_string()));
    }
    for segment in value.split('.') {
        if segment.is_empty()
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(ProjectPackageError::InvalidPackageKey(value.to_string()));
        }
    }
    Ok(())
}

fn validate_component(field: &'static str, value: &str) -> Result<(), ProjectPackageError> {
    if value.trim().is_empty()
        || value.len() > 256
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err(ProjectPackageError::InvalidText {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_version_requirement(value: &str) -> Result<(), ProjectPackageError> {
    if value.trim().is_empty() || value.len() > 128 {
        return Err(ProjectPackageError::InvalidVersion(value.to_string()));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'.' | b'-' | b'+' | b'^' | b'~' | b'<' | b'>' | b'=' | b',' | b'*' | b' '
            )
    }) {
        return Err(ProjectPackageError::InvalidVersion(value.to_string()));
    }
    Ok(())
}

fn validate_source(value: &str) -> Result<(), ProjectPackageError> {
    if value.trim().is_empty() || value.len() > 4096 {
        return Err(ProjectPackageError::InvalidSource(value.to_string()));
    }
    if value.contains("://") {
        parse_https_url(value)?;
    }
    Ok(())
}

fn parse_https_url(value: &str) -> Result<Url, ProjectPackageError> {
    let url = Url::parse(value).map_err(|source| ProjectPackageError::InvalidUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ProjectPackageError::HttpsRequired(value.to_string()));
    }
    Ok(url)
}

fn validate_sha256(value: &str) -> Result<(), ProjectPackageError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProjectPackageError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectPackageError {
    #[error("invalid RBE package YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("unsupported RBE package format {0}")]
    UnsupportedFormat(u32),
    #[error("invalid package key {0:?}")]
    InvalidPackageKey(String),
    #[error("package requirement must contain a version and/or source")]
    EmptyRequirement,
    #[error("invalid version requirement {0:?}")]
    InvalidVersion(String),
    #[error("invalid package source {0:?}")]
    InvalidSource(String),
    #[error("invalid {field}: {value:?}")]
    InvalidText { field: &'static str, value: String },
    #[error("invalid URL {value:?}: {source}")]
    InvalidUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("remote package artifact/source URL must use HTTPS: {0:?}")]
    HttpsRequired(String),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_manifest_supports_simple_and_detailed_requirements() {
        let manifest = ProjectPackageManifest::parse_yaml(
            r#"
format: 1
project:
  name: cool-backend
  version: 1.0.0
packages:
  advancenet: 4.0.1
  runtime.python:
    version: "3.10"
  mycoolpackage:
    version: 2.1.0
    source: https://example.com/packages/mycoolpackage/
"#,
        )
        .unwrap();
        assert_eq!(manifest.packages.len(), 3);
        assert_eq!(manifest.packages["runtime.python"].version(), Some("3.10"));
    }

    #[test]
    fn lock_rehydrates_deleted_cache_without_reresolving_versions() {
        let lock = ProjectPackageLock::parse_yaml(&format!(
            r#"
format: 1
packages:
  advancenet:
    version: 4.0.1
    resolved_from: registry:advancenet
    artifact_url: https://cdn.kastrick.invalid/advancenet-4.0.1.zip
    artifact_sha256: {artifact}
    manifest_sha256: {manifest}
    source_sha256: {source}
    runtime:
      kind: rust
      version: 1.98.1
"#,
            artifact = "a".repeat(64),
            manifest = "b".repeat(64),
            source = "c".repeat(64),
        ))
        .unwrap();
        let plan = lock
            .rehydration_plan(&ProjectCacheLayout::new("/work/backend"))
            .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].cache_path,
            PathBuf::from("/work/backend/.cache/library").join("a".repeat(64))
        );
        assert_eq!(
            plan[0].artifact_url.as_str(),
            "https://cdn.kastrick.invalid/advancenet-4.0.1.zip"
        );
    }

    #[test]
    fn canonical_paths_match_package_and_cache_names() {
        let layout = ProjectCacheLayout::new("/project");
        assert_eq!(
            layout.manifest_path(),
            PathBuf::from("/project/package.rbe.yaml")
        );
        assert_eq!(
            layout.lock_path(),
            PathBuf::from("/project/package.lock.rbe.yaml")
        );
        assert_eq!(
            layout.library_cache_root(),
            PathBuf::from("/project/.cache/library")
        );
        assert_eq!(
            layout.rbe_system_cache_root(),
            PathBuf::from("/project/.cache/rbe")
        );
    }
}
