//! Source-only planning primitives for RBE external-library installation.
//!
//! This crate deliberately does not download runtimes, execute build scripts, or
//! produce native binaries. It resolves deterministic versions, project-local
//! paths, registry configuration, and the build steps a trusted installer may
//! later execute.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rbe_library_lock::ProjectLayout;
use rbe_library_package::{BuildStep, HostOs, LibraryManifest};
use semver::{Version, VersionReq};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    pub os: HostOs,
    pub arch: String,
}

impl HostTarget {
    pub fn new(os: HostOs, arch: impl Into<String>) -> Result<Self, InstallerError> {
        let arch = arch.into();
        validate_component("host architecture", &arch)?;
        Ok(Self { os, arch })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRequirement {
    pub kind: String,
    pub version_requirement: String,
    pub managed: bool,
    pub entry: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkRequirement {
    pub family: String,
    pub package: String,
    pub version_requirement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub package: String,
    pub package_version: String,
    pub host: HostTarget,
    pub runtime: RuntimeRequirement,
    pub sdk: SdkRequirement,
    pub build_steps: Vec<BuildStep>,
    pub dependencies: BTreeMap<String, String>,
    pub requested_capabilities: Vec<String>,
    pub requested_build_capabilities: Vec<String>,
}

impl InstallPlan {
    pub fn from_manifest(
        manifest: &LibraryManifest,
        host: HostTarget,
    ) -> Result<Self, InstallerError> {
        manifest
            .validate()
            .map_err(|error| InstallerError::Manifest(error.to_string()))?;

        Ok(Self {
            package: manifest.name.clone(),
            package_version: manifest.version.clone(),
            host: host.clone(),
            runtime: RuntimeRequirement {
                kind: manifest.runtime.kind.clone(),
                version_requirement: manifest.runtime.version.clone(),
                managed: manifest.runtime.managed,
                entry: manifest.runtime.entry.clone(),
            },
            sdk: SdkRequirement {
                family: manifest.sdk.family.clone(),
                package: manifest.sdk.package.clone(),
                version_requirement: manifest.sdk.version.clone(),
            },
            build_steps: manifest.build.for_host(host.os).to_vec(),
            dependencies: manifest.dependencies.clone(),
            requested_capabilities: enabled_keys(&manifest.capabilities),
            requested_build_capabilities: enabled_keys(&manifest.build_capabilities),
        })
    }

    pub fn resolve_environment(
        &self,
        layout: &ProjectLayout,
        catalog: &VersionCatalog,
    ) -> Result<ResolvedEnvironment, InstallerError> {
        let runtime_version =
            catalog.resolve_runtime(&self.runtime.kind, &self.runtime.version_requirement)?;
        let sdk_version = catalog.resolve_sdk(&self.sdk.package, &self.sdk.version_requirement)?;

        let runtime_path = if self.runtime.managed {
            Some(
                layout
                    .runtime_dir(&self.runtime.kind, &runtime_version)
                    .map_err(|error| InstallerError::Layout(error.to_string()))?,
            )
        } else {
            None
        };
        let sdk_path = layout
            .sdk_dir(&self.sdk.family, &sdk_version)
            .map_err(|error| InstallerError::Layout(error.to_string()))?;

        Ok(ResolvedEnvironment {
            runtime: ResolvedRuntime {
                kind: self.runtime.kind.clone(),
                version: runtime_version,
                managed: self.runtime.managed,
                local_path: runtime_path,
            },
            sdk: ResolvedSdk {
                family: self.sdk.family.clone(),
                package: self.sdk.package.clone(),
                version: sdk_version,
                local_path: sdk_path,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRuntime {
    pub kind: String,
    pub version: String,
    pub managed: bool,
    pub local_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSdk {
    pub family: String,
    pub package: String,
    pub version: String,
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEnvironment {
    pub runtime: ResolvedRuntime,
    pub sdk: ResolvedSdk,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionCatalog {
    runtimes: BTreeMap<String, BTreeSet<Version>>,
    sdks: BTreeMap<String, BTreeSet<Version>>,
}

impl VersionCatalog {
    pub fn add_runtime(
        &mut self,
        kind: impl Into<String>,
        version: impl AsRef<str>,
    ) -> Result<(), InstallerError> {
        let kind = kind.into();
        validate_component("runtime kind", &kind)?;
        let version = parse_exact_version("runtime version", version.as_ref())?;
        self.runtimes.entry(kind).or_default().insert(version);
        Ok(())
    }

    pub fn add_sdk(
        &mut self,
        package: impl Into<String>,
        version: impl AsRef<str>,
    ) -> Result<(), InstallerError> {
        let package = package.into();
        validate_sdk_package(&package)?;
        let version = parse_exact_version("SDK version", version.as_ref())?;
        self.sdks.entry(package).or_default().insert(version);
        Ok(())
    }

    pub fn resolve_runtime(&self, kind: &str, requirement: &str) -> Result<String, InstallerError> {
        let versions = self
            .runtimes
            .get(kind)
            .ok_or_else(|| InstallerError::NoVersions(kind.to_string()))?;
        resolve_highest(requirement, versions)
    }

    pub fn resolve_sdk(&self, package: &str, requirement: &str) -> Result<String, InstallerError> {
        let versions = self
            .sdks
            .get(package)
            .ok_or_else(|| InstallerError::NoVersions(package.to_string()))?;
        resolve_highest(requirement, versions)
    }
}

fn resolve_highest(
    requirement: &str,
    versions: &BTreeSet<Version>,
) -> Result<String, InstallerError> {
    let requirement = VersionReq::parse(requirement).map_err(|source| {
        InstallerError::InvalidVersionRequirement {
            value: requirement.to_string(),
            source,
        }
    })?;
    versions
        .iter()
        .rev()
        .find(|version| requirement.matches(version))
        .map(ToString::to_string)
        .ok_or_else(|| InstallerError::NoCompatibleVersion(requirement.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEndpoints {
    pub package_index: Url,
    pub cargo_sparse: Url,
    pub npm_registry: Url,
    pub python_simple: Url,
}

impl RegistryEndpoints {
    pub fn new(
        package_index: impl AsRef<str>,
        cargo_sparse: impl AsRef<str>,
        npm_registry: impl AsRef<str>,
        python_simple: impl AsRef<str>,
    ) -> Result<Self, InstallerError> {
        Ok(Self {
            package_index: parse_registry_url("package index", package_index.as_ref())?,
            cargo_sparse: parse_registry_url("Cargo sparse registry", cargo_sparse.as_ref())?,
            npm_registry: parse_registry_url("npm registry", npm_registry.as_ref())?,
            python_simple: parse_registry_url("Python simple index", python_simple.as_ref())?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigArtifact {
    pub relative_path: PathBuf,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkRegistryPlan {
    pub files: Vec<ConfigArtifact>,
}

impl SdkRegistryPlan {
    pub fn new(endpoints: &RegistryEndpoints) -> Self {
        let cargo = format!(
            "[registries.rbe]\nindex = \"sparse+{}\"\n",
            endpoint_with_slash(&endpoints.cargo_sparse)
        );
        let npm = format!(
            "@rbe:registry={}\n",
            endpoint_with_slash(&endpoints.npm_registry)
        );
        let python = format!(
            "[global]\nindex-url = {}\n",
            endpoint_with_slash(&endpoints.python_simple)
        );

        Self {
            files: vec![
                ConfigArtifact {
                    relative_path: PathBuf::from(".rbe/registry/cargo/config.toml"),
                    contents: cargo,
                },
                ConfigArtifact {
                    relative_path: PathBuf::from(".rbe/registry/npm/.npmrc"),
                    contents: npm,
                },
                ConfigArtifact {
                    relative_path: PathBuf::from(".rbe/registry/python/pip.conf"),
                    contents: python,
                },
            ],
        }
    }

    pub fn materialized_paths(&self, project_root: &Path) -> Vec<PathBuf> {
        self.files
            .iter()
            .map(|file| project_root.join(&file.relative_path))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageRequest {
    Registry {
        name: String,
        requirement: Option<String>,
    },
    Url(Url),
    LocalArchive(PathBuf),
}

impl PackageRequest {
    pub fn registry(
        name: impl Into<String>,
        requirement: Option<impl Into<String>>,
    ) -> Result<Self, InstallerError> {
        let name = name.into();
        validate_component("package name", &name)?;
        let requirement = requirement.map(Into::into);
        if let Some(value) = &requirement {
            VersionReq::parse(value).map_err(|source| {
                InstallerError::InvalidVersionRequirement {
                    value: value.clone(),
                    source,
                }
            })?;
        }
        Ok(Self::Registry { name, requirement })
    }

    pub fn url(value: impl AsRef<str>) -> Result<Self, InstallerError> {
        Ok(Self::Url(parse_download_url(value.as_ref())?))
    }

    pub fn local(path: impl Into<PathBuf>) -> Result<Self, InstallerError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(InstallerError::InvalidLocalArchive);
        }
        Ok(Self::LocalArchive(path))
    }
}

fn enabled_keys(values: &BTreeMap<String, bool>) -> Vec<String> {
    values
        .iter()
        .filter_map(|(key, enabled)| enabled.then_some(key.clone()))
        .collect()
}

fn parse_exact_version(field: &'static str, value: &str) -> Result<Version, InstallerError> {
    Version::parse(value).map_err(|source| InstallerError::InvalidExactVersion {
        field,
        value: value.to_string(),
        source,
    })
}

fn validate_component(field: &'static str, value: &str) -> Result<(), InstallerError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(InstallerError::InvalidComponent {
            field,
            value: value.to_string(),
        });
    };
    let valid = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_');
    if !valid || value.len() > 96 {
        return Err(InstallerError::InvalidComponent {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_sdk_package(value: &str) -> Result<(), InstallerError> {
    if value.is_empty()
        || value.len() > 192
        || value.chars().any(char::is_whitespace)
        || value.contains('\\')
        || value.contains("..")
    {
        return Err(InstallerError::InvalidSdkPackage(value.to_string()));
    }
    Ok(())
}

fn parse_registry_url(field: &'static str, value: &str) -> Result<Url, InstallerError> {
    let url = Url::parse(value).map_err(|source| InstallerError::InvalidUrl {
        field,
        value: value.to_string(),
        source,
    })?;
    validate_network_url(field, &url)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(InstallerError::UnsafeUrl {
            field,
            value: value.to_string(),
        });
    }
    Ok(url)
}

fn parse_download_url(value: &str) -> Result<Url, InstallerError> {
    let url = Url::parse(value).map_err(|source| InstallerError::InvalidUrl {
        field: "package URL",
        value: value.to_string(),
        source,
    })?;
    validate_network_url("package URL", &url)?;
    if url.fragment().is_some() {
        return Err(InstallerError::UnsafeUrl {
            field: "package URL",
            value: value.to_string(),
        });
    }
    Ok(url)
}

fn validate_network_url(field: &'static str, url: &Url) -> Result<(), InstallerError> {
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return Err(InstallerError::UnsafeUrl {
            field,
            value: url.to_string(),
        });
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or_default()) => Ok(()),
        _ => Err(InstallerError::UnsafeUrl {
            field,
            value: url.to_string(),
        }),
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn endpoint_with_slash(url: &Url) -> String {
    let mut value = url.as_str().to_string();
    if !value.ends_with('/') {
        value.push('/');
    }
    value
}

#[derive(Debug, thiserror::Error)]
pub enum InstallerError {
    #[error("package manifest is invalid: {0}")]
    Manifest(String),
    #[error("project-local layout is invalid: {0}")]
    Layout(String),
    #[error("invalid {field} {value:?}")]
    InvalidComponent { field: &'static str, value: String },
    #[error("invalid SDK package name {0:?}")]
    InvalidSdkPackage(String),
    #[error("invalid exact {field} {value:?}: {source}")]
    InvalidExactVersion {
        field: &'static str,
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid version requirement {value:?}: {source}")]
    InvalidVersionRequirement {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("no versions are available for {0:?}")]
    NoVersions(String),
    #[error("no available version satisfies {0:?}")]
    NoCompatibleVersion(String),
    #[error("invalid {field} URL {value:?}: {source}")]
    InvalidUrl {
        field: &'static str,
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("unsafe {field} URL {value:?}; remote package infrastructure requires HTTPS")]
    UnsafeUrl { field: &'static str, value: String },
    #[error("local archive path must not be empty")]
    InvalidLocalArchive,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "1.2.0"
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
"router:register" = false

[build_capabilities]
"network:dependencies" = true

[[build.windows]]
program = "bun"
args = ["install", "--frozen-lockfile"]

[[build.other]]
program = "bun"
args = ["install"]
"#;

    #[test]
    fn plan_selects_os_steps_and_enabled_capabilities_only() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let plan = InstallPlan::from_manifest(
            &manifest,
            HostTarget::new(HostOs::Windows, "x86_64").unwrap(),
        )
        .unwrap();
        assert_eq!(plan.build_steps[0].args, ["install", "--frozen-lockfile"]);
        assert_eq!(plan.requested_capabilities, ["net:http", "net:p2p"]);
        assert_eq!(plan.requested_build_capabilities, ["network:dependencies"]);
    }

    #[test]
    fn resolves_highest_compatible_versions_to_project_local_paths() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let plan = InstallPlan::from_manifest(
            &manifest,
            HostTarget::new(HostOs::Linux, "x86_64").unwrap(),
        )
        .unwrap();
        let mut catalog = VersionCatalog::default();
        catalog.add_runtime("bun", "1.0.9").unwrap();
        catalog.add_runtime("bun", "1.2.3").unwrap();
        catalog.add_runtime("bun", "2.0.0").unwrap();
        catalog.add_sdk("@rbe/sdk", "0.1.2").unwrap();
        catalog.add_sdk("@rbe/sdk", "0.1.9").unwrap();
        catalog.add_sdk("@rbe/sdk", "0.2.0").unwrap();

        let resolved = plan
            .resolve_environment(&ProjectLayout::new("/project"), &catalog)
            .unwrap();
        assert_eq!(resolved.runtime.version, "1.2.3");
        assert_eq!(resolved.sdk.version, "0.1.9");
        assert_eq!(
            resolved.runtime.local_path.unwrap(),
            Path::new("/project/.rbe/runtimes/bun/1.2.3")
        );
        assert_eq!(
            resolved.sdk.local_path,
            Path::new("/project/.rbe/sdk/javascript/0.1.9")
        );
    }

    #[test]
    fn registry_plan_is_local_and_requires_https_except_loopback() {
        assert!(RegistryEndpoints::new(
            "http://registry.example/v1/",
            "https://registry.example/cargo/index/",
            "https://registry.example/npm/",
            "https://registry.example/python/simple/"
        )
        .is_err());

        let endpoints = RegistryEndpoints::new(
            "http://127.0.0.1:8080/registry/v1/",
            "https://registry.example/cargo/index/",
            "https://registry.example/npm/",
            "https://registry.example/python/simple/",
        )
        .unwrap();
        let plan = SdkRegistryPlan::new(&endpoints);
        assert_eq!(plan.files.len(), 3);
        assert!(plan.files[0].contents.contains("sparse+https://"));
        assert!(plan.files[1].contents.contains("@rbe:registry="));
        assert!(plan.files[2].contents.contains("index-url = https://"));
        for path in plan.materialized_paths(Path::new("/project")) {
            assert!(path.starts_with("/project/.rbe/registry"));
        }
    }

    #[test]
    fn package_requests_validate_registry_names_and_remote_transport() {
        assert!(PackageRequest::registry("advancenet", Some("^1.2")).is_ok());
        assert!(PackageRequest::registry("../evil", None::<String>).is_err());
        assert!(PackageRequest::url("https://packages.example/advancenet.zip").is_ok());
        assert!(PackageRequest::url("http://packages.example/advancenet.zip").is_err());
        assert!(PackageRequest::url("http://localhost:8080/advancenet.zip").is_ok());
    }
}
