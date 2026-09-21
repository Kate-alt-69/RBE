use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    InstallRequestError, KASTRICK_SYSTEM_RUNTIME_MANIFEST_PREFIX, RBE_SYSTEM_BUNJS,
    RBE_SYSTEM_NODEJS, RBE_SYSTEM_PYTHON, RBE_SYSTEM_RUST,
};

pub const SYSTEM_RUNTIME_MANIFEST_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemRuntimeKind {
    Python,
    Nodejs,
    Bunjs,
    Rust,
}

impl SystemRuntimeKind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Python => RBE_SYSTEM_PYTHON,
            Self::Nodejs => RBE_SYSTEM_NODEJS,
            Self::Bunjs => RBE_SYSTEM_BUNJS,
            Self::Rust => RBE_SYSTEM_RUST,
        }
    }

    pub const fn cache_component(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Nodejs => "nodejs",
            Self::Bunjs => "bunjs",
            Self::Rust => "rust",
        }
    }

    pub fn from_key(value: &str) -> Result<Self, InstallRequestError> {
        match value {
            RBE_SYSTEM_PYTHON => Ok(Self::Python),
            RBE_SYSTEM_NODEJS => Ok(Self::Nodejs),
            RBE_SYSTEM_BUNJS => Ok(Self::Bunjs),
            RBE_SYSTEM_RUST => Ok(Self::Rust),
            _ => Err(InstallRequestError::UnknownSystemRuntime(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableArchive {
    Zip,
    TarGz,
    TarXz,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemRuntimeManifest {
    pub format: u32,
    pub runtime: String,
    pub version: String,
    pub host: String,
    pub source: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub archive: PortableArchive,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl SystemRuntimeManifest {
    pub fn parse_json(
        input: &str,
        expected: SystemRuntimeKind,
        expected_host: &str,
    ) -> Result<Self, InstallRequestError> {
        let manifest: Self = serde_json::from_str(input)?;
        manifest.validate_for(expected, expected_host)?;
        Ok(manifest)
    }

    pub fn validate_for(
        &self,
        expected: SystemRuntimeKind,
        expected_host: &str,
    ) -> Result<(), InstallRequestError> {
        if self.format != SYSTEM_RUNTIME_MANIFEST_FORMAT {
            return Err(InstallRequestError::UnsupportedSystemRuntimeManifest(
                self.format,
            ));
        }
        if self.runtime != expected.key() {
            return Err(InstallRequestError::SystemRuntimeMismatch {
                expected: expected.key().to_string(),
                actual: self.runtime.clone(),
            });
        }
        validate_host_id(expected_host)?;
        if self.host != expected_host {
            return Err(InstallRequestError::SystemRuntimeHostMismatch {
                expected: expected_host.to_string(),
                actual: self.host.clone(),
            });
        }
        validate_version(&self.version)?;
        parse_https_url(&self.source)?;
        canonical_sha256(&self.sha256)?;
        if self.size_bytes == 0 {
            return Err(InstallRequestError::InvalidSystemRuntimeSize);
        }
        validate_entrypoint(&self.entrypoint)?;
        if let Some(publisher) = &self.publisher {
            validate_bounded_text("system runtime publisher", publisher)?;
        }
        if let Some(signature) = &self.signature {
            validate_bounded_text("system runtime signature", signature)?;
        }
        Ok(())
    }

    pub fn plan(
        &self,
        cache_root: &Path,
        expected: SystemRuntimeKind,
        expected_host: &str,
    ) -> Result<SystemRuntimePlan, InstallRequestError> {
        self.validate_for(expected, expected_host)?;
        if cache_root.as_os_str().is_empty() {
            return Err(InstallRequestError::InvalidCacheRoot);
        }
        let runtime_root = cache_root
            .join("rbe")
            .join("sys")
            .join(expected.cache_component());
        let install_dir = runtime_root.join(&self.version).join(expected_host);
        Ok(SystemRuntimePlan {
            runtime: expected,
            version: self.version.clone(),
            host: expected_host.to_string(),
            manifest_cache_path: runtime_root.join("manifest.json"),
            source: parse_https_url(&self.source)?,
            sha256: canonical_sha256(&self.sha256)?,
            size_bytes: self.size_bytes,
            archive: self.archive,
            executable: install_dir.join(&self.entrypoint),
            install_dir,
            cache_root: runtime_root,
            hardening: CacheHardeningPolicy::default(),
            mutate_system_path: false,
            visible_as_user_package: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemRuntimeManifestRequest {
    pub runtime: SystemRuntimeKind,
    pub host: String,
    pub endpoint: Url,
}

impl SystemRuntimeManifestRequest {
    pub fn new(
        kastrick_registry: &str,
        runtime: SystemRuntimeKind,
        host: &str,
    ) -> Result<Self, InstallRequestError> {
        validate_host_id(host)?;
        let base = parse_https_base(kastrick_registry)?;
        let relative = format!(
            "{}/{}/{}/manifest.json",
            KASTRICK_SYSTEM_RUNTIME_MANIFEST_PREFIX.trim_matches('/'),
            runtime.key(),
            host
        );
        let endpoint = base.join(&relative).map_err(InstallRequestError::JoinUrl)?;
        Ok(Self {
            runtime,
            host: host.to_string(),
            endpoint,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheHardeningPolicy {
    pub unix_mode: u32,
    pub windows_user_only_acl: bool,
    pub verify_hash_on_every_use: bool,
    pub trust_cache_path: bool,
}

impl Default for CacheHardeningPolicy {
    fn default() -> Self {
        Self {
            unix_mode: 0o700,
            windows_user_only_acl: true,
            verify_hash_on_every_use: true,
            trust_cache_path: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemRuntimePlan {
    pub runtime: SystemRuntimeKind,
    pub version: String,
    pub host: String,
    pub source: Url,
    pub sha256: String,
    pub size_bytes: u64,
    pub archive: PortableArchive,
    pub cache_root: PathBuf,
    pub manifest_cache_path: PathBuf,
    pub install_dir: PathBuf,
    pub executable: PathBuf,
    pub hardening: CacheHardeningPolicy,
    pub mutate_system_path: bool,
    pub visible_as_user_package: bool,
}

fn parse_https_url(value: &str) -> Result<Url, InstallRequestError> {
    let url = Url::parse(value).map_err(|source| InstallRequestError::InvalidExternalUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(InstallRequestError::HttpsRequired(value.to_string()));
    }
    Ok(url)
}

fn parse_https_base(value: &str) -> Result<Url, InstallRequestError> {
    let mut url = parse_https_url(value)?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn canonical_sha256(value: &str) -> Result<String, InstallRequestError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(InstallRequestError::InvalidSha256(value.to_string()));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_version(value: &str) -> Result<(), InstallRequestError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return Err(InstallRequestError::InvalidSystemRuntimeVersion(
            value.to_string(),
        ));
    }
    Ok(())
}

fn validate_entrypoint(value: &str) -> Result<(), InstallRequestError> {
    if value.is_empty()
        || value.len() > 512
        || value.contains("..")
        || value.contains(':')
        || value.starts_with('/')
        || value.starts_with('\\')
    {
        return Err(InstallRequestError::InvalidEntrypoint(value.to_string()));
    }
    Ok(())
}

fn validate_host_id(value: &str) -> Result<(), InstallRequestError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(InstallRequestError::InvalidHostId(value.to_string()));
    }
    Ok(())
}

fn validate_bounded_text(field: &'static str, value: &str) -> Result<(), InstallRequestError> {
    if value.trim().is_empty() || value.len() > 1024 {
        return Err(InstallRequestError::InvalidSystemRuntimeText(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(runtime: SystemRuntimeKind) -> String {
        format!(
            r#"{{
  "format": 1,
  "runtime": "{}",
  "version": "3.13.7",
  "host": "windows-x86_64",
  "source": "https://runtime.kastrick.invalid/runtime.zip",
  "sha256": "{}",
  "size_bytes": 1048576,
  "archive": "zip",
  "entrypoint": "bin/runtime.exe",
  "publisher": "kastrick"
}}"#,
            runtime.key(),
            "a".repeat(64)
        )
    }

    #[test]
    fn manifests_are_remote_and_system_runtimes_share_the_cache_model() {
        for runtime in [
            SystemRuntimeKind::Python,
            SystemRuntimeKind::Nodejs,
            SystemRuntimeKind::Bunjs,
            SystemRuntimeKind::Rust,
        ] {
            let parsed =
                SystemRuntimeManifest::parse_json(&manifest(runtime), runtime, "windows-x86_64")
                    .unwrap();
            let plan = parsed
                .plan(Path::new("/users/kate/.cache"), runtime, "windows-x86_64")
                .unwrap();
            assert_eq!(
                plan.cache_root,
                PathBuf::from("/users/kate/.cache/rbe/sys").join(runtime.cache_component())
            );
            assert!(!plan.mutate_system_path);
            assert!(!plan.visible_as_user_package);
            assert!(plan.hardening.verify_hash_on_every_use);
            assert!(!plan.hardening.trust_cache_path);
        }
    }

    #[test]
    fn manifest_request_is_derived_from_endpoint_not_embedded_payload() {
        let request = SystemRuntimeManifestRequest::new(
            "https://registry.kastrick.invalid/",
            SystemRuntimeKind::Python,
            "linux-x86_64",
        )
        .unwrap();
        assert_eq!(
            request.endpoint.as_str(),
            "https://registry.kastrick.invalid/registry/v1/system-runtime/rbe.sys.python/linux-x86_64/manifest.json"
        );
    }

    #[test]
    fn manifest_cannot_swap_runtime_or_host() {
        let error = SystemRuntimeManifest::parse_json(
            &manifest(SystemRuntimeKind::Python),
            SystemRuntimeKind::Rust,
            "windows-x86_64",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            InstallRequestError::SystemRuntimeMismatch { .. }
        ));
    }
}
