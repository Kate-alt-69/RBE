use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use rbe_install_request::{SystemRuntimeKind, SystemRuntimeManifest, SystemRuntimePlan};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SYSTEM_RUNTIME_ADMISSION_FORMAT: u32 = 1;
pub const SYSTEM_RUNTIME_MANIFEST_MAX_BYTES: u64 = 128 * 1024;
pub const SYSTEM_RUNTIME_ADMISSION_MAX_BYTES: u64 = 64 * 1024;
pub const SYSTEM_RUNTIME_ADMISSION_FILE: &str = "admission.rbe.json";

/// Exact runtime identity admitted by trusted RBE hydration.
///
/// The archive digest proves which downloaded artifact was accepted; the
/// executable digest separately pins the extracted bytes that `script` is
/// allowed to execute. Runtime callers must re-hash the executable on every
/// use instead of treating a cache path as authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedSystemRuntime {
    pub runtime: SystemRuntimeKind,
    pub version: String,
    pub host: String,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub artifact_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemRuntimeAdmission {
    pub format: u32,
    pub runtime: String,
    pub version: String,
    pub host: String,
    pub artifact_sha256: String,
    pub executable: String,
    pub executable_sha256: String,
}

pub fn current_system_runtime_host() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Rehydrate one already-admitted managed runtime from the project-local cache.
///
/// This function never looks at PATH and never creates a new identity from the
/// bytes currently on disk. Both the cached registry manifest and the trusted
/// admission record must agree, and the exact executable is SHA-256 verified
/// before it is returned.
pub fn load_admitted_system_runtime(
    project_root: impl AsRef<Path>,
    runtime: SystemRuntimeKind,
) -> Result<AdmittedSystemRuntime, SystemRuntimeAdmissionError> {
    let project_root = project_root.as_ref();
    if !project_root.is_absolute() || !project_root.is_dir() {
        return Err(SystemRuntimeAdmissionError::InvalidProjectRoot(
            project_root.to_path_buf(),
        ));
    }
    ensure_no_symlink_components(project_root)?;

    let host = current_system_runtime_host();
    let cache_root = project_root.join(".cache");
    let runtime_root = cache_root
        .join("rbe")
        .join("sys")
        .join(runtime.cache_component());
    let manifest_path = runtime_root.join("manifest.json");
    let manifest_bytes = read_bounded_regular_file(
        &manifest_path,
        SYSTEM_RUNTIME_MANIFEST_MAX_BYTES,
        "runtime manifest",
    )?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|_| SystemRuntimeAdmissionError::ManifestUtf8(manifest_path.clone()))?;
    let manifest = SystemRuntimeManifest::parse_json(manifest_text, runtime, &host)?;
    let plan = manifest.plan(&cache_root, runtime, &host)?;
    verify_admission_for_plan(&plan)
}

pub(crate) fn verify_admission_for_plan(
    plan: &SystemRuntimePlan,
) -> Result<AdmittedSystemRuntime, SystemRuntimeAdmissionError> {
    let admission_path = plan.install_dir.join(SYSTEM_RUNTIME_ADMISSION_FILE);
    let bytes = read_bounded_regular_file(
        &admission_path,
        SYSTEM_RUNTIME_ADMISSION_MAX_BYTES,
        "runtime admission",
    )?;
    let admission: SystemRuntimeAdmission = serde_json::from_slice(&bytes)
        .map_err(|source| SystemRuntimeAdmissionError::AdmissionJson {
            path: admission_path.clone(),
            source,
        })?;

    if admission.format != SYSTEM_RUNTIME_ADMISSION_FORMAT {
        return Err(SystemRuntimeAdmissionError::AdmissionMismatch(format!(
            "unsupported runtime admission format {}",
            admission.format
        )));
    }
    if admission.runtime != plan.runtime.key()
        || admission.version != plan.version
        || admission.host != plan.host
        || !admission
            .artifact_sha256
            .eq_ignore_ascii_case(&plan.sha256)
    {
        return Err(SystemRuntimeAdmissionError::AdmissionMismatch(
            "runtime/version/host/artifact identity does not match the cached manifest".into(),
        ));
    }

    let expected_relative = relative_utf8(&plan.install_dir, &plan.executable)?;
    if admission.executable != expected_relative {
        return Err(SystemRuntimeAdmissionError::AdmissionMismatch(format!(
            "runtime entrypoint mismatch: expected {expected_relative:?}, got {:?}",
            admission.executable
        )));
    }
    validate_sha256(&admission.executable_sha256)?;
    ensure_safe_regular_file(&plan.executable)?;
    let actual = sha256_file(&plan.executable)?;
    if !actual.eq_ignore_ascii_case(&admission.executable_sha256) {
        return Err(SystemRuntimeAdmissionError::ExecutableHashMismatch {
            path: plan.executable.clone(),
            expected: admission.executable_sha256,
            actual,
        });
    }

    Ok(AdmittedSystemRuntime {
        runtime: plan.runtime,
        version: plan.version.clone(),
        host: plan.host.clone(),
        executable: plan.executable.clone(),
        executable_sha256: actual,
        artifact_sha256: plan.sha256.clone(),
    })
}

