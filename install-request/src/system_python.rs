use std::path::{Path, PathBuf};

use url::Url;

use crate::{InstallRequestError, VersionSelector, RBE_SYSTEM_PYTHON};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableArchive {
    Zip,
    TarGz,
    TarXz,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPythonBootstrap {
    pub version: String,
    pub source: Url,
    pub sha256: String,
    pub archive: PortableArchive,
    pub entrypoint: String,
}

impl SystemPythonBootstrap {
    pub fn new(
        version: impl Into<String>,
        source: impl AsRef<str>,
        sha256: impl Into<String>,
        archive: PortableArchive,
        entrypoint: impl Into<String>,
    ) -> Result<Self, InstallRequestError> {
        let version = version.into();
        let _ = VersionSelector::parse(&version)?;
        let source = parse_https_url(source.as_ref())?;
        let sha256 = canonical_sha256(sha256.into())?;
        let entrypoint = entrypoint.into();
        validate_entrypoint(&entrypoint)?;
        Ok(Self {
            version,
            source,
            sha256,
            archive,
            entrypoint,
        })
    }

    pub fn plan(
        &self,
        user_rbe_root: &Path,
        host_id: &str,
    ) -> Result<SystemPythonPlan, InstallRequestError> {
        if user_rbe_root.as_os_str().is_empty() {
            return Err(InstallRequestError::InvalidUserRbeRoot);
        }
        validate_host_id(host_id)?;
        let install_dir = user_rbe_root
            .join("system")
            .join(RBE_SYSTEM_PYTHON)
            .join(&self.version)
            .join(host_id);
        Ok(SystemPythonPlan {
            runtime_key: RBE_SYSTEM_PYTHON,
            version: self.version.clone(),
            source: self.source.clone(),
            sha256: self.sha256.clone(),
            archive: self.archive,
            executable: install_dir.join(&self.entrypoint),
            install_dir,
            mutate_system_path: false,
            visible_as_user_package: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPythonPlan {
    pub runtime_key: &'static str,
    pub version: String,
    pub source: Url,
    pub sha256: String,
    pub archive: PortableArchive,
    pub install_dir: PathBuf,
    pub executable: PathBuf,
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

fn canonical_sha256(value: String) -> Result<String, InstallRequestError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(InstallRequestError::InvalidSha256(value));
    }
    Ok(value.to_ascii_lowercase())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_python_is_hidden_portable_and_never_mutates_path() {
        let bootstrap = SystemPythonBootstrap::new(
            "3.13.7",
            "https://runtime.kastrick.invalid/python-3.13.7.zip",
            "a".repeat(64),
            PortableArchive::Zip,
            "python.exe",
        )
        .unwrap();
        let plan = bootstrap
            .plan(Path::new("/users/kate/.rbe"), "windows-x86_64")
            .unwrap();
        assert_eq!(plan.runtime_key, RBE_SYSTEM_PYTHON);
        assert_eq!(
            plan.install_dir,
            PathBuf::from("/users/kate/.rbe/system/rbe.sys.python/3.13.7/windows-x86_64")
        );
        assert!(!plan.mutate_system_path);
        assert!(!plan.visible_as_user_package);
    }
}
