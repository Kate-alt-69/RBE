//! Root-package export links consumed by RELC.
//!
//! The package manager owns version resolution and installation. RELC receives
//! only the already-verified, explicit project roots and their public exports.
//! Private/transitive dependency graphs are intentionally absent from this
//! structure, so they cannot become directly importable REL namespaces.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

pub const PACKAGE_LINK_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLinkContext {
    #[serde(default = "package_link_format")]
    pub format: u32,
    #[serde(default)]
    pub roots: BTreeMap<String, PackageRootLink>,
}

impl Default for PackageLinkContext {
    fn default() -> Self {
        Self {
            format: PACKAGE_LINK_FORMAT,
            roots: BTreeMap::new(),
        }
    }
}

const fn package_link_format() -> u32 {
    PACKAGE_LINK_FORMAT
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRootLink {
    pub version: String,
    pub artifact_sha256: String,
    #[serde(default)]
    pub exports: BTreeMap<String, PackageExportLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageExportLink {
    /// Path inside the verified/extracted package artifact.
    pub entry: String,
    /// SDK language used to execute this export.
    pub language: String,
}

impl PackageLinkContext {
    pub fn parse_json(input: &str) -> Result<Self, PackageLinkError> {
        let context: Self = serde_json::from_str(input)?;
        context.validate()?;
        Ok(context)
    }

    pub fn render_json(&self) -> Result<String, PackageLinkError> {
        self.validate()?;
        let mut output = serde_json::to_string_pretty(self)?;
        output.push('\n');
        Ok(output)
    }

    pub fn validate(&self) -> Result<(), PackageLinkError> {
        if self.format != PACKAGE_LINK_FORMAT {
            return Err(PackageLinkError::UnsupportedFormat(self.format));
        }
        for (name, root) in &self.roots {
            validate_name(name)?;
            validate_version(&root.version)?;
            validate_sha256(&root.artifact_sha256)?;
            if root.exports.is_empty() {
                return Err(PackageLinkError::NoExports(name.clone()));
            }
            for (export, link) in &root.exports {
                validate_name(export)?;
                validate_entry(&link.entry)?;
                validate_language(&link.language)?;
            }
        }
        Ok(())
    }

    pub fn root(&self, package: &str) -> Option<&PackageRootLink> {
        self.roots.get(package)
    }

    /// Resolve only an explicit root package export. The returned public error
    /// deliberately does not reveal whether a similarly named private package,
    /// internal component, or hidden export exists anywhere in the install graph.
    pub fn resolve(
        &self,
        package: &str,
        export: &str,
    ) -> Result<ResolvedPackageExport<'_>, PackageLinkError> {
        let Some((canonical_package, root)) = self.roots.get_key_value(package) else {
            return Err(PackageLinkError::ExportNotFound {
                package: package.to_string(),
                export: export.to_string(),
            });
        };
        let Some((canonical_export, link)) = root.exports.get_key_value(export) else {
            return Err(PackageLinkError::ExportNotFound {
                package: package.to_string(),
                export: export.to_string(),
            });
        };
        Ok(ResolvedPackageExport {
            package: canonical_package,
            export: canonical_export,
            version: &root.version,
            artifact_sha256: &root.artifact_sha256,
            entry: &link.entry,
            language: &link.language,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPackageExport<'a> {
    pub package: &'a str,
    pub export: &'a str,
    pub version: &'a str,
    pub artifact_sha256: &'a str,
    pub entry: &'a str,
    pub language: &'a str,
}

fn validate_name(value: &str) -> Result<(), PackageLinkError> {
    if value.is_empty()
        || value.len() > 192
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(PackageLinkError::InvalidName(value.to_string()));
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<(), PackageLinkError> {
    if value.trim().is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(PackageLinkError::InvalidVersion(value.to_string()));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), PackageLinkError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PackageLinkError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn validate_entry(value: &str) -> Result<(), PackageLinkError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 2048
        || path.is_absolute()
        || value.contains('\\')
        || value.contains(':')
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PackageLinkError::InvalidEntry(value.to_string()));
    }
    Ok(())
}

fn validate_language(value: &str) -> Result<(), PackageLinkError> {
    if matches!(value, "rust" | "javascript" | "typescript" | "python") {
        Ok(())
    } else {
        Err(PackageLinkError::InvalidLanguage(value.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageLinkError {
    #[error("invalid package-link JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported package-link format {0}")]
    UnsupportedFormat(u32),
    #[error("invalid package/export name {0:?}")]
    InvalidName(String),
    #[error("invalid package version {0:?}")]
    InvalidVersion(String),
    #[error("invalid package artifact SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("invalid package export entry {0:?}")]
    InvalidEntry(String),
    #[error("unsupported package export language {0:?}")]
    InvalidLanguage(String),
    #[error("root package {0:?} exposes no public exports")]
    NoExports(String),
    #[error("PACKAGE_EXPORT_NOT_FOUND: package export not found: {export} from {package}")]
    ExportNotFound { package: String, export: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PackageLinkContext {
        PackageLinkContext {
            format: 1,
            roots: BTreeMap::from([(
                "advancenet".into(),
                PackageRootLink {
                    version: "2.0.0".into(),
                    artifact_sha256: "a".repeat(64),
                    exports: BTreeMap::from([(
                        "request".into(),
                        PackageExportLink {
                            entry: "components/request/request.ts".into(),
                            language: "typescript".into(),
                        },
                    )]),
                },
            )]),
        }
    }

    #[test]
    fn empty_default_context_is_a_valid_format_one_context() {
        let context = PackageLinkContext::default();
        assert_eq!(context.format, PACKAGE_LINK_FORMAT);
        assert!(context.roots.is_empty());
        context.validate().unwrap();
    }

    #[test]
    fn resolves_only_declared_root_exports() {
        let context = context();
        let resolved = context.resolve("advancenet", "request").unwrap();
        assert_eq!(resolved.package, "advancenet");
        assert_eq!(resolved.export, "request");
        assert_eq!(resolved.language, "typescript");
    }

    #[test]
    fn missing_root_and_missing_export_use_same_public_error_shape() {
        let context = context();
        let missing_root = context
            .resolve("rbe-compiler-syntax", "parser")
            .unwrap_err()
            .to_string();
        let missing_export = context
            .resolve("advancenet", "internal-cache")
            .unwrap_err()
            .to_string();
        assert!(missing_root.starts_with("PACKAGE_EXPORT_NOT_FOUND:"));
        assert!(missing_export.starts_with("PACKAGE_EXPORT_NOT_FOUND:"));
    }

    #[test]
    fn traversal_entries_are_rejected() {
        let mut context = context();
        context
            .roots
            .get_mut("advancenet")
            .unwrap()
            .exports
            .get_mut("request")
            .unwrap()
            .entry = "../secret.ts".into();
        assert!(matches!(
            context.validate(),
            Err(PackageLinkError::InvalidEntry(_))
        ));
    }
}
