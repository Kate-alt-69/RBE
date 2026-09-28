//! Application-level RPX project manifest, generated lock state, and local index.
//!
//! Published `.rbe.zip` archives keep their package authoring manifest. This
//! module is only for RBE applications/projects: `package.rbe.json` plus RPX's
//! generated `.cache/package.rbe.lock.json` state.

use anyhow::{bail, Context, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROJECT_FORMAT: u32 = 1;
pub const PROJECT_MANIFEST: &str = "package.rbe.json";
pub const PROJECT_LOCK: &str = "package.rbe.lock.json";
pub const LOCAL_INDEX: &str = "index.rbe.json";

fn default_format() -> u32 {
    PROJECT_FORMAT
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    #[serde(default = "default_format")]
    pub format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, ProjectPackageRequirement>,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
}

impl ProjectManifest {
    pub fn parse(input: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(input).context("invalid package.rbe.json")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(project_root: &Path) -> Result<Self> {
        let path = project_root.join(PROJECT_MANIFEST);
        let input = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&input)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format != PROJECT_FORMAT {
            bail!(
                "unsupported package.rbe.json format {}; expected {}",
                self.format,
                PROJECT_FORMAT
            );
        }
        if let Some(name) = &self.name {
            validate_project_name(name)?;
        }
        if let Some(version) = &self.version {
            Version::parse(version)
                .with_context(|| format!("invalid project version {version:?}"))?;
        }
        for (name, requirement) in &self.packages {
            validate_package_name(name)?;
            requirement.validate(name)?;
        }
        for (name, command) in &self.scripts {
            validate_script(name, command)?;
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).context("failed to serialize package.rbe.json")
    }

    pub fn sha256(&self) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json()?);
        Ok(format!("{:x}", hasher.finalize()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjectPackageRequirement {
    Version(String),
    Detailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl ProjectPackageRequirement {
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

    fn validate(&self, package: &str) -> Result<()> {
        if self.version().is_none() && self.source().is_none() {
            bail!("package {package:?} must define a version and/or source");
        }
        if let Some(version) = self.version() {
            VersionReq::parse(version).with_context(|| {
                format!("invalid version requirement {version:?} for package {package:?}")
            })?;
        }
        if let Some(source) = self.source() {
            if source.trim().is_empty() || source.len() > 4096 {
                bail!("invalid source for package {package:?}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectLock {
    #[serde(default = "default_format")]
    pub format: u32,
    pub manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_revision: Option<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, LockedPackage>,
}

impl ProjectLock {
    pub fn new(manifest: &ProjectManifest) -> Result<Self> {
        Ok(Self {
            format: PROJECT_FORMAT,
            manifest_sha256: manifest.sha256()?,
            index_revision: None,
            packages: BTreeMap::new(),
        })
    }

    pub fn parse(input: &str) -> Result<Self> {
        let lock: Self = serde_json::from_str(input).context("invalid package.rbe.lock.json")?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn load(project_root: &Path) -> Result<Option<Self>> {
        let path = ProjectPaths::new(project_root).lock_path();
        let input = match fs::read_to_string(&path) {
            Ok(input) => input,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Self::parse(&input).map(Some)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format != PROJECT_FORMAT {
            bail!(
                "unsupported package.rbe.lock.json format {}; expected {}",
                self.format,
                PROJECT_FORMAT
            );
        }
        validate_sha256("manifest_sha256", &self.manifest_sha256)?;
        if let Some(revision) = &self.index_revision {
            if revision.trim().is_empty() || revision.len() > 256 {
                bail!("invalid local registry index revision");
            }
        }
        for (name, package) in &self.packages {
            validate_package_name(name)?;
            package.validate(name)?;
        }
        Ok(())
    }

    pub fn matches_manifest(&self, manifest: &ProjectManifest) -> Result<bool> {
        Ok(self.manifest_sha256.eq_ignore_ascii_case(&manifest.sha256()?))
    }

    pub fn render_pretty(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self).context("failed to serialize package.rbe.lock.json")
    }

    pub fn write(&self, project_root: &Path) -> Result<PathBuf> {
        let paths = ProjectPaths::new(project_root);
        let path = paths.lock_path();
        fs::create_dir_all(paths.cache_root())?;
        let next = paths.cache_root().join("package.rbe.lock.json.next");
        fs::write(&next, self.render_pretty()?)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&next, &path)?;
        Ok(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub requested: String,
    pub version: String,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

impl LockedPackage {
    fn validate(&self, package: &str) -> Result<()> {
        VersionReq::parse(&self.requested).with_context(|| {
            format!("invalid locked requirement {:?} for {package:?}", self.requested)
        })?;
        Version::parse(&self.version)
            .with_context(|| format!("invalid locked version {:?} for {package:?}", self.version))?;
        if !self.artifact_url.starts_with("https://") {
            bail!("locked artifact URL for {package:?} must use HTTPS");
        }
        validate_sha256("artifact_sha256", &self.artifact_sha256)?;
        validate_sha256("manifest_sha256", &self.manifest_sha256)?;
        for (name, requirement) in &self.dependencies {
            validate_package_name(name)?;
            VersionReq::parse(requirement).with_context(|| {
                format!("invalid dependency requirement {requirement:?} for {name:?}")
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalIndex {
    #[serde(default = "default_format")]
    pub format: u32,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, LocalIndexPackage>,
}

impl LocalIndex {
    pub fn parse(input: &str) -> Result<Self> {
        let index: Self = serde_json::from_str(input).context("invalid RPX local index")?;
        index.validate()?;
        Ok(index)
    }

    pub fn load(project_root: &Path) -> Result<Option<Self>> {
        let path = ProjectPaths::new(project_root).local_index_path();
        let input = match fs::read_to_string(&path) {
            Ok(input) => input,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Self::parse(&input).map(Some)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format != PROJECT_FORMAT {
            bail!("unsupported RPX local index format {}", self.format);
        }
        if self.revision.trim().is_empty() || self.revision.len() > 256 {
            bail!("invalid RPX local index revision");
        }
        for (name, package) in &self.packages {
            validate_package_name(name)?;
            package.validate(name)?;
        }
        Ok(())
    }

    pub fn resolve(&self, package: &str, requirement: &str) -> Result<Option<String>> {
        validate_package_name(package)?;
        let requirement = VersionReq::parse(requirement)
            .with_context(|| format!("invalid version requirement {requirement:?}"))?;
        let Some(entry) = self.packages.get(package) else {
            return Ok(None);
        };
        let mut best: Option<Version> = None;
        for raw in &entry.versions {
            let version = Version::parse(raw)
                .with_context(|| format!("index contains invalid version {raw:?} for {package:?}"))?;
            if !version.pre.is_empty() || !requirement.matches(&version) {
                continue;
            }
            match &best {
                Some(current) if version <= *current => {}
                _ => best = Some(version),
            }
        }
        Ok(best.map(|version| version.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalIndexPackage {
    pub latest: String,
    #[serde(default)]
    pub versions: Vec<String>,
}

impl LocalIndexPackage {
    fn validate(&self, package: &str) -> Result<()> {
        let latest = Version::parse(&self.latest)
            .with_context(|| format!("invalid latest version for {package:?}"))?;
        if !latest.pre.is_empty() {
            bail!("latest version for {package:?} must be stable");
        }
        let mut seen = BTreeSet::new();
        for raw in &self.versions {
            Version::parse(raw)
                .with_context(|| format!("invalid indexed version {raw:?} for {package:?}"))?;
            if !seen.insert(raw.clone()) {
                bail!("duplicate indexed version {raw:?} for {package:?}");
            }
        }
        if !seen.contains(&self.latest) {
            bail!("latest version for {package:?} is missing from versions");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    root: PathBuf,
}

impl ProjectPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(PROJECT_MANIFEST)
    }

    pub fn cache_root(&self) -> PathBuf {
        self.root.join(".cache")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.cache_root().join(PROJECT_LOCK)
    }

    pub fn library_root(&self) -> PathBuf {
        self.cache_root().join("library")
    }

    pub fn local_index_path(&self) -> PathBuf {
        self.library_root().join(LOCAL_INDEX)
    }

    pub fn artifact_path(&self, sha256: &str) -> Result<PathBuf> {
        validate_sha256("artifact sha256", sha256)?;
        Ok(self
            .library_root()
            .join("artifacts")
            .join(format!("{}.rbe.zip", sha256.to_ascii_lowercase())))
    }
}

pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let mut current = if start.is_file() {
        start
            .parent()
            .context("project search path has no parent")?
            .to_path_buf()
    } else {
        start.to_path_buf()
    };
    if !current.is_absolute() {
        current = std::env::current_dir()?.join(current);
    }
    loop {
        if current.join(PROJECT_MANIFEST).is_file() {
            return Ok(current);
        }
        if !current.pop() {
            bail!("could not find {PROJECT_MANIFEST}; create it at the RBE application root");
        }
    }
}

fn validate_project_name(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > 214
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("invalid project name {value:?}");
    }
    Ok(())
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

fn validate_script(name: &str, command: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
        })
    {
        bail!("invalid RPX script name {name:?}");
    }
    if command.trim().is_empty() || command.len() > 16 * 1024 || command.contains('\0') {
        bail!("invalid command for RPX script {name:?}");
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

    #[test]
    fn application_manifest_is_json_not_archive_yaml() {
        let manifest = ProjectManifest::parse(
            r#"{
  "name": "kastrick-backend",
  "version": "1.0.0",
  "packages": {
    "advancenet": "^1.4.0",
    "rbe-tools": { "version": "^1.0.0" }
  },
  "scripts": {
    "test": "backend check --settings settings.template.json",
    "dev": "backend -debug"
  }
}"#,
        )
        .unwrap();
        assert_eq!(manifest.packages.len(), 2);
        assert_eq!(manifest.scripts["test"], "backend check --settings settings.template.json");
        assert_eq!(manifest.packages["advancenet"].version(), Some("^1.4.0"));
    }

    #[test]
    fn generated_lock_lives_under_cache_and_tracks_manifest_hash() {
        let manifest = ProjectManifest::parse(
            r#"{"packages":{"advancenet":"^1.4.0"},"scripts":{}}"#,
        )
        .unwrap();
        let lock = ProjectLock::new(&manifest).unwrap();
        assert!(lock.matches_manifest(&manifest).unwrap());
        assert_eq!(
            ProjectPaths::new("/app").lock_path(),
            PathBuf::from("/app/.cache/package.rbe.lock.json")
        );
        assert_eq!(
            ProjectPaths::new("/app").local_index_path(),
            PathBuf::from("/app/.cache/library/index.rbe.json")
        );
    }

    #[test]
    fn local_index_resolves_highest_matching_stable_version() {
        let index = LocalIndex::parse(
            r#"{
  "format": 1,
  "revision": "rev-12",
  "packages": {
    "advancenet": {
      "latest": "1.5.0",
      "versions": ["1.3.9", "1.4.0", "1.4.8", "1.5.0", "1.6.0-beta.1"]
    }
  }
}"#,
        )
        .unwrap();
        assert_eq!(
            index.resolve("advancenet", "^1.4.0").unwrap().as_deref(),
            Some("1.5.0")
        );
    }

    #[test]
    fn scripts_and_package_names_are_fail_closed() {
        assert!(ProjectManifest::parse(r#"{"scripts":{"../lol":"echo nope"}}"#).is_err());
        assert!(ProjectManifest::parse(r#"{"packages":{"Bad/Name":"1.0.0"}}"#).is_err());
    }
}
