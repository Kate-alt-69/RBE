//! Authoring model for RBE SDK packages.
//!
//! A package has one root `package.rbe.toml` and an import surface formed by
//! directories under `components/`. A component named `request` is imported as
//! `:import[request from <package>]`; internal implementation code should live
//! outside `components/` and therefore is not discoverable as a package export.
//!
//! Packages are single-language by default. `language = "global"` is the only
//! explicit opt-in that allows individual components to use different SDK
//! languages.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const PACKAGE_MANIFEST: &str = "package.rbe.toml";
pub const DEFAULT_COMPONENTS_DIR: &str = "components";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub package: PackageIdentity,
    #[serde(default)]
    pub components: ComponentsConfig,
    #[serde(default)]
    pub dependencies: DependencyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub language: PackageLanguage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<JsRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageLanguage {
    Rust,
    Javascript,
    Typescript,
    Python,
    /// Explicit opt-in for a package whose individual components use different SDK languages.
    Global,
}

impl PackageLanguage {
    pub fn source_extension(self) -> Option<&'static str> {
        match self {
            Self::Rust => Some("rs"),
            Self::Javascript => Some("js"),
            Self::Typescript => Some("ts"),
            Self::Python => Some("py"),
            Self::Global => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsRuntime {
    Node,
    Bun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentsConfig {
    #[serde(default = "default_components_root")]
    pub root: String,
}

fn default_components_root() -> String {
    DEFAULT_COMPONENTS_DIR.to_string()
}

impl Default for ComponentsConfig {
    fn default() -> Self {
        Self {
            root: default_components_root(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyConfig {
    /// RBE-package dependencies are package-private transitive dependencies.
    /// They do not become directly importable by a consuming REL project unless
    /// that project also declares/installs them as a root package.
    #[serde(default)]
    pub rbe: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CheckedPackage {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: PackageManifest,
    pub components: Vec<CheckedComponent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckedComponent {
    pub name: String,
    pub directory: PathBuf,
    pub source: PathBuf,
    pub language: PackageLanguage,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageIndex {
    pub format: u32,
    pub package: PackageIndexIdentity,
    pub exports: Vec<PackageExport>,
    pub private_dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageIndexIdentity {
    pub name: String,
    pub version: String,
    pub language: PackageLanguage,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageExport {
    pub name: String,
    pub source: String,
    pub language: PackageLanguage,
}

impl CheckedPackage {
    pub fn index(&self) -> PackageIndex {
        PackageIndex {
            format: 1,
            package: PackageIndexIdentity {
                name: self.manifest.package.name.clone(),
                version: self.manifest.package.version.clone(),
                language: self.manifest.package.language,
            },
            exports: self
                .components
                .iter()
                .map(|component| PackageExport {
                    name: component.name.clone(),
                    source: relative_slash(&self.root, &component.source),
                    language: component.language,
                })
                .collect(),
            private_dependencies: self.manifest.dependencies.rbe.clone(),
        }
    }
}

pub fn find_package_root(start: impl AsRef<Path>) -> Result<PathBuf, PackageError> {
    let start = start.as_ref();
    let mut current = if start.is_file() {
        start.parent().unwrap_or(start).to_path_buf()
    } else {
        start.to_path_buf()
    };
    if !current.is_absolute() {
        current = std::env::current_dir()?.join(current);
    }
    loop {
        if current.join(PACKAGE_MANIFEST).is_file() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(PackageError::ManifestNotFound(start.to_path_buf()));
        }
    }
}

/// Check an entire package, regardless of whether `start` points at a child path.
pub fn check_package(start: impl AsRef<Path>) -> Result<CheckedPackage, PackageError> {
    let root = find_package_root(start)?;
    check_with_selection(root, None)
}

/// Check the package target represented by `start`.
///
/// A path inside `components/<name>/...` checks only that component. A package
/// root, the `components/` directory itself, or any non-component path checks
/// the complete package. This makes `rpx compile components/endpoint` a real
/// targeted compile/preflight instead of silently validating unrelated exports.
pub fn check_target(start: impl AsRef<Path>) -> Result<CheckedPackage, PackageError> {
    let start = absolute_path(start.as_ref())?;
    if !start.exists() {
        return Err(PackageError::TargetNotFound(start));
    }

    let root = find_package_root(&start)?;
    let manifest = load_manifest(&root)?;
    let component_name = selected_component_name(&root, &manifest, &start)?;
    check_loaded(root, manifest, component_name.as_deref())
}

fn check_with_selection(
    root: PathBuf,
    component: Option<&str>,
) -> Result<CheckedPackage, PackageError> {
    let manifest = load_manifest(&root)?;
    check_loaded(root, manifest, component)
}

fn load_manifest(root: &Path) -> Result<PackageManifest, PackageError> {
    let manifest_path = root.join(PACKAGE_MANIFEST);
    let input = fs::read_to_string(&manifest_path)?;
    let manifest: PackageManifest = toml::from_str(&input)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn check_loaded(
    root: PathBuf,
    manifest: PackageManifest,
    component: Option<&str>,
) -> Result<CheckedPackage, PackageError> {
    let manifest_path = root.join(PACKAGE_MANIFEST);
    let components_root = root.join(&manifest.components.root);
    if !components_root.is_dir() {
        return Err(PackageError::ComponentsDirectoryMissing(components_root));
    }

    let mut components = Vec::new();
    if let Some(name) = component {
        validate_name("component", name)?;
        let directory = components_root.join(name);
        if !directory.is_dir() {
            return Err(PackageError::ComponentDirectoryMissing {
                component: name.to_string(),
                directory,
            });
        }
        components.push(check_component(&manifest, name, &directory)?);
    } else {
        for entry in fs::read_dir(&components_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| PackageError::InvalidComponentName("<non-utf8>".into()))?;
            validate_name("component", &name)?;
            components.push(check_component(&manifest, &name, &entry.path())?);
        }
        components.sort_by(|left, right| left.name.cmp(&right.name));
    }

    if components.is_empty() {
        return Err(PackageError::NoComponents(components_root));
    }

    Ok(CheckedPackage {
        root,
        manifest_path,
        manifest,
        components,
    })
}

fn selected_component_name(
    root: &Path,
    manifest: &PackageManifest,
    target: &Path,
) -> Result<Option<String>, PackageError> {
    let components_root = root.join(&manifest.components.root);
    let target = target.canonicalize()?;
    let components_root = match components_root.canonicalize() {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };

    let Ok(relative) = target.strip_prefix(&components_root) else {
        return Ok(None);
    };
    let mut parts = relative.components();
    let Some(Component::Normal(name)) = parts.next() else {
        return Ok(None);
    };
    let name = name
        .to_str()
        .ok_or_else(|| PackageError::InvalidComponentName("<non-utf8>".into()))?
        .to_string();
    validate_name("component", &name)?;
    Ok(Some(name))
}

fn validate_manifest(manifest: &PackageManifest) -> Result<(), PackageError> {
    validate_name("package", &manifest.package.name)?;
    if manifest.package.version.trim().is_empty() || manifest.package.version.len() > 128 {
        return Err(PackageError::InvalidVersion(
            manifest.package.version.clone(),
        ));
    }
    validate_relative_dir(&manifest.components.root)?;

    match manifest.package.language {
        PackageLanguage::Javascript | PackageLanguage::Typescript => {
            if manifest.package.runtime.is_none() {
                return Err(PackageError::MissingJsRuntime);
            }
        }
        PackageLanguage::Rust | PackageLanguage::Python | PackageLanguage::Global => {
            if manifest.package.runtime.is_some() {
                return Err(PackageError::UnexpectedJsRuntime);
            }
        }
    }

    for (name, version) in &manifest.dependencies.rbe {
        validate_name("RBE dependency", name)?;
        if version.trim().is_empty() || version.len() > 128 {
            return Err(PackageError::InvalidDependencyVersion {
                package: name.clone(),
                version: version.clone(),
            });
        }
    }
    Ok(())
}

fn check_component(
    manifest: &PackageManifest,
    name: &str,
    directory: &Path,
) -> Result<CheckedComponent, PackageError> {
    let (source, language) = if let Some(extension) = manifest.package.language.source_extension() {
        let source = directory.join(format!("{name}.{extension}"));
        if !source.is_file() {
            return Err(PackageError::ComponentSourceMissing {
                component: name.to_string(),
                expected: source,
            });
        }
        (source, manifest.package.language)
    } else {
        let candidates = [
            ("rs", PackageLanguage::Rust),
            ("js", PackageLanguage::Javascript),
            ("ts", PackageLanguage::Typescript),
            ("py", PackageLanguage::Python),
        ];
        let mut found = Vec::new();
        for (extension, language) in candidates {
            let source = directory.join(format!("{name}.{extension}"));
            if source.is_file() {
                found.push((source, language));
            }
        }
        match found.len() {
            0 => {
                return Err(PackageError::GlobalComponentSourceMissing {
                    component: name.to_string(),
                    directory: directory.to_path_buf(),
                })
            }
            1 => found.pop().expect("length checked"),
            _ => {
                return Err(PackageError::GlobalComponentAmbiguous {
                    component: name.to_string(),
                    directory: directory.to_path_buf(),
                })
            }
        }
    };

    Ok(CheckedComponent {
        name: name.to_string(),
        directory: directory.to_path_buf(),
        source,
        language,
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf, PackageError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_name(field: &'static str, value: &str) -> Result<(), PackageError> {
    if value.is_empty()
        || value.len() > 192
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(PackageError::InvalidName {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_relative_dir(value: &str) -> Result<(), PackageError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PackageError::InvalidComponentsRoot(value.to_string()));
    }
    Ok(())
}

fn relative_slash(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("package target does not exist: {0}")]
    TargetNotFound(PathBuf),
    #[error("package.rbe.toml was not found from {0}")]
    ManifestNotFound(PathBuf),
    #[error("failed to access package files: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid package.rbe.toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid {field} name {value:?}; use lowercase letters, digits, '-' or '_'")]
    InvalidName { field: &'static str, value: String },
    #[error("invalid package version {0:?}")]
    InvalidVersion(String),
    #[error("components root must be a safe relative directory: {0:?}")]
    InvalidComponentsRoot(String),
    #[error("JavaScript/TypeScript packages must declare runtime = \"node\" or runtime = \"bun\"")]
    MissingJsRuntime,
    #[error("runtime is only valid for JavaScript/TypeScript packages")]
    UnexpectedJsRuntime,
    #[error("components directory does not exist: {0}")]
    ComponentsDirectoryMissing(PathBuf),
    #[error("package has no component directories under {0}")]
    NoComponents(PathBuf),
    #[error("component {component:?} directory does not exist: {directory}")]
    ComponentDirectoryMissing {
        component: String,
        directory: PathBuf,
    },
    #[error("invalid component name {0:?}")]
    InvalidComponentName(String),
    #[error("component {component:?} is missing its language entry file: {expected}")]
    ComponentSourceMissing {
        component: String,
        expected: PathBuf,
    },
    #[error("global component {component:?} needs exactly one named .rs/.js/.ts/.py entry file under {directory}")]
    GlobalComponentSourceMissing {
        component: String,
        directory: PathBuf,
    },
    #[error("global component {component:?} has multiple named SDK-language entry files under {directory}")]
    GlobalComponentAmbiguous {
        component: String,
        directory: PathBuf,
    },
    #[error("invalid private RBE dependency version for {package:?}: {version:?}")]
    InvalidDependencyVersion { package: String, version: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn package_language_extensions_are_stable() {
        assert_eq!(PackageLanguage::Typescript.source_extension(), Some("ts"));
        assert_eq!(PackageLanguage::Python.source_extension(), Some("py"));
        assert_eq!(PackageLanguage::Global.source_extension(), None);
    }

    #[test]
    fn package_names_do_not_accept_path_syntax() {
        assert!(validate_name("package", "advancenet").is_ok());
        assert!(validate_name("package", "../advancenet").is_err());
    }

    #[test]
    fn component_target_does_not_validate_unrelated_components() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rbe-sdk-package-{stamp}"));
        let request = root.join("components/request");
        let broken = root.join("components/broken");
        fs::create_dir_all(&request).unwrap();
        fs::create_dir_all(&broken).unwrap();
        fs::write(
            root.join(PACKAGE_MANIFEST),
            "[package]\nname = \"advancenet\"\nversion = \"1.0.0\"\nlanguage = \"typescript\"\nruntime = \"bun\"\n",
        )
        .unwrap();
        fs::write(request.join("request.ts"), "export const request = 1;\n").unwrap();

        let targeted = check_target(&request).unwrap();
        assert_eq!(targeted.components.len(), 1);
        assert_eq!(targeted.components[0].name, "request");
        assert!(check_package(&root).is_err());

        let _ = fs::remove_dir_all(root);
    }
}
