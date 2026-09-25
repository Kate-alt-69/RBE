//! Root-package export links consumed by RELC.
//!
//! The package manager owns version resolution and installation. RELC receives
//! only the already-verified, explicit project roots and their public exports.
//! Private/transitive dependency graphs are intentionally absent from this
//! structure, so they cannot become directly importable REL namespaces.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use serde_json::{Map as JsonMap, Value as JsonValue};

pub const PACKAGE_LINK_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLinkContext {
    pub format: u32,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRootLink {
    pub version: String,
    pub artifact_sha256: String,
    pub exports: BTreeMap<String, PackageExportLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageExportLink {
    /// Path inside the verified/extracted package artifact.
    pub entry: String,
    /// SDK language used to execute this export.
    pub language: String,
}

impl PackageLinkContext {
    pub fn parse_json(input: &str) -> Result<Self, PackageLinkError> {
        let value: JsonValue = serde_json::from_str(input)?;
        let root = object(&value, "package-link root")?;
        reject_unknown(root, &["format", "roots"], "package-link root")?;

        let format = match root.get("format") {
            Some(value) => u32_value(value, "format")?,
            None => PACKAGE_LINK_FORMAT,
        };
        let roots_value = root
            .get("roots")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(JsonMap::new()));
        let roots_object = object(&roots_value, "roots")?;
        let mut roots = BTreeMap::new();

        for (name, value) in roots_object {
            let package = object(value, "root package")?;
            reject_unknown(
                package,
                &["version", "artifact_sha256", "exports"],
                "root package",
            )?;
            let version = string_value(package.get("version"), "version")?.to_string();
            let artifact_sha256 =
                string_value(package.get("artifact_sha256"), "artifact_sha256")?.to_string();
            let exports_value = package
                .get("exports")
                .cloned()
                .unwrap_or_else(|| JsonValue::Object(JsonMap::new()));
            let exports_object = object(&exports_value, "exports")?;
            let mut exports = BTreeMap::new();
            for (export, value) in exports_object {
                let link = object(value, "package export")?;
                reject_unknown(link, &["entry", "language"], "package export")?;
                exports.insert(
                    export.clone(),
                    PackageExportLink {
                        entry: string_value(link.get("entry"), "entry")?.to_string(),
                        language: string_value(link.get("language"), "language")?.to_string(),
                    },
                );
            }
            roots.insert(
                name.clone(),
                PackageRootLink {
                    version,
                    artifact_sha256,
                    exports,
                },
            );
        }

        let context = Self { format, roots };
        context.validate()?;
        Ok(context)
    }

    pub fn render_json(&self) -> Result<String, PackageLinkError> {
        self.validate()?;
        let mut roots = JsonMap::new();
        for (name, root) in &self.roots {
            let mut exports = JsonMap::new();
            for (export, link) in &root.exports {
                exports.insert(
                    export.clone(),
                    serde_json::json!({
                        "entry": link.entry,
                        "language": link.language,
                    }),
                );
            }
            roots.insert(
                name.clone(),
                serde_json::json!({
                    "version": root.version,
                    "artifact_sha256": root.artifact_sha256,
                    "exports": exports,
                }),
            );
        }
        let value = serde_json::json!({
            "format": self.format,
            "roots": roots,
        });
        let mut output = serde_json::to_string_pretty(&value)?;
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

fn object<'a>(value: &'a JsonValue, label: &str) -> Result<&'a JsonMap<String, JsonValue>, PackageLinkError> {
    value
        .as_object()
        .ok_or_else(|| PackageLinkError::InvalidShape(format!("{label} must be a JSON object")))
}

fn string_value<'a>(value: Option<&'a JsonValue>, field: &str) -> Result<&'a str, PackageLinkError> {
    value
        .and_then(JsonValue::as_str)
        .ok_or_else(|| PackageLinkError::InvalidShape(format!("{field} must be a JSON string")))
}

fn u32_value(value: &JsonValue, field: &str) -> Result<u32, PackageLinkError> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| PackageLinkError::InvalidShape(format!("{field} must be a u32")))
}

fn reject_unknown(
    object: &JsonMap<String, JsonValue>,
    allowed: &[&str],
    label: &str,
) -> Result<(), PackageLinkError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(PackageLinkError::InvalidShape(format!(
            "{label} contains unknown field {key:?}"
        )));
    }
    Ok(())
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

#[derive(Debug)]
pub enum PackageLinkError {
    Json(serde_json::Error),
    InvalidShape(String),
    UnsupportedFormat(u32),
    InvalidName(String),
    InvalidVersion(String),
    InvalidSha256(String),
    InvalidEntry(String),
    InvalidLanguage(String),
    NoExports(String),
    ExportNotFound { package: String, export: String },
}

impl From<serde_json::Error> for PackageLinkError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl fmt::Display for PackageLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid package-link JSON: {error}"),
            Self::InvalidShape(message) => write!(formatter, "invalid package-link JSON: {message}"),
            Self::UnsupportedFormat(format) => write!(formatter, "unsupported package-link format {format}"),
            Self::InvalidName(value) => write!(formatter, "invalid package/export name {value:?}"),
            Self::InvalidVersion(value) => write!(formatter, "invalid package version {value:?}"),
            Self::InvalidSha256(value) => {
                write!(formatter, "invalid package artifact SHA-256 {value:?}")
            }
            Self::InvalidEntry(value) => write!(formatter, "invalid package export entry {value:?}"),
            Self::InvalidLanguage(value) => {
                write!(formatter, "unsupported package export language {value:?}")
            }
            Self::NoExports(package) => write!(formatter, "root package {package:?} exposes no public exports"),
            Self::ExportNotFound { package, export } => write!(
                formatter,
                "PACKAGE_EXPORT_NOT_FOUND: package export not found: {export} from {package}"
            ),
        }
    }
}

impl std::error::Error for PackageLinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
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
    fn json_round_trip_preserves_root_only_links() {
        let context = context();
        let rendered = context.render_json().unwrap();
        assert_eq!(PackageLinkContext::parse_json(&rendered).unwrap(), context);
    }

    #[test]
    fn json_parser_rejects_unknown_fields() {
        let error = PackageLinkContext::parse_json(
            r#"{"format":1,"roots":{},"private":{"secret":{}}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
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
