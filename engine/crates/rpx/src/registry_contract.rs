//! Frozen Kastrick RPX registry read contract.
//!
//! The server owns the public wire shape. RPX normalizes the mostly-static
//! `?index-list` response into its local cache and validates exact package
//! responses before any artifact download is admitted.

use crate::project::{LocalIndex, LocalIndexPackage, ProjectPaths, PROJECT_FORMAT};
use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const REGISTRY_AUTHORITY: &str = "kastrick-rpx";
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexListResponse {
    pub ok: bool,
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub authority: String,
    pub revision: String,
    #[serde(rename = "generatedAt")]
    pub generated_at: Option<String>,
    pub packages: Vec<IndexListPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexListPackage {
    pub name: String,
    #[serde(rename = "latestStable")]
    pub latest_stable: Option<String>,
    pub versions: Vec<String>,
}

impl IndexListResponse {
    pub fn parse(input: &str) -> Result<Self> {
        let response: Self =
            serde_json::from_str(input).context("invalid RPX index-list response")?;
        response.validate()?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.ok {
            bail!("RPX index-list response was not successful");
        }
        if self.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!(
                "unsupported RPX registry schema {}; expected {}",
                self.schema_version,
                REGISTRY_SCHEMA_VERSION
            );
        }
        if self.authority != REGISTRY_AUTHORITY {
            bail!(
                "unexpected RPX registry authority {:?}; expected {:?}",
                self.authority,
                REGISTRY_AUTHORITY
            );
        }
        if self.revision.trim().is_empty() || self.revision.len() > 256 {
            bail!("invalid RPX registry revision");
        }

        let mut names = BTreeSet::new();
        for package in &self.packages {
            validate_package_name(&package.name)?;
            if !names.insert(package.name.clone()) {
                bail!("duplicate package {:?} in RPX index-list", package.name);
            }
            let mut versions = BTreeSet::new();
            for raw in &package.versions {
                Version::parse(raw).with_context(|| {
                    format!(
                        "invalid version {raw:?} in RPX index-list package {:?}",
                        package.name
                    )
                })?;
                if !versions.insert(raw) {
                    bail!(
                        "duplicate version {raw:?} in RPX index-list package {:?}",
                        package.name
                    );
                }
            }
            if let Some(latest) = &package.latest_stable {
                let latest_version = Version::parse(latest).with_context(|| {
                    format!(
                        "invalid latestStable {latest:?} for package {:?}",
                        package.name
                    )
                })?;
                if !latest_version.pre.is_empty() {
                    bail!("latestStable for package {:?} must be stable", package.name);
                }
                if !versions.contains(latest) {
                    bail!(
                        "latestStable {latest:?} is not listed in versions for package {:?}",
                        package.name
                    );
                }
            }
        }
        Ok(())
    }

    /// Normalize the public array-shaped wire index into RPX's local map cache.
    /// Packages without a published stable release are deliberately omitted from
    /// install resolution; they remain visible through the public registry API.
    pub fn into_local_index(self) -> Result<LocalIndex> {
        self.validate()?;
        let mut packages = BTreeMap::new();
        for package in self.packages {
            let Some(latest) = package.latest_stable else {
                continue;
            };
            packages.insert(
                package.name,
                LocalIndexPackage {
                    latest,
                    versions: package.versions,
                },
            );
        }
        let index = LocalIndex {
            format: PROJECT_FORMAT,
            revision: self.revision,
            generated_at: self.generated_at,
            packages,
        };
        index.validate()?;
        Ok(index)
    }

    pub fn persist_local_cache(self, project_root: &Path) -> Result<PathBuf> {
        let index = self.into_local_index()?;
        let paths = ProjectPaths::new(project_root);
        let path = paths.local_index_path();
        fs::create_dir_all(paths.library_root())?;
        let temporary = paths.library_root().join("index.rbe.json.next");
        fs::write(&temporary, serde_json::to_vec_pretty(&index)?)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        Ok(path)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageIndexResponse {
    pub ok: bool,
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub authority: String,
    pub revision: String,
    pub package: String,
    pub version: String,
    #[serde(rename = "resolvedFrom")]
    pub resolved_from: String,
    pub metadata: Value,
    pub release: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRegistryRelease {
    pub package: String,
    pub version: String,
    pub revision: String,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    pub artifact_size: u64,
}

impl PackageIndexResponse {
    pub fn parse_resolved(
        input: &str,
        expected_package: &str,
        expected_version: Option<&str>,
    ) -> Result<ResolvedRegistryRelease> {
        let response: Self =
            serde_json::from_str(input).context("invalid RPX package index response")?;
        response.resolve(expected_package, expected_version)
    }

    pub fn resolve(
        self,
        expected_package: &str,
        expected_version: Option<&str>,
    ) -> Result<ResolvedRegistryRelease> {
        if !self.ok {
            bail!("RPX package index response was not successful");
        }
        if self.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!(
                "unsupported RPX package index schema {}",
                self.schema_version
            );
        }
        if self.authority != REGISTRY_AUTHORITY {
            bail!(
                "unexpected RPX package index authority {:?}",
                self.authority
            );
        }
        if self.revision.trim().is_empty() || self.revision.len() > 256 {
            bail!("invalid RPX package index revision");
        }
        validate_package_name(&self.package)?;
        if self.package != expected_package {
            bail!(
                "RPX registry returned package {:?} while {:?} was requested",
                self.package,
                expected_package
            );
        }
        let response_version = Version::parse(&self.version)
            .with_context(|| format!("invalid registry package version {:?}", self.version))?;
        if let Some(expected) = expected_version {
            if self.version != expected {
                bail!(
                    "RPX registry returned version {:?} while {:?} was requested",
                    self.version,
                    expected
                );
            }
        }
        if self.resolved_from != "explicit" && self.resolved_from != "latest-stable" {
            bail!(
                "invalid RPX registry resolvedFrom value {:?}",
                self.resolved_from
            );
        }

        let release = self
            .release
            .as_object()
            .context("RPX registry release must be an object")?;
        let release_version = release
            .get("version")
            .and_then(Value::as_str)
            .context("RPX registry release is missing version")?;
        if Version::parse(release_version)? != response_version {
            bail!("RPX registry response/release version mismatch");
        }
        if release.get("published").and_then(Value::as_bool) != Some(true) {
            bail!("RPX registry release is not published");
        }
        if release.get("yanked").and_then(Value::as_bool) == Some(true) {
            bail!("RPX registry release is yanked and cannot be newly resolved");
        }

        let artifact_url = release
            .get("artifact_url")
            .and_then(Value::as_str)
            .context("RPX registry release has no artifact_url")?;
        if !artifact_url.starts_with("https://") {
            bail!("RPX registry artifact_url must use HTTPS");
        }
        let artifact_sha256 = release
            .get("package_sha256")
            .and_then(Value::as_str)
            .context("RPX registry release has no package_sha256")?;
        validate_sha256("package_sha256", artifact_sha256)?;
        let manifest_sha256 = release
            .get("manifest_sha256")
            .and_then(Value::as_str)
            .context("RPX registry release has no manifest_sha256")?;
        validate_sha256("manifest_sha256", manifest_sha256)?;
        let artifact_size = release
            .get("artifact_size")
            .and_then(Value::as_u64)
            .context("RPX registry release has no artifact_size")?;
        if artifact_size == 0 {
            bail!("RPX registry artifact_size must be greater than zero");
        }

        Ok(ResolvedRegistryRelease {
            package: self.package,
            version: self.version,
            revision: self.revision,
            artifact_url: artifact_url.to_string(),
            artifact_sha256: artifact_sha256.to_ascii_lowercase(),
            manifest_sha256: manifest_sha256.to_ascii_lowercase(),
            artifact_size,
        })
    }
}

fn validate_package_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 192 {
        bail!("invalid package name {value:?}");
    }
    for segment in value.split('.') {
        if segment.is_empty()
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("invalid package name {value:?}");
        }
    }
    Ok(())
}

