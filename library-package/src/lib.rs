//! Shared parser and archive inspector for RBE external-library packages.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::path::{Component, Path};

use serde::Deserialize;

pub const LIBRARY_MANIFEST: &str = "library.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Javascript,
    Python,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    Linux,
    Macos,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkSpec {
    pub family: String,
    pub package: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    pub kind: String,
    pub version: String,
    #[serde(default = "default_true")]
    pub managed: bool,
    pub entry: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExportSpec {
    pub root: bool,
    pub sublibraries: Vec<String>,
}

impl Default for ExportSpec {
    fn default() -> Self {
        Self {
            root: true,
            sublibraries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildStep {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BuildSpec {
    pub windows: Vec<BuildStep>,
    pub linux: Vec<BuildStep>,
    pub macos: Vec<BuildStep>,
    pub other: Vec<BuildStep>,
}

impl BuildSpec {
    pub fn for_host(&self, host: HostOs) -> &[BuildStep] {
        let exact = match host {
            HostOs::Windows => &self.windows,
            HostOs::Linux => &self.linux,
            HostOs::Macos => &self.macos,
            HostOs::Other => &self.other,
        };
        if exact.is_empty() && host != HostOs::Other {
            &self.other
        } else {
            exact
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryManifest {
    pub name: String,
    pub version: String,
    pub language: Language,
    pub rbe_abi_min: u32,
    pub rbe_abi_max: u32,
    pub sdk: SdkSpec,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub exports: ExportSpec,
    #[serde(default)]
    pub capabilities: BTreeMap<String, bool>,
    #[serde(default)]
    pub build_capabilities: BTreeMap<String, bool>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub build: BuildSpec,
}

impl LibraryManifest {
    pub fn parse(input: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        validate_component("package name", &self.name)?;
        require_text("version", &self.version)?;
        if self.rbe_abi_min == 0 || self.rbe_abi_min > self.rbe_abi_max {
            return Err(ManifestError::InvalidAbi {
                min: self.rbe_abi_min,
                max: self.rbe_abi_max,
            });
        }

        require_text("sdk.family", &self.sdk.family)?;
        require_text("sdk.package", &self.sdk.package)?;
        require_text("sdk.version", &self.sdk.version)?;
        require_text("runtime.kind", &self.runtime.kind)?;
        require_text("runtime.version", &self.runtime.version)?;
        validate_relative_path("runtime.entry", &self.runtime.entry)?;

        match self.language {
            Language::Rust if self.runtime.kind != "rust" => {
                return Err(ManifestError::RuntimeLanguageMismatch)
            }
            Language::Javascript if !matches!(self.runtime.kind.as_str(), "bun" | "node") => {
                return Err(ManifestError::RuntimeLanguageMismatch)
            }
            Language::Python if self.runtime.kind != "python" => {
                return Err(ManifestError::RuntimeLanguageMismatch)
            }
            _ => {}
        }

        match self.language {
            Language::Rust if self.sdk.family != "rust" => {
                return Err(ManifestError::SdkLanguageMismatch)
            }
            Language::Javascript if self.sdk.family != "javascript" => {
                return Err(ManifestError::SdkLanguageMismatch)
            }
            Language::Python if self.sdk.family != "python" => {
                return Err(ManifestError::SdkLanguageMismatch)
            }
            _ => {}
        }

        let mut exports = BTreeSet::new();
        for sublibrary in &self.exports.sublibraries {
            validate_component("export sublibrary", sublibrary)?;
            if !exports.insert(sublibrary) {
                return Err(ManifestError::DuplicateExport(sublibrary.clone()));
            }
        }

        for key in self
            .capabilities
            .keys()
            .chain(self.build_capabilities.keys())
        {
            validate_capability(key)?;
        }
        for (name, requirement) in &self.dependencies {
            validate_component("dependency", name)?;
            require_text("dependency version", requirement)?;
        }
        for step in self
            .build
            .windows
            .iter()
            .chain(&self.build.linux)
            .chain(&self.build.macos)
            .chain(&self.build.other)
        {
            validate_build_step(step)?;
        }
        Ok(())
    }

    pub const fn supports_abi(&self, abi: u32) -> bool {
        abi >= self.rbe_abi_min && abi <= self.rbe_abi_max
    }
}

fn default_true() -> bool {
    true
}

fn require_text(field: &'static str, value: &str) -> Result<(), ManifestError> {
    if value.trim().is_empty() || value.len() > 256 {
        return Err(ManifestError::InvalidText(field));
    }
    Ok(())
}

fn validate_component(field: &'static str, value: &str) -> Result<(), ManifestError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ManifestError::InvalidComponent(field, value.to_string()));
    };
    let valid = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_');
    if !valid || value.len() > 96 {
        return Err(ManifestError::InvalidComponent(field, value.to_string()));
    }
    Ok(())
}

fn validate_capability(value: &str) -> Result<(), ManifestError> {
    if value.is_empty() || value.len() > 192 {
        return Err(ManifestError::InvalidCapability(value.to_string()));
    }
    for part in value.split(':') {
        validate_component("capability", part)
            .map_err(|_| ManifestError::InvalidCapability(value.to_string()))?;
    }
    Ok(())
}

fn validate_relative_path(field: &'static str, value: &str) -> Result<(), ManifestError> {
    if value.is_empty() || value.contains('\\') || value.contains(':') {
        return Err(ManifestError::InvalidPath(field, value.to_string()));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ManifestError::InvalidPath(field, value.to_string()));
    }
    Ok(())
}

fn validate_build_step(step: &BuildStep) -> Result<(), ManifestError> {
    if step.program.is_empty()
        || step.program.len() > 128
        || step.program.contains('/')
        || step.program.contains('\\')
        || step.program.contains(':')
        || matches!(step.program.as_str(), "." | "..")
    {
        return Err(ManifestError::InvalidBuildProgram(step.program.clone()));
    }
    if step.args.len() > 128 || step.args.iter().any(|arg| arg.len() > 4096) {
        return Err(ManifestError::InvalidBuildArguments);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0} must be non-empty and bounded")]
    InvalidText(&'static str),
    #[error("invalid {0}: {1:?}")]
    InvalidComponent(&'static str, String),
    #[error("invalid ABI range {min}..={max}")]
    InvalidAbi { min: u32, max: u32 },
    #[error("runtime.kind is incompatible with language")]
    RuntimeLanguageMismatch,
    #[error("sdk.family is incompatible with language")]
    SdkLanguageMismatch,
    #[error("duplicate exported sublibrary {0:?}")]
    DuplicateExport(String),
    #[error("invalid capability {0:?}")]
    InvalidCapability(String),
    #[error("invalid {0} path {1:?}")]
    InvalidPath(&'static str, String),
    #[error("invalid build program {0:?}; build steps use a logical executable name, not a path")]
    InvalidBuildProgram(String),
    #[error("build step argument limits exceeded")]
    InvalidBuildArguments,
}

#[derive(Debug, Clone, Copy)]
pub struct ArchivePolicy {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    pub max_manifest_bytes: u64,
}

impl Default for ArchivePolicy {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_entry_bytes: 64 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_manifest_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub size: u64,
    pub directory: bool,
}

#[derive(Debug)]
pub struct InspectedPackage {
    pub manifest: LibraryManifest,
    pub entries: Vec<ArchiveEntry>,
    pub total_uncompressed_bytes: u64,
}

pub fn inspect_zip<R: Read + Seek>(
    reader: R,
    policy: ArchivePolicy,
) -> Result<InspectedPackage, ArchiveError> {
    let mut archive = zip::ZipArchive::new(reader)?;
    if archive.len() > policy.max_entries {
        return Err(ArchiveError::TooManyEntries(archive.len()));
    }

    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(archive.len());
    let mut total = 0_u64;
    let mut manifest_source = None;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let raw = file.name().to_string();
        let canonical = canonical_archive_path(&raw)?;
        let collision_key = canonical.to_ascii_lowercase();
        if !seen.insert(collision_key) {
            return Err(ArchiveError::DuplicatePath(canonical));
        }

        if let Some(mode) = file.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(ArchiveError::Symlink(canonical));
            }
        }

        let size = file.size();
        if size > policy.max_entry_bytes {
            return Err(ArchiveError::EntryTooLarge(canonical));
        }
        total = total.checked_add(size).ok_or(ArchiveError::TotalTooLarge)?;
        if total > policy.max_total_bytes {
            return Err(ArchiveError::TotalTooLarge);
        }

        let directory = file.is_dir();
        if canonical == LIBRARY_MANIFEST {
            if directory || size > policy.max_manifest_bytes {
                return Err(ArchiveError::InvalidManifestFile);
            }
            let mut bytes = Vec::with_capacity(size as usize);
            file.by_ref()
                .take(policy.max_manifest_bytes + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > policy.max_manifest_bytes {
                return Err(ArchiveError::InvalidManifestFile);
            }
            manifest_source =
                Some(String::from_utf8(bytes).map_err(|_| ArchiveError::ManifestUtf8)?);
        }

        entries.push(ArchiveEntry {
            path: canonical,
            size,
            directory,
        });
    }

    let source = manifest_source.ok_or(ArchiveError::MissingManifest)?;
    let manifest = LibraryManifest::parse(&source)?;
    Ok(InspectedPackage {
        manifest,
        entries,
        total_uncompressed_bytes: total,
    })
}

fn canonical_archive_path(raw: &str) -> Result<String, ArchiveError> {
    if raw.is_empty() || raw.contains('\\') || raw.starts_with('/') || raw.contains(':') {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }
    let directory = raw.ends_with('/');
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(ArchiveError::UnsafePath(raw.to_string()));
        }
        parts.push(part);
    }
    let mut path = parts.join("/");
    if directory {
        path.push('/');
    }
    Ok(path)
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("invalid zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("zip IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("package manifest is invalid: {0}")]
    Manifest(#[from] ManifestError),
    #[error("package archive has {0} entries, above the configured limit")]
    TooManyEntries(usize),
    #[error("unsafe package path {0:?}")]
    UnsafePath(String),
    #[error("duplicate/case-colliding package path {0:?}")]
    DuplicatePath(String),
    #[error("symlinks are not accepted in source packages: {0:?}")]
    Symlink(String),
    #[error("package entry exceeds the per-file size limit: {0:?}")]
    EntryTooLarge(String),
    #[error("package uncompressed size exceeds the configured total limit")]
    TotalTooLarge,
    #[error("library.toml must be a bounded regular UTF-8 file")]
    InvalidManifestFile,
    #[error("library.toml is not UTF-8")]
    ManifestUtf8,
    #[error("package is missing root library.toml")]
    MissingManifest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    const VALID: &str = r#"
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

[exports]
sublibraries = ["retry", "mesh"]

[capabilities]
"net:http" = true
"router:register" = false

[[build.windows]]
program = "bun"
args = ["install", "--frozen-lockfile"]

[[build.other]]
program = "bun"
args = ["install"]
"#;

    #[test]
    fn parses_and_selects_exact_or_other_build_steps() {
        let manifest = LibraryManifest::parse(VALID).unwrap();
        assert!(manifest.supports_abi(1));
        assert_eq!(manifest.build.for_host(HostOs::Windows)[0].args.len(), 2);
        assert_eq!(manifest.build.for_host(HostOs::Linux)[0].args, ["install"]);
    }

    #[test]
    fn rejects_language_runtime_mismatch_and_unsafe_entry() {
        assert!(
            LibraryManifest::parse(&VALID.replace("kind = \"bun\"", "kind = \"python\"")).is_err()
        );
        assert!(LibraryManifest::parse(&VALID.replace("src/index.js", "../index.js")).is_err());
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        for path in ["../evil", "/absolute", "a/../b", "a\\b"] {
            assert!(canonical_archive_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn inspects_package_and_rejects_case_collisions() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file(LIBRARY_MANIFEST, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(VALID.as_bytes()).unwrap();
            writer
                .start_file("src/A.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"a").unwrap();
            writer.finish().unwrap();
        }
        let package =
            inspect_zip(Cursor::new(bytes.into_inner()), ArchivePolicy::default()).unwrap();
        assert_eq!(package.manifest.name, "advancenet");

        let mut duplicate = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut duplicate);
            writer
                .start_file(LIBRARY_MANIFEST, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(VALID.as_bytes()).unwrap();
            writer
                .start_file("src/A.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"a").unwrap();
            writer
                .start_file("src/a.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"b").unwrap();
            writer.finish().unwrap();
        }
        assert!(matches!(
            inspect_zip(
                Cursor::new(duplicate.into_inner()),
                ArchivePolicy::default()
            ),
            Err(ArchiveError::DuplicatePath(_))
        ));
    }
}
