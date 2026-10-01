use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use atomic_io::AtomicIo;
use rbe_install_executor::{
    DiskBudget, DiskBudgetInput, DiskBudgetPolicy, SystemRuntimeArtifactPlan,
};
use rbe_install_request::{
    PortableArchive, SystemRuntimeKind, SystemRuntimeManifest, SystemRuntimeManifestRequest,
    SystemRuntimePlan,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use zip::ZipArchive;

use crate::http::get_following_redirects;

pub const SYSTEM_RUNTIME_ADMISSION_FORMAT: u32 = 1;
pub const SYSTEM_RUNTIME_MANIFEST_MAX_BYTES: u64 = 128 * 1024;
pub const SYSTEM_RUNTIME_ADMISSION_MAX_BYTES: u64 = 64 * 1024;
pub const SYSTEM_RUNTIME_ADMISSION_FILE: &str = "admission.rbe.json";
const MAX_RUNTIME_FILES: usize = 32_768;
const MAX_RUNTIME_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_RUNTIME_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RUNTIME_PATH_BYTES: usize = 4 * 1024;
const MANIFEST_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const MANIFEST_IDLE_TIMEOUT_SECONDS: u64 = 30;
const MANIFEST_MAX_REDIRECTS: u8 = 5;

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

impl AdmittedSystemRuntime {
    /// Fetch, validate, materialize and admit one managed `rbe.sys.*` runtime.
    ///
    /// This is the production bridge used by RPX/install orchestration. The
    /// registry manifest is fetched through the hardened install-runtime HTTP
    /// client; no shell, PATH lookup, host package manager or arbitrary socket
    /// access is used. ZIP and raw artifacts are currently materialized. The
    /// manifest keeps tar.gz/tar.xz reserved, but those formats fail closed
    /// until a trusted in-process decoder is part of this crate.
    pub async fn hydrate_from_registry(
        request: &SystemRuntimeManifestRequest,
        cache_root: impl AsRef<Path>,
    ) -> Result<(Self, SystemRuntimeManifest), SystemRuntimeAdmissionError> {
        let manifest = fetch_manifest(request).await?;
        let cache_root = cache_root.as_ref();
        prepare_cache_root(cache_root)?;
        let plan = manifest.plan(cache_root, request.runtime, &request.host)?;

        if plan.install_dir.exists() {
            let admission_path = plan.install_dir.join(SYSTEM_RUNTIME_ADMISSION_FILE);
            if admission_path.exists() {
                let admitted = verify_admission_for_plan(&plan)?;
                persist_manifest(&plan, &manifest)?;
                return Ok((admitted, manifest));
            }
            remove_incomplete_install_dir(&plan.install_dir)?;
        }

        let artifact = SystemRuntimeArtifactPlan::from_runtime(&plan)?;
        download_runtime_artifact(&artifact).await?;
        materialize_runtime(&plan, &artifact)?;

        let admission = admission_for_materialized_plan(&plan)?;
        let admission_bytes = serde_json::to_vec_pretty(&admission).map_err(|source| {
            SystemRuntimeAdmissionError::SerializeMetadata {
                label: "runtime admission",
                source,
            }
        })?;
        AtomicIo::new().write_atomic(
            &plan.install_dir.join(SYSTEM_RUNTIME_ADMISSION_FILE),
            &admission_bytes,
        )?;
        persist_manifest(&plan, &manifest)?;

        let admitted = verify_admission_for_plan(&plan)?;
        cleanup_staging_file(&artifact.staging_path)?;
        Ok((admitted, manifest))
    }
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

async fn fetch_manifest(
    request: &SystemRuntimeManifestRequest,
) -> Result<SystemRuntimeManifest, SystemRuntimeAdmissionError> {
    let mut response = get_following_redirects(
        request.endpoint.clone(),
        None,
        MANIFEST_CONNECT_TIMEOUT_SECONDS,
        MANIFEST_IDLE_TIMEOUT_SECONDS,
        MANIFEST_MAX_REDIRECTS,
    )
    .await?;
    if !response.status().is_success() {
        return Err(SystemRuntimeAdmissionError::ManifestHttpStatus(
            response.status().as_u16(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > SYSTEM_RUNTIME_MANIFEST_MAX_BYTES)
    {
        return Err(SystemRuntimeAdmissionError::RemoteManifestTooLarge);
    }

    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(
            Duration::from_secs(MANIFEST_IDLE_TIMEOUT_SECONDS),
            response.chunk(),
        )
        .await
        .map_err(|_| SystemRuntimeAdmissionError::ManifestBodyTimeout)?
        .map_err(crate::InstallRuntimeError::HttpBody)?;
        let Some(chunk) = chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > SYSTEM_RUNTIME_MANIFEST_MAX_BYTES as usize {
            return Err(SystemRuntimeAdmissionError::RemoteManifestTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| SystemRuntimeAdmissionError::RemoteManifestUtf8)?;
    SystemRuntimeManifest::parse_json(text, request.runtime, &request.host).map_err(Into::into)
}

async fn download_runtime_artifact(
    artifact: &SystemRuntimeArtifactPlan,
) -> Result<(), SystemRuntimeAdmissionError> {
    let staging_parent = artifact.staging_path.parent().ok_or_else(|| {
        SystemRuntimeAdmissionError::UnsafePath(artifact.staging_path.clone())
    })?;
    ensure_no_symlink_components(staging_parent)?;
    std::fs::create_dir_all(staging_parent)?;
    ensure_no_symlink_components(staging_parent)?;

    let available_bytes = fs2::available_space(staging_parent)?;
    DiskBudget::plan(
        DiskBudgetInput {
            artifact_bytes: artifact.size_bytes,
            reusable_partial_bytes: 0,
            available_bytes,
        },
        DiskBudgetPolicy::default(),
    )?;

    if artifact.staging_path.exists() {
        ensure_safe_regular_file(&artifact.staging_path)?;
        std::fs::remove_file(&artifact.staging_path)?;
    }

    let mut response = get_following_redirects(
        artifact.source.clone(),
        None,
        artifact.limits.connect_timeout_seconds,
        artifact.limits.idle_timeout_seconds,
        artifact.limits.maximum_redirects,
    )
    .await?;
    if !response.status().is_success() {
        return Err(SystemRuntimeAdmissionError::ArtifactHttpStatus(
            response.status().as_u16(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != artifact.size_bytes)
    {
        return Err(SystemRuntimeAdmissionError::ArtifactContentLengthMismatch);
    }

    let mut verifier = artifact.verifier()?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&artifact.staging_path)
        .await?;
    loop {
        let chunk = tokio::time::timeout(
            Duration::from_secs(artifact.limits.idle_timeout_seconds.max(1)),
            response.chunk(),
        )
        .await
        .map_err(|_| SystemRuntimeAdmissionError::ArtifactBodyTimeout)?
        .map_err(crate::InstallRuntimeError::HttpBody)?;
        let Some(chunk) = chunk else {
            break;
        };
        verifier.update(&chunk)?;
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    let verified = verifier.finish()?;
    if verified.size_bytes != artifact.size_bytes
        || !verified.sha256.eq_ignore_ascii_case(&artifact.sha256)
    {
        return Err(SystemRuntimeAdmissionError::ArtifactVerificationDrift);
    }
    Ok(())
}

fn materialize_runtime(
    plan: &SystemRuntimePlan,
    artifact: &SystemRuntimeArtifactPlan,
) -> Result<(), SystemRuntimeAdmissionError> {
    if plan.install_dir.exists() {
        return Err(SystemRuntimeAdmissionError::InstallDirectoryExists(
            plan.install_dir.clone(),
        ));
    }
    let install_parent = plan.install_dir.parent().ok_or_else(|| {
        SystemRuntimeAdmissionError::UnsafePath(plan.install_dir.clone())
    })?;
    ensure_no_symlink_components(install_parent)?;
    std::fs::create_dir_all(install_parent)?;
    ensure_no_symlink_components(install_parent)?;

    let staging_root = plan
        .cache_root
        .join(".staging")
        .join(format!("unpack-{}", artifact.sha256));
    if staging_root.exists() {
        ensure_safe_directory(&staging_root)?;
        std::fs::remove_dir_all(&staging_root)?;
    }
    std::fs::create_dir(&staging_root)?;

    let result = match artifact.archive {
        PortableArchive::Zip => extract_zip_runtime(plan, artifact, &staging_root),
        PortableArchive::Raw => extract_raw_runtime(plan, artifact, &staging_root),
        PortableArchive::TarGz | PortableArchive::TarXz => Err(
            SystemRuntimeAdmissionError::UnsupportedArchive(artifact.archive),
        ),
    };
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(error);
    }

    let relative_entrypoint = plan
        .executable
        .strip_prefix(&plan.install_dir)
        .map_err(|_| SystemRuntimeAdmissionError::UnsafePath(plan.executable.clone()))?;
    let staged_executable = staging_root.join(relative_entrypoint);
    ensure_safe_regular_file(&staged_executable)?;
    harden_executable(&staged_executable)?;

    std::fs::rename(&staging_root, &plan.install_dir)?;
    ensure_safe_regular_file(&plan.executable)?;
    Ok(())
}

fn extract_raw_runtime(
    plan: &SystemRuntimePlan,
    artifact: &SystemRuntimeArtifactPlan,
    staging_root: &Path,
) -> Result<(), SystemRuntimeAdmissionError> {
    let relative = plan
        .executable
        .strip_prefix(&plan.install_dir)
        .map_err(|_| SystemRuntimeAdmissionError::UnsafePath(plan.executable.clone()))?;
    validate_relative_path(relative)?;
    let destination = staging_root.join(relative);
    let parent = destination.parent().ok_or_else(|| {
        SystemRuntimeAdmissionError::UnsafePath(destination.clone())
    })?;
    std::fs::create_dir_all(parent)?;
    let mut source = File::open(&artifact.staging_path)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)?;
    let copied = std::io::copy(&mut source, &mut target)?;
    target.flush()?;
    target.sync_all()?;
    if copied != artifact.size_bytes {
        return Err(SystemRuntimeAdmissionError::MaterializedSizeMismatch {
            expected: artifact.size_bytes,
            actual: copied,
        });
    }
    Ok(())
}

fn extract_zip_runtime(
    _plan: &SystemRuntimePlan,
    artifact: &SystemRuntimeArtifactPlan,
    staging_root: &Path,
) -> Result<(), SystemRuntimeAdmissionError> {
    let mut archive = ZipArchive::new(File::open(&artifact.staging_path)?)?;
    let mut seen = BTreeSet::new();
    let mut total_bytes = 0u64;
    let mut file_count = 0usize;

    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = normalized_zip_path(file.name(), file.is_dir())?;
        let collision_key = name.to_ascii_lowercase();
        if !seen.insert(collision_key) {
            return Err(SystemRuntimeAdmissionError::DuplicateArchivePath(name));
        }
        reject_non_regular_zip_mode(file.unix_mode(), file.is_dir(), &name)?;
        if !file.is_dir() {
            file_count = file_count.saturating_add(1);
            if file_count > MAX_RUNTIME_FILES {
                return Err(SystemRuntimeAdmissionError::TooManyArchiveFiles(file_count));
            }
            if file.size() > MAX_RUNTIME_FILE_BYTES {
                return Err(SystemRuntimeAdmissionError::ArchiveFileTooLarge {
                    path: name,
                    size: file.size(),
                });
            }
            total_bytes = total_bytes
                .checked_add(file.size())
                .ok_or(SystemRuntimeAdmissionError::ArchiveSizeOverflow)?;
            if total_bytes > MAX_RUNTIME_UNPACKED_BYTES {
                return Err(SystemRuntimeAdmissionError::ArchiveTooLarge(total_bytes));
            }
        }
    }

    let available_bytes = fs2::available_space(staging_root)?;
    DiskBudget::plan(
        DiskBudgetInput {
            artifact_bytes: 0,
            reusable_partial_bytes: 0,
            available_bytes,
        },
        DiskBudgetPolicy {
            unpack_overhead_bytes: total_bytes,
            ..Default::default()
        },
    )?;

    let mut archive = ZipArchive::new(File::open(&artifact.staging_path)?)?;
    for index in 0..archive.len() {
        let mut source = archive.by_index(index)?;
        let name = normalized_zip_path(source.name(), source.is_dir())?;
        let destination = join_relative(staging_root, name.trim_end_matches('/'))?;
        if source.is_dir() {
            std::fs::create_dir_all(&destination)?;
            continue;
        }
        let parent = destination.parent().ok_or_else(|| {
            SystemRuntimeAdmissionError::UnsafePath(destination.clone())
        })?;
        std::fs::create_dir_all(parent)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let copied = std::io::copy(&mut source, &mut target)?;
        target.flush()?;
        if copied != source.size() {
            return Err(SystemRuntimeAdmissionError::MaterializedSizeMismatch {
                expected: source.size(),
                actual: copied,
            });
        }
    }
    Ok(())
}

fn persist_manifest(
    plan: &SystemRuntimePlan,
    manifest: &SystemRuntimeManifest,
) -> Result<(), SystemRuntimeAdmissionError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|source| {
        SystemRuntimeAdmissionError::SerializeMetadata {
            label: "runtime manifest",
            source,
        }
    })?;
    if bytes.len() as u64 > SYSTEM_RUNTIME_MANIFEST_MAX_BYTES {
        return Err(SystemRuntimeAdmissionError::RemoteManifestTooLarge);
    }
    AtomicIo::new().write_atomic(&plan.manifest_cache_path, &bytes)?;
    Ok(())
}

fn prepare_cache_root(cache_root: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    if !cache_root.is_absolute() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(
            cache_root.to_path_buf(),
        ));
    }
    ensure_no_symlink_components(cache_root)?;
    std::fs::create_dir_all(cache_root)?;
    ensure_no_symlink_components(cache_root)?;
    Ok(())
}

fn remove_incomplete_install_dir(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    ensure_safe_directory(path)?;
    let admission = path.join(SYSTEM_RUNTIME_ADMISSION_FILE);
    if admission.exists() {
        return Err(SystemRuntimeAdmissionError::ExistingAdmissionInvalid(path.to_path_buf()));
    }
    std::fs::remove_dir_all(path)?;
    Ok(())
}

fn cleanup_staging_file(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    if !path.exists() {
        return Ok(());
    }
    ensure_safe_regular_file(path)?;
    std::fs::remove_file(path)?;
    Ok(())
}

fn normalized_zip_path(value: &str, directory: bool) -> Result<String, SystemRuntimeAdmissionError> {
    if value.is_empty()
        || value.len() > MAX_RUNTIME_PATH_BYTES
        || value.contains(['\\', '\0', ':'])
        || value.starts_with('/')
    {
        return Err(SystemRuntimeAdmissionError::UnsafeArchivePath(
            value.to_string(),
        ));
    }
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(SystemRuntimeAdmissionError::UnsafeArchivePath(
            value.to_string(),
        ));
    }
    let mut normalized = trimmed.to_string();
    if directory {
        normalized.push('/');
    }
    Ok(normalized)
}

fn reject_non_regular_zip_mode(
    mode: Option<u32>,
    directory: bool,
    path: &str,
) -> Result<(), SystemRuntimeAdmissionError> {
    let Some(mode) = mode else {
        return Ok(());
    };
    let kind = mode & 0o170000;
    if kind == 0 {
        return Ok(());
    }
    let expected = if directory { 0o040000 } else { 0o100000 };
    if kind != expected {
        return Err(SystemRuntimeAdmissionError::UnsafeArchiveEntry(
            path.to_string(),
        ));
    }
    Ok(())
}

fn join_relative(root: &Path, relative: &str) -> Result<PathBuf, SystemRuntimeAdmissionError> {
    let relative = Path::new(relative);
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

fn validate_relative_path(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    for component in path.components() {
        match component {
            Component::Normal(value) if !value.is_empty() => {}
            _ => return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf())),
        }
    }
    Ok(())
}