fn validate_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid {field}: expected 64 hexadecimal characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_frozen_kastrick_index_list_shape() {
        let response = IndexListResponse::parse(
            r#"{
  "ok": true,
  "schemaVersion": 1,
  "authority": "kastrick-rpx",
  "revision": "rev-14",
  "generatedAt": null,
  "packages": [
    {"name":"advancenet","latestStable":"1.5.0","versions":["1.4.0","1.5.0"]},
    {"name":"unpublished","latestStable":null,"versions":["0.1.0"]}
  ]
}"#,
        )
        .unwrap();
        let local = response.into_local_index().unwrap();
        assert_eq!(local.revision, "rev-14");
        assert!(local.packages.contains_key("advancenet"));
        assert!(!local.packages.contains_key("unpublished"));
        assert_eq!(
            local.resolve("advancenet", "^1.4.0").unwrap().as_deref(),
            Some("1.5.0")
        );
    }

    #[test]
    fn exact_release_requires_verified_published_artifact_fields() {
        let package_sha = "a".repeat(64);
        let manifest_sha = "b".repeat(64);
        let input = format!(
            r#"{{
  "ok": true,
  "schemaVersion": 1,
  "authority": "kastrick-rpx",
  "revision": "rev-15",
  "package": "advancenet",
  "version": "1.5.0",
  "resolvedFrom": "explicit",
  "metadata": {{"latestStable":"1.5.0"}},
  "release": {{
    "version":"1.5.0",
    "published":true,
    "yanked":false,
    "artifact_url":"https://registry.example/advancenet-1.5.0.rbe.zip",
    "package_sha256":"{package_sha}",
    "manifest_sha256":"{manifest_sha}",
    "artifact_size":4096
  }}
}}"#
        );
        let release =
            PackageIndexResponse::parse_resolved(&input, "advancenet", Some("1.5.0")).unwrap();
        assert_eq!(release.artifact_sha256, package_sha);
        assert_eq!(release.manifest_sha256, manifest_sha);
        assert_eq!(release.artifact_size, 4096);
    }

    #[test]
    fn new_resolution_rejects_yanked_release() {
        let input = format!(
            r#"{{
  "ok": true,
  "schemaVersion": 1,
  "authority": "kastrick-rpx",
  "revision": "rev-15",
  "package": "advancenet",
  "version": "1.5.0",
  "resolvedFrom": "explicit",
  "metadata": {{}},
  "release": {{
    "version":"1.5.0",
    "published":true,
    "yanked":true,
    "artifact_url":"https://registry.example/a.rbe.zip",
    "package_sha256":"{}",
    "manifest_sha256":"{}",
    "artifact_size":1
  }}
}}"#,
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(PackageIndexResponse::parse_resolved(&input, "advancenet", None).is_err());
    }

    #[test]
    fn persists_normalized_local_cache_atomically() {
        let response = IndexListResponse::parse(
            r#"{
  "ok": true,
  "schemaVersion": 1,
  "authority": "kastrick-rpx",
  "revision": "rev-cache",
  "generatedAt": "2026-09-28T10:00:00Z",
  "packages": [
    {"name":"advancenet","latestStable":"1.0.0","versions":["1.0.0"]}
  ]
}"#,
        )
        .unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rpx-index-cache-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = response.persist_local_cache(&root).unwrap();
        assert_eq!(path, root.join(".cache/library/index.rbe.json"));
        let cached = LocalIndex::load(&root).unwrap().unwrap();
        assert_eq!(cached.revision, "rev-cache");
        let _ = fs::remove_dir_all(root);
    }
}
