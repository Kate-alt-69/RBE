use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rbe_install_executor::{ArtifactDownloadPlan, DownloadLimits, ResumePolicy};
use rbe_install_request::{validate_registry_package_name, RegistryPackageRelease};
use rbe_library_package::{inspect_zip, ArchivePolicy, LibraryManifest, LIBRARY_MANIFEST};
use rbe_project_package::{
    LockedProjectPackage, LockedToolchain, ProjectCacheLayout, ProjectPackageLock,
};
use sha2::{Digest, Sha256};
use zip::result::ZipError;
use zip::ZipArchive;

use crate::{stage_artifact, ArtifactStage, InstallRuntimeError};

pub const RPX_PACKAGE_INDEX: &str = ".rbe/package-index.json";
pub const MAX_RPX_PACKAGE_INDEX_BYTES: u64 = 512 * 1024;

#[derive(Debug)]
pub struct VerifiedRegistryPackage {
    pub stage: ArtifactStage,
    pub manifest: LibraryManifest,
    pub manifest_sha256: String,
    pub locked: LockedProjectPackage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRpxRootIndex {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub index_json: String,
}

pub fn registry_artifact_plan(
    layout: &ProjectCacheLayout,
    package: &str,
    release: &RegistryPackageRelease,
) -> Result<ArtifactDownloadPlan, InstallRuntimeError> {
    validate_registry_package_name(package)?;
    release.validate()?;
    if release.yanked {
        return Err(InstallRuntimeError::YankedRegistryRelease {
            package: package.to_string(),
            version: release.version.clone(),
        });
    }

    let expected_sha256 = release.artifact.sha256.to_ascii_lowercase();
    let final_dir = layout.library_artifact_dir(&expected_sha256)?;
    let staging_dir = layout
        .library_cache_root()
        .join(".staging")
        .join(&expected_sha256);

    Ok(ArtifactDownloadPlan {
        package: package.to_string(),
        version: release.version.clone(),
        source: release.artifact.source_url()?,
        expected_sha256,
        expected_size_bytes: Some(release.artifact.size_bytes),
        partial_path: staging_dir.join("artifact.rbe.part"),
        staging_dir,
        final_artifact_path: final_dir.join("artifact.rbe"),
        final_dir,
        limits: DownloadLimits::default(),
        resume: ResumePolicy::default(),
    })
}

pub async fn stage_registry_package(
    layout: &ProjectCacheLayout,
    package: &str,
    release: &RegistryPackageRelease,
) -> Result<VerifiedRegistryPackage, InstallRuntimeError> {
    let plan = registry_artifact_plan(layout, package, release)?;
    let stage = stage_artifact(&plan).await?;
    inspect_registry_stage(package, release, stage)
}

pub fn inspect_registry_stage(
    package: &str,
    release: &RegistryPackageRelease,
    stage: ArtifactStage,
) -> Result<VerifiedRegistryPackage, InstallRuntimeError> {
    validate_registry_package_name(package)?;
    release.validate()?;

    let policy = ArchivePolicy::default();
    let inspected = inspect_zip(File::open(&stage.promotion.verified_partial)?, policy)?;
    validate_manifest_matches_release(package, release, &inspected.manifest)?;
    let manifest_sha256 =
        hash_manifest(&stage.promotion.verified_partial, policy.max_manifest_bytes)?;

    let locked = LockedProjectPackage {
        version: release.version.clone(),
        resolved_from: format!("registry:{package}"),
        artifact_url: release.artifact.source.clone(),
        artifact_sha256: release.artifact.sha256.to_ascii_lowercase(),
        manifest_sha256: manifest_sha256.clone(),
        source_sha256: release
            .artifact
            .source_sha256
            .as_ref()
            .map(|value| value.to_ascii_lowercase()),
        dependencies: release.dependencies.clone(),
        runtime: Some(LockedToolchain {
            kind: inspected.manifest.runtime.kind.clone(),
            version: inspected.manifest.runtime.version.clone(),
        }),
        sdk: Some(LockedToolchain {
            kind: inspected.manifest.sdk.family.clone(),
            version: inspected.manifest.sdk.version.clone(),
        }),
    };

    Ok(VerifiedRegistryPackage {
        stage,
        manifest: inspected.manifest,
        manifest_sha256,
        locked,
    })
}

/// Read the public RPX package indexes for the project's explicit root packages.
///
/// This deliberately iterates `lock.packages` only. Root-scoped transitive
/// packages in `lock.private` are install details and can never become REL
/// package namespaces through this API. Every cache artifact is re-hashed
/// against the pinned lock SHA before its index is trusted.
pub fn read_verified_rpx_root_indexes(
    project_root: &Path,
) -> Result<Vec<VerifiedRpxRootIndex>, InstallRuntimeError> {
    let layout = ProjectCacheLayout::new(project_root);
    let lock_path = layout.lock_path();
    if !lock_path.try_exists()? {
        return Ok(Vec::new());
    }

    let lock = ProjectPackageLock::parse_yaml(&std::fs::read_to_string(&lock_path)?)?;
    let mut indexes = Vec::new();
    for (package, locked) in &lock.packages {
        if let Some(index_json) = read_verified_rpx_index(&layout, package, locked)? {
            indexes.push(VerifiedRpxRootIndex {
                package: package.clone(),
                version: locked.version.clone(),
                artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
                index_json,
            });
        }
    }
    Ok(indexes)
}

fn read_verified_rpx_index(
    layout: &ProjectCacheLayout,
    package: &str,
    locked: &LockedProjectPackage,
) -> Result<Option<String>, InstallRuntimeError> {
    let artifact_dir = layout.library_artifact_dir(&locked.artifact_sha256)?;
    let artifact_path = artifact_dir.join("artifact.rbe");
    verify_locked_cache_artifact(&artifact_path, &locked.artifact_sha256)?;

    let mut archive = ZipArchive::new(File::open(&artifact_path)?)?;
    let mut index = match archive.by_name(RPX_PACKAGE_INDEX) {
        Ok(index) => index,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if index.is_dir() {
        return Err(InstallRuntimeError::InvalidRpxPackageIndexEntry {
            package: package.to_string(),
        });
    }
    if index.size() > MAX_RPX_PACKAGE_INDEX_BYTES {
        return Err(InstallRuntimeError::RpxPackageIndexTooLarge {
            package: package.to_string(),
            limit: MAX_RPX_PACKAGE_INDEX_BYTES,
            observed: index.size(),
        });
    }

    let capacity = usize::try_from(index.size()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    index
        .by_ref()
        .take(MAX_RPX_PACKAGE_INDEX_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RPX_PACKAGE_INDEX_BYTES {
        return Err(InstallRuntimeError::RpxPackageIndexTooLarge {
            package: package.to_string(),
            limit: MAX_RPX_PACKAGE_INDEX_BYTES,
            observed: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| InstallRuntimeError::RpxPackageIndexUtf8 {
            package: package.to_string(),
        })
}

fn verify_locked_cache_artifact(
    path: &Path,
    expected_sha256: &str,
) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeCacheEntry(
            path.display().to_string(),
        ));
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
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256.to_ascii_lowercase() {
        return Err(InstallRuntimeError::ExistingArtifactMismatch {
            path: path.display().to_string(),
            expected_sha256: expected_sha256.to_ascii_lowercase(),
        });
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), InstallRuntimeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(InstallRuntimeError::SymlinkedPath(
                    current.display().to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_manifest_matches_release(
    package: &str,
    release: &RegistryPackageRelease,
    manifest: &LibraryManifest,
) -> Result<(), InstallRuntimeError> {
    require_match(package, "name", package, &manifest.name)?;
    require_match(package, "version", &release.version, &manifest.version)?;
    require_match(
        package,
        "rbe ABI range",
        &format!("{}..={}", release.rbe_abi_min, release.rbe_abi_max),
        &format!("{}..={}", manifest.rbe_abi_min, manifest.rbe_abi_max),
    )?;
    if release.dependencies != manifest.dependencies {
        return Err(InstallRuntimeError::PackageMetadataMismatch {
            package: package.to_string(),
            field: "dependencies",
            registry: format!("{:?}", release.dependencies),
            manifest: format!("{:?}", manifest.dependencies),
        });
    }
    Ok(())
}

fn require_match(
    package: &str,
    field: &'static str,
    registry: &str,
    manifest: &str,
) -> Result<(), InstallRuntimeError> {
    if registry != manifest {
        return Err(InstallRuntimeError::PackageMetadataMismatch {
            package: package.to_string(),
            field,
            registry: registry.to_string(),
            manifest: manifest.to_string(),
        });
    }
    Ok(())
}

fn hash_manifest(path: &Path, maximum_bytes: u64) -> Result<String, InstallRuntimeError> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut manifest = archive.by_name(LIBRARY_MANIFEST)?;
    if manifest.is_dir() || manifest.size() > maximum_bytes {
        return Err(InstallRuntimeError::InvalidManifestForHashing);
    }

    let capacity = usize::try_from(manifest.size()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    manifest
        .by_ref()
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(InstallRuntimeError::InvalidManifestForHashing);
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Cursor, Write};

    use rbe_install_request::RegistryArtifact;
    use rbe_project_package::ProjectPackageLock;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    use super::*;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "4.0.1"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "javascript"
package = "@rbe/sdk"
version = "0.1"

[runtime]
kind = "bun"
version = "1.3"
entry = "src/index.js"

[dependencies]
rbe-core = "^1"
"#;

    fn package_bytes(manifest: &str) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file(LIBRARY_MANIFEST, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(manifest.as_bytes()).unwrap();
            writer
                .start_file("src/index.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"export default 1;").unwrap();
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    fn rpx_package_bytes(index: Option<&str>) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file("package.rbe.toml", SimpleFileOptions::default())
                .unwrap();
            writer
                .write_all(b"[package]\nname='demo'\nversion='1.0.0'\n")
                .unwrap();
            if let Some(index) = index {
                writer
                    .start_file(RPX_PACKAGE_INDEX, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(index.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    fn locked(version: &str, bytes: &[u8]) -> LockedProjectPackage {
        LockedProjectPackage {
            version: version.into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: format!("{:x}", Sha256::digest(bytes)),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: None,
            sdk: None,
        }
    }

    fn write_cached_artifact(
        layout: &ProjectCacheLayout,
        locked: &LockedProjectPackage,
        bytes: &[u8],
    ) {
        let dir = layout
            .library_artifact_dir(&locked.artifact_sha256)
            .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artifact.rbe"), bytes).unwrap();
    }

    #[tokio::test]
    async fn complete_verified_partial_is_inspected_before_lock_creation() {
        let bytes = package_bytes(MANIFEST);
        let release = release(&bytes);
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        let plan = registry_artifact_plan(&layout, "advancenet", &release).unwrap();
        std::fs::create_dir_all(&plan.staging_dir).unwrap();
        std::fs::write(&plan.partial_path, &bytes).unwrap();

        let package = stage_registry_package(&layout, "advancenet", &release)
            .await
            .unwrap();

        assert_eq!(package.manifest.name, "advancenet");
        assert_eq!(
            package.manifest_sha256,
            format!("{:x}", Sha256::digest(MANIFEST.as_bytes()))
        );
        assert_eq!(package.locked.version, "4.0.1");
        assert_eq!(package.locked.manifest_sha256, package.manifest_sha256);
        assert_eq!(package.locked.runtime.as_ref().unwrap().kind, "bun");
        assert_eq!(package.locked.sdk.as_ref().unwrap().kind, "javascript");
    }

    #[tokio::test]
    async fn registry_and_manifest_identity_must_match() {
        let bytes = package_bytes(&MANIFEST.replace("4.0.1", "4.0.2"));
        let release = release(&bytes);
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        let plan = registry_artifact_plan(&layout, "advancenet", &release).unwrap();
        std::fs::create_dir_all(&plan.staging_dir).unwrap();
        std::fs::write(&plan.partial_path, &bytes).unwrap();

        let error = stage_registry_package(&layout, "advancenet", &release)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::PackageMetadataMismatch {
                field: "version",
                ..
            }
        ));
    }

    #[test]
    fn verified_rpx_index_reader_exposes_only_explicit_roots() {
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        let root_index = r#"{"format":1,"package":{"name":"advancenet","version":"2.0.0","language":"typescript"},"exports":[],"private_dependencies":{}}"#;
        let private_index = r#"{"format":1,"package":{"name":"secret-parser","version":"9.0.0","language":"rust"},"exports":[],"private_dependencies":{}}"#;
        let root_bytes = rpx_package_bytes(Some(root_index));
        let private_bytes = rpx_package_bytes(Some(private_index));
        let root_locked = locked("2.0.0", &root_bytes);
        let private_locked = locked("9.0.0", &private_bytes);
        write_cached_artifact(&layout, &root_locked, &root_bytes);
        write_cached_artifact(&layout, &private_locked, &private_bytes);

        let lock = ProjectPackageLock {
            format: 1,
            packages: BTreeMap::from([("advancenet".into(), root_locked)]),
            private: BTreeMap::from([(
                "advancenet".into(),
                BTreeMap::from([("secret-parser".into(), private_locked)]),
            )]),
        };
        std::fs::write(layout.lock_path(), lock.render_yaml().unwrap()).unwrap();

        let indexes = read_verified_rpx_root_indexes(temp.path()).unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].package, "advancenet");
        assert_eq!(indexes[0].version, "2.0.0");
        assert_eq!(indexes[0].index_json, root_index);
        assert!(!indexes[0].index_json.contains("secret-parser"));
    }

    #[test]
    fn legacy_root_without_rpx_index_is_not_exposed() {
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        let bytes = rpx_package_bytes(None);
        let root_locked = locked("1.0.0", &bytes);
        write_cached_artifact(&layout, &root_locked, &bytes);
        let lock = ProjectPackageLock {
            format: 1,
            packages: BTreeMap::from([("legacy".into(), root_locked)]),
            private: BTreeMap::new(),
        };
        std::fs::write(layout.lock_path(), lock.render_yaml().unwrap()).unwrap();

        assert!(read_verified_rpx_root_indexes(temp.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rpx_index_reader_rejects_cache_bytes_that_do_not_match_lock_hash() {
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        let bytes = rpx_package_bytes(Some("{}"));
        let root_locked = locked("1.0.0", &bytes);
        let dir = layout
            .library_artifact_dir(&root_locked.artifact_sha256)
            .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artifact.rbe"), b"corrupt").unwrap();
        let lock = ProjectPackageLock {
            format: 1,
            packages: BTreeMap::from([("demo".into(), root_locked)]),
            private: BTreeMap::new(),
        };
        std::fs::write(layout.lock_path(), lock.render_yaml().unwrap()).unwrap();

        assert!(matches!(
            read_verified_rpx_root_indexes(temp.path()),
            Err(InstallRuntimeError::ExistingArtifactMismatch { .. })
        ));
    }

    fn release(bytes: &[u8]) -> RegistryPackageRelease {
        RegistryPackageRelease {
            version: "4.0.1".into(),
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            yanked: false,
            dependencies: BTreeMap::from([("rbe-core".into(), "^1".into())]),
            artifact: RegistryArtifact {
                source: "https://example.com/advancenet.rbe".into(),
                sha256: format!("{:x}", Sha256::digest(bytes)),
                size_bytes: bytes.len() as u64,
                source_sha256: None,
                publisher: None,
                signature: None,
                reproducible_build: false,
                shipped_binary_sha256: None,
            },
        }
    }
}