pub(crate) fn admission_for_materialized_plan(
    plan: &SystemRuntimePlan,
) -> Result<SystemRuntimeAdmission, SystemRuntimeAdmissionError> {
    ensure_safe_regular_file(&plan.executable)?;
    let executable_sha256 = sha256_file(&plan.executable)?;
    Ok(SystemRuntimeAdmission {
        format: SYSTEM_RUNTIME_ADMISSION_FORMAT,
        runtime: plan.runtime.key().to_string(),
        version: plan.version.clone(),
        host: plan.host.clone(),
        artifact_sha256: plan.sha256.clone(),
        executable: relative_utf8(&plan.install_dir, &plan.executable)?,
        executable_sha256,
    })
}

fn read_bounded_regular_file(
    path: &Path,
    maximum_bytes: u64,
    label: &'static str,
) -> Result<Vec<u8>, SystemRuntimeAdmissionError> {
    ensure_safe_regular_file(path)?;
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > maximum_bytes {
        return Err(SystemRuntimeAdmissionError::MetadataTooLarge {
            label,
            path: path.to_path_buf(),
            maximum_bytes,
            observed_bytes: metadata.len(),
        });
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(SystemRuntimeAdmissionError::MetadataTooLarge {
            label,
            path: path.to_path_buf(),
            maximum_bytes,
            observed_bytes: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn ensure_safe_regular_file(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    if !path.is_absolute() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SystemRuntimeAdmissionError::SymlinkedPath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn relative_utf8(root: &Path, path: &Path) -> Result<String, SystemRuntimeAdmissionError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
        };
        let value = value
            .to_str()
            .ok_or_else(|| SystemRuntimeAdmissionError::NonUtf8Path(path.to_path_buf()))?;
        if value.is_empty() || matches!(value, "." | "..") {
            return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
        }
        parts.push(value);
    }
    if parts.is_empty() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    Ok(parts.join("/"))
}

fn validate_sha256(value: &str) -> Result<(), SystemRuntimeAdmissionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SystemRuntimeAdmissionError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, SystemRuntimeAdmissionError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, thiserror::Error)]
pub enum SystemRuntimeAdmissionError {
    #[error("RBE project root must be an existing absolute directory: {0}")]
    InvalidProjectRoot(PathBuf),
    #[error("managed runtime path traverses a symbolic link: {0}")]
    SymlinkedPath(PathBuf),
    #[error("managed runtime path is not a safe regular file: {0}")]
    UnsafePath(PathBuf),
    #[error("managed runtime path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("managed {label} at {path} exceeds {maximum_bytes} bytes (observed {observed_bytes})")]
    MetadataTooLarge {
        label: &'static str,
        path: PathBuf,
        maximum_bytes: u64,
        observed_bytes: u64,
    },
    #[error("managed runtime manifest is not valid UTF-8: {0}")]
    ManifestUtf8(PathBuf),
    #[error("managed runtime admission JSON at {path} is invalid: {source}")]
    AdmissionJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("managed runtime admission does not match the cached manifest: {0}")]
    AdmissionMismatch(String),
    #[error("invalid managed runtime SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("managed runtime executable changed after admission at {path}: expected SHA-256 {expected}, got {actual}")]
    ExecutableHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Manifest(#[from] rbe_install_request::InstallRequestError),
    #[error("managed runtime cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_host_matches_registry_host_shape() {
        let host = current_system_runtime_host();
        assert!(host.contains('-'));
        assert!(!host.contains('/') && !host.contains('\\'));
    }

    #[test]
    fn admission_format_is_versioned() {
        assert_eq!(SYSTEM_RUNTIME_ADMISSION_FORMAT, 1);
    }
}
