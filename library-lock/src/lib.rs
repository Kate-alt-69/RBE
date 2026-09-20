//! Deterministic `rbe.lock` model for resolved external libraries.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use rbe_library_package::LibraryManifest;
use serde::{Deserialize, Serialize};

pub const RBE_LOCK_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RbeLock {
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LockedPackage>,
}

impl Default for RbeLock {
    fn default() -> Self {
        Self {
            format: RBE_LOCK_FORMAT,
            packages: BTreeMap::new(),
        }
    }
}

impl RbeLock {
    pub fn parse(input: &str) -> Result<Self, LockError> {
        let lock: Self = toml::from_str(input)?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn render(&self) -> Result<String, LockError> {
        self.validate()?;
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn validate(&self) -> Result<(), LockError> {
        if self.format != RBE_LOCK_FORMAT {
            return Err(LockError::UnsupportedFormat(self.format));
        }
        for (name, package) in &self.packages {
            validate_name(name)?;
            package.validate()?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        &mut self,
        manifest: &LibraryManifest,
        content_sha256: impl Into<String>,
        resolved_sdk_version: impl Into<String>,
        resolved_runtime_version: impl Into<String>,
        build_id: impl Into<String>,
        admitted_capabilities: impl IntoIterator<Item = String>,
        admitted_build_capabilities: impl IntoIterator<Item = String>,
        publisher: Option<String>,
        signature: Option<String>,
    ) -> Result<(), LockError> {
        let package = LockedPackage::from_resolved(
            manifest,
            content_sha256.into(),
            resolved_sdk_version.into(),
            resolved_runtime_version.into(),
            build_id.into(),
            admitted_capabilities,
            admitted_build_capabilities,
            publisher,
            signature,
        )?;
        self.packages.insert(manifest.name.clone(), package);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub version: String,
    pub content_sha256: String,
    pub rbe_abi_min: u32,
    pub rbe_abi_max: u32,
    pub sdk: LockedSdk,
    pub runtime: LockedRuntime,
    pub build_id: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub admitted_capabilities: Vec<String>,
    #[serde(default)]
    pub admitted_build_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl LockedPackage {
    #[allow(clippy::too_many_arguments)]
    pub fn from_resolved(
        manifest: &LibraryManifest,
        content_sha256: String,
        resolved_sdk_version: String,
        resolved_runtime_version: String,
        build_id: String,
        admitted_capabilities: impl IntoIterator<Item = String>,
        admitted_build_capabilities: impl IntoIterator<Item = String>,
        publisher: Option<String>,
        signature: Option<String>,
    ) -> Result<Self, LockError> {
        manifest
            .validate()
            .map_err(|error| LockError::Manifest(error.to_string()))?;
        validate_sha256(&content_sha256)?;
        require_text("resolved SDK version", &resolved_sdk_version)?;
        require_text("resolved runtime version", &resolved_runtime_version)?;
        require_text("build id", &build_id)?;

        let admitted_capabilities = validate_admission(
            "runtime capability",
            admitted_capabilities,
            &manifest.capabilities,
        )?;
        let admitted_build_capabilities = validate_admission(
            "build capability",
            admitted_build_capabilities,
            &manifest.build_capabilities,
        )?;

        let package = Self {
            version: manifest.version.clone(),
            content_sha256: content_sha256.to_ascii_lowercase(),
            rbe_abi_min: manifest.rbe_abi_min,
            rbe_abi_max: manifest.rbe_abi_max,
            sdk: LockedSdk {
                family: manifest.sdk.family.clone(),
                package: manifest.sdk.package.clone(),
                version: resolved_sdk_version,
            },
            runtime: LockedRuntime {
                kind: manifest.runtime.kind.clone(),
                version: resolved_runtime_version,
                managed: manifest.runtime.managed,
                entry: manifest.runtime.entry.clone(),
            },
            build_id,
            dependencies: manifest.dependencies.clone(),
            admitted_capabilities,
            admitted_build_capabilities,
            publisher,
            signature,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<(), LockError> {
        require_text("package version", &self.version)?;
        validate_sha256(&self.content_sha256)?;
        if self.rbe_abi_min == 0 || self.rbe_abi_min > self.rbe_abi_max {
            return Err(LockError::InvalidAbi {
                min: self.rbe_abi_min,
                max: self.rbe_abi_max,
            });
        }
        self.sdk.validate()?;
        self.runtime.validate()?;
        require_text("build id", &self.build_id)?;
        validate_sorted_unique("admitted_capabilities", &self.admitted_capabilities)?;
        validate_sorted_unique(
            "admitted_build_capabilities",
            &self.admitted_build_capabilities,
        )?;
        for (name, requirement) in &self.dependencies {
            validate_name(name)?;
            require_text("dependency requirement", requirement)?;
        }
        if let Some(publisher) = &self.publisher {
            require_text("publisher", publisher)?;
        }
        if let Some(signature) = &self.signature {
            require_text("signature", signature)?;
        }
        Ok(())
    }

    pub fn matches_manifest(&self, manifest: &LibraryManifest) -> Result<(), LockError> {
        manifest
            .validate()
            .map_err(|error| LockError::Manifest(error.to_string()))?;
        if self.version != manifest.version
            || self.rbe_abi_min != manifest.rbe_abi_min
            || self.rbe_abi_max != manifest.rbe_abi_max
            || self.sdk.family != manifest.sdk.family
            || self.sdk.package != manifest.sdk.package
            || self.runtime.kind != manifest.runtime.kind
            || self.runtime.managed != manifest.runtime.managed
            || self.runtime.entry != manifest.runtime.entry
            || self.dependencies != manifest.dependencies
        {
            return Err(LockError::ManifestDrift(manifest.name.clone()));
        }
        for capability in &self.admitted_capabilities {
            if manifest.capabilities.get(capability) != Some(&true) {
                return Err(LockError::ManifestDrift(manifest.name.clone()));
            }
        }
        for capability in &self.admitted_build_capabilities {
            if manifest.build_capabilities.get(capability) != Some(&true) {
                return Err(LockError::ManifestDrift(manifest.name.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSdk {
    pub family: String,
    pub package: String,
    pub version: String,
}

impl LockedSdk {
    fn validate(&self) -> Result<(), LockError> {
        require_text("SDK family", &self.family)?;
        require_text("SDK package", &self.package)?;
        require_text("SDK version", &self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedRuntime {
    pub kind: String,
    pub version: String,
    pub managed: bool,
    pub entry: String,
}

impl LockedRuntime {
    fn validate(&self) -> Result<(), LockError> {
        validate_runtime_component("runtime kind", &self.kind)?;
        validate_runtime_component("runtime version", &self.version)?;
        validate_entry(&self.entry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLayout {
    root: PathBuf,
}

impl ProjectLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("rbe.lock")
    }

    pub fn rbe_dir(&self) -> PathBuf {
        self.root.join(".rbe")
    }

    pub fn library_cache_dir(&self, sha256: &str) -> Result<PathBuf, LockError> {
        validate_sha256(sha256)?;
        Ok(self
            .rbe_dir()
            .join("library-cache")
            .join(sha256.to_ascii_lowercase()))
    }

    pub fn runtime_dir(&self, kind: &str, version: &str) -> Result<PathBuf, LockError> {
        validate_runtime_component("runtime kind", kind)?;
        validate_runtime_component("runtime version", version)?;
        Ok(self.rbe_dir().join("runtimes").join(kind).join(version))
    }

    pub fn sdk_dir(&self, family: &str, version: &str) -> Result<PathBuf, LockError> {
        validate_runtime_component("SDK family", family)?;
        validate_runtime_component("SDK version", version)?;
        Ok(self.rbe_dir().join("sdk").join(family).join(version))
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.rbe_dir().join("registry")
    }
}

fn validate_admission(
    kind: &'static str,
    values: impl IntoIterator<Item = String>,
    requested: &BTreeMap<String, bool>,
) -> Result<Vec<String>, LockError> {
    let mut admitted = BTreeSet::new();
    for value in values {
        if requested.get(&value) != Some(&true) {
            return Err(LockError::UnrequestedCapability { kind, value });
        }
        admitted.insert(value);
    }
    Ok(admitted.into_iter().collect())
}

fn validate_sorted_unique(field: &'static str, values: &[String]) -> Result<(), LockError> {
    let mut previous: Option<&str> = None;
    for value in values {
        require_text(field, value)?;
        if let Some(prev) = previous {
            if prev >= value.as_str() {
                return Err(LockError::NonCanonicalList(field));
            }
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), LockError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LockError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn validate_name(value: &str) -> Result<(), LockError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(LockError::InvalidName(value.to_string()));
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit())
        || !chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    {
        return Err(LockError::InvalidName(value.to_string()));
    }
    Ok(())
}

fn validate_runtime_component(field: &'static str, value: &str) -> Result<(), LockError> {
    require_text(field, value)?;
    if value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err(LockError::InvalidRuntimeComponent {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_entry(value: &str) -> Result<(), LockError> {
    require_text("runtime entry", value)?;
    let path = Path::new(value);
    if path.is_absolute()
        || value.contains('\\')
        || value.contains(':')
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LockError::InvalidEntry(value.to_string()));
    }
    Ok(())
}

fn require_text(field: &'static str, value: &str) -> Result<(), LockError> {
    if value.trim().is_empty() || value.len() > 512 {
        return Err(LockError::InvalidText(field));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("invalid rbe.lock TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize rbe.lock: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported rbe.lock format {0}")]
    UnsupportedFormat(u32),
    #[error("invalid package name {0:?}")]
    InvalidName(String),
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("invalid ABI range {min}..={max}")]
    InvalidAbi { min: u32, max: u32 },
    #[error("{0} must be non-empty and bounded")]
    InvalidText(&'static str),
    #[error("invalid {field}: {value:?}")]
    InvalidRuntimeComponent { field: &'static str, value: String },
    #[error("invalid runtime entry {0:?}")]
    InvalidEntry(String),
    #[error("{kind} {value:?} was not requested by the package manifest")]
    UnrequestedCapability { kind: &'static str, value: String },
    #[error("{0} must be sorted and duplicate-free")]
    NonCanonicalList(&'static str),
    #[error("package manifest is invalid: {0}")]
    Manifest(String),
    #[error("locked package no longer matches manifest {0:?}")]
    ManifestDrift(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "1.0.0"
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

[build_capabilities]
"network:dependencies" = true
"#;

    #[test]
    fn lock_round_trip_pins_resolved_runtime_and_capabilities() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let mut lock = RbeLock::default();
        lock.admit(
            &manifest,
            "a".repeat(64),
            "0.1.7",
            "1.2.3",
            "build-abc",
            vec!["net:http".into()],
            vec!["network:dependencies".into()],
            Some("kastrick".into()),
            Some("sig:v1:abc".into()),
        )
        .unwrap();

        let rendered = lock.render().unwrap();
        let parsed = RbeLock::parse(&rendered).unwrap();
        let package = parsed.packages.get("advancenet").unwrap();
        assert_eq!(package.runtime.version, "1.2.3");
        assert_eq!(package.sdk.version, "0.1.7");
        assert_eq!(package.admitted_capabilities, ["net:http"]);
        package.matches_manifest(&manifest).unwrap();
    }

    #[test]
    fn admission_cannot_grant_unrequested_authority() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let error = LockedPackage::from_resolved(
            &manifest,
            "b".repeat(64),
            "0.1.0".into(),
            "1.0.0".into(),
            "build-1".into(),
            vec!["router:register".into()],
            Vec::new(),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, LockError::UnrequestedCapability { .. }));
    }

    #[test]
    fn project_layout_is_local_and_content_addressed() {
        let layout = ProjectLayout::new("/project");
        assert_eq!(layout.lock_path(), Path::new("/project").join("rbe.lock"));
        assert_eq!(
            layout.library_cache_dir(&"c".repeat(64)).unwrap(),
            Path::new("/project")
                .join(".rbe/library-cache")
                .join("c".repeat(64))
        );
        assert_eq!(
            layout.runtime_dir("bun", "1.2.3").unwrap(),
            Path::new("/project/.rbe/runtimes/bun/1.2.3")
        );
    }
}