fn harden_executable(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
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
    let file = File::open(path)?;
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

fn ensure_safe_directory(path: &Path) -> Result<(), SystemRuntimeAdmissionError> {
    if !path.is_absolute() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SystemRuntimeAdmissionError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
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
    #[error("managed runtime path is not a safe regular file/directory: {0}")]
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
    #[error("remote managed runtime manifest is not valid UTF-8")]
    RemoteManifestUtf8,
    #[error("remote managed runtime manifest exceeded the bounded metadata limit")]
    RemoteManifestTooLarge,
    #[error("remote managed runtime manifest body stalled")]
    ManifestBodyTimeout,
    #[error("managed runtime manifest server returned HTTP {0}")]
    ManifestHttpStatus(u16),
    #[error("managed runtime artifact server returned HTTP {0}")]
    ArtifactHttpStatus(u16),
    #[error("managed runtime artifact Content-Length does not match the pinned size")]
    ArtifactContentLengthMismatch,
    #[error("managed runtime artifact body stalled")]
    ArtifactBodyTimeout,
    #[error("managed runtime artifact verification drifted after the pinned verifier completed")]
    ArtifactVerificationDrift,
    #[error("managed runtime admission JSON at {path} is invalid: {source}")]
    AdmissionJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not serialize {label}: {source}")]
    SerializeMetadata {
        label: &'static str,
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
    #[error("existing managed runtime admission is invalid; refusing to re-pin possibly modified bytes at {0}")]
    ExistingAdmissionInvalid(PathBuf),
    #[error("managed runtime install directory already exists: {0}")]
    InstallDirectoryExists(PathBuf),
    #[error("managed runtime archive format {0:?} is not yet supported by the trusted in-process hydrator; publish this host artifact as ZIP or raw")]
    UnsupportedArchive(PortableArchive),
    #[error("managed runtime archive contains unsafe path {0:?}")]
    UnsafeArchivePath(String),
    #[error("managed runtime archive contains a symlink/special entry {0:?}")]
    UnsafeArchiveEntry(String),
    #[error("managed runtime archive contains duplicate/case-colliding path {0:?}")]
    DuplicateArchivePath(String),
    #[error("managed runtime archive contains too many files: {0}")]
    TooManyArchiveFiles(usize),
    #[error("managed runtime archive file {path:?} is too large: {size} bytes")]
    ArchiveFileTooLarge { path: String, size: u64 },
    #[error("managed runtime archive size accounting overflow")]
    ArchiveSizeOverflow,
    #[error("managed runtime archive expands beyond the allowed size: {0} bytes")]
    ArchiveTooLarge(u64),
    #[error("materialized managed runtime size mismatch: expected {expected}, got {actual}")]
    MaterializedSizeMismatch { expected: u64, actual: u64 },
    #[error(transparent)]
    Manifest(#[from] rbe_install_request::InstallRequestError),
    #[error(transparent)]
    Executor(#[from] rbe_install_executor::ExecutorError),
    #[error(transparent)]
    Transport(#[from] crate::InstallRuntimeError),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
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

    #[test]
    fn zip_paths_reject_traversal_absolute_and_windows_drive_forms() {
        for path in ["../node", "/node", "bin/../node", "C:/node", "bin\\node"] {
            assert!(normalized_zip_path(path, false).is_err(), "{path}");
        }
        assert_eq!(normalized_zip_path("bin/node", false).unwrap(), "bin/node");
    }
}
