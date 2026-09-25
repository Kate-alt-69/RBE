use std::fs::File;
use std::io::Read;
use std::path::Path;

use rbe_install_executor::{ArtifactDownloadPlan, DownloadLimits, ResumePolicy};
use rbe_install_request::{validate_registry_package_name, RegistryPackageRelease};
use rbe_library_package::{inspect_zip, ArchivePolicy, LibraryManifest, LIBRARY_MANIFEST};
use rbe_project_package::{LockedProjectPackage, LockedToolchain, ProjectCacheLayout};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::{stage_artifact, ArtifactStage, InstallRuntimeError};

#[derive(Debug)]
pub struct VerifiedRegistryPackage {
    pub stage: ArtifactStage,
    pub manifest: LibraryManifest,
    pub manifest_sha256: String,
    pub locked: LockedProjectPackage,
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
    let manifest_sha256 = hash_manifest(
        &stage.promotion.verified_partial,
        policy.max_manifest_bytes,
    )?;

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
        assert_eq!(
            package.locked.sdk.as_ref().unwrap().kind,
            "javascript"
        );
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
}
