use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rbe_library_package::{inspect_zip, ArchivePolicy};
use rbe_project_package::ProjectCacheLayout;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::{InstallRuntimeError, VerifiedRpxRootSnapshot};

pub const PACKAGE_SERVICE_CAPABILITY: &str = "service:package";
pub const MAX_PACKAGE_SERVICES: usize = 32;
pub const MAX_PACKAGE_SERVICE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPackageServiceSource {
    pub package: String,
    pub artifact_sha256: String,
    pub archive_path: String,
    pub source: String,
    pub source_sha256: String,
}

/// Re-open one installed package through its active SHA-pinned artifact and
/// return the package-owned `.service` sources it is allowed to contribute.
///
/// Package services are deliberately convention based instead of copied into
/// application source: a package must request `service:package` and place the
/// source under `service/**/*.service` in its verified archive. The returned
/// records are source data only; Service Mother remains responsible for
/// compiling, qualifying, and owning their lifecycle.
pub fn read_verified_package_services(
    project_root: &Path,
    snapshot: &VerifiedRpxRootSnapshot,
) -> Result<Vec<VerifiedPackageServiceSource>, PackageServiceError> {
    let requested = snapshot
        .worker
        .read_requested_capabilities(project_root)
        .map_err(PackageServiceError::Install)?;
    if !requested
        .iter()
        .any(|capability| capability == PACKAGE_SERVICE_CAPABILITY)
    {
        return Ok(Vec::new());
    }

    let layout = ProjectCacheLayout::new(project_root);
    let artifact_path = layout
        .library_artifact_dir(&snapshot.artifact_sha256)?
        .join("artifact.rbe");
    verify_artifact(&artifact_path, &snapshot.artifact_sha256)?;

    let policy = ArchivePolicy::default();
    let inspected = inspect_zip(File::open(&artifact_path)?, policy)?;
    if inspected.manifest.name != snapshot.package
        || inspected.manifest.version != snapshot.version
        || inspected.manifest.runtime.kind != snapshot.worker.runtime_kind
        || inspected.manifest.runtime.entry != snapshot.worker.runtime_entry
        || inspected.manifest.runtime.managed != snapshot.worker.runtime_managed
    {
        return Err(PackageServiceError::SnapshotDrift(
            snapshot.package.clone(),
        ));
    }
    if inspected
        .manifest
        .capabilities
        .get(PACKAGE_SERVICE_CAPABILITY)
        != Some(&true)
    {
        return Err(PackageServiceError::SnapshotDrift(
            snapshot.package.clone(),
        ));
    }

    let mut service_entries = inspected
        .entries
        .iter()
        .filter(|entry| {
            !entry.directory
                && entry.path.starts_with("service/")
                && entry.path.ends_with(".service")
        })
        .collect::<Vec<_>>();
    service_entries.sort_by(|left, right| left.path.cmp(&right.path));

    if service_entries.is_empty() {
        return Err(PackageServiceError::CapabilityWithoutService {
            package: snapshot.package.clone(),
        });
    }
    if service_entries.len() > MAX_PACKAGE_SERVICES {
        return Err(PackageServiceError::TooManyServices {
            package: snapshot.package.clone(),
            observed: service_entries.len(),
            maximum: MAX_PACKAGE_SERVICES,
        });
    }

    let mut archive = ZipArchive::new(File::open(&artifact_path)?)?;
    let mut services = Vec::with_capacity(service_entries.len());
    for entry in service_entries {
        if entry.size > MAX_PACKAGE_SERVICE_BYTES {
            return Err(PackageServiceError::ServiceTooLarge {
                package: snapshot.package.clone(),
                path: entry.path.clone(),
                observed: entry.size,
                maximum: MAX_PACKAGE_SERVICE_BYTES,
            });
        }

        let mut source = archive.by_name(&entry.path)?;
        if source.is_dir() || source.size() != entry.size {
            return Err(PackageServiceError::ArchiveEntryDrift {
                package: snapshot.package.clone(),
                path: entry.path.clone(),
            });
        }
        let capacity = usize::try_from(entry.size).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        source
            .by_ref()
            .take(MAX_PACKAGE_SERVICE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PACKAGE_SERVICE_BYTES {
            return Err(PackageServiceError::ServiceTooLarge {
                package: snapshot.package.clone(),
                path: entry.path.clone(),
                observed: bytes.len() as u64,
                maximum: MAX_PACKAGE_SERVICE_BYTES,
            });
        }
        let source = String::from_utf8(bytes).map_err(|_| PackageServiceError::ServiceUtf8 {
            package: snapshot.package.clone(),
            path: entry.path.clone(),
        })?;
        let source_sha256 = format!("{:x}", Sha256::digest(source.as_bytes()));
        services.push(VerifiedPackageServiceSource {
            package: snapshot.package.clone(),
            artifact_sha256: snapshot.artifact_sha256.to_ascii_lowercase(),
            archive_path: entry.path.clone(),
            source,
            source_sha256,
        });
    }

    // Re-hash after all archive reads so a concurrent cache replacement cannot
    // turn an unverified service body into trusted Service Mother input.
    verify_artifact(&artifact_path, &snapshot.artifact_sha256)?;
    Ok(services)
}

fn verify_artifact(path: &Path, expected_sha256: &str) -> Result<(), PackageServiceError> {
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PackageServiceError::UnsafeArtifact(path.to_path_buf()));
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0))?;
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(PackageServiceError::ArtifactHashMismatch {
            path: path.to_path_buf(),
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), PackageServiceError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PackageServiceError::SymlinkedPath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PackageServiceError {
    #[error(transparent)]
    Install(InstallRuntimeError),
    #[error(transparent)]
    ProjectPackage(#[from] rbe_project_package::ProjectPackageError),
    #[error(transparent)]
    Archive(#[from] rbe_library_package::ArchiveError),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error("package service discovery I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("verified package service artifact is not a safe regular file: {0}")]
    UnsafeArtifact(PathBuf),
    #[error("verified package service path traverses a symbolic link: {0}")]
    SymlinkedPath(PathBuf),
    #[error("verified package service artifact hash mismatch at {path}: expected {expected}, got {actual}")]
    ArtifactHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("verified package metadata drifted before service discovery for {0:?}")]
    SnapshotDrift(String),
    #[error("package {package:?} requests service:package but contains no service/**/*.service source")]
    CapabilityWithoutService { package: String },
    #[error("package {package:?} contributes {observed} services; maximum is {maximum}")]
    TooManyServices {
        package: String,
        observed: usize,
        maximum: usize,
    },
    #[error("package {package:?} service {path:?} is {observed} bytes; maximum is {maximum}")]
    ServiceTooLarge {
        package: String,
        path: String,
        observed: u64,
        maximum: u64,
    },
    #[error("package {package:?} service {path:?} is not valid UTF-8")]
    ServiceUtf8 { package: String, path: String },
    #[error("package {package:?} service archive entry changed during verified read: {path:?}")]
    ArchiveEntryDrift { package: String, path: String },
}
