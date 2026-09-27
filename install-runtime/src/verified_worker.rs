use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rbe_library_package::{inspect_zip, ArchivePolicy, LibraryManifest, LIBRARY_MANIFEST};
use rbe_library_resolver::version_satisfies_requirement;
use rbe_project_package::{LockedProjectPackage, ProjectCacheLayout, ProjectPackageLock};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::InstallRuntimeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPackageWorkerIdentity {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub rbe_abi_min: u32,
    pub rbe_abi_max: u32,
    pub sdk_language: String,
    pub sdk_name: String,
    pub sdk_version: String,
    pub runtime_kind: String,
    pub runtime_version: String,
    pub runtime_entry: String,
    pub runtime_managed: bool,
}

/// Reconstruct the executable identity of every explicit project root from the
/// SHA-pinned artifact and the resolved project lock.
///
/// The artifact contributes immutable package/ABI/SDK/runtime declarations;
/// the lock contributes exact resolved SDK/runtime versions. Both sides are
/// checked against each other before any identity can be handed to Library Host.
pub fn read_verified_root_worker_identities(
    project_root: &Path,
) -> Result<BTreeMap<String, VerifiedPackageWorkerIdentity>, InstallRuntimeError> {
    let layout = ProjectCacheLayout::new(project_root);
    let lock_path = layout.lock_path();
    if !lock_path.try_exists()? {
        return Ok(BTreeMap::new());
    }

    let lock = ProjectPackageLock::parse_yaml(&std::fs::read_to_string(&lock_path)?)?;
    let mut identities = BTreeMap::new();
    for (package, locked) in &lock.packages {
        let artifact_dir = layout.library_artifact_dir(&locked.artifact_sha256)?;
        let artifact_path = artifact_dir.join("artifact.rbe");
        verify_locked_artifact(&artifact_path, &locked.artifact_sha256)?;

        let policy = ArchivePolicy::default();
        let inspected = inspect_zip(File::open(&artifact_path)?, policy)?;
        let manifest_sha256 = hash_manifest(&artifact_path, policy.max_manifest_bytes)?;
        let identity = identity_from_manifest(
            package,
            locked,
            &inspected.manifest,
            &manifest_sha256,
        )?;
        if identities.insert(package.clone(), identity).is_some() {
            return Err(InstallRuntimeError::DuplicateVerifiedPackage {
                root: package.clone(),
                package: package.clone(),
            });
        }
    }
    Ok(identities)
}

fn identity_from_manifest(
    package: &str,
    locked: &LockedProjectPackage,
    manifest: &LibraryManifest,
    manifest_sha256: &str,
) -> Result<VerifiedPackageWorkerIdentity, InstallRuntimeError> {
    require_verified_match(package, "package name", package, &manifest.name)?;
    require_verified_match(package, "package version", &locked.version, &manifest.version)?;
    require_verified_match(
        package,
        "manifest SHA-256",
        &locked.manifest_sha256.to_ascii_lowercase(),
        &manifest_sha256.to_ascii_lowercase(),
    )?;
    if locked.dependencies != manifest.dependencies {
        return Err(InstallRuntimeError::VerifiedPackageMetadataMismatch {
            package: package.to_string(),
            field: "dependencies",
            locked: format!("{:?}", locked.dependencies),
            artifact: format!("{:?}", manifest.dependencies),
        });
    }

    let runtime = locked
        .runtime
        .as_ref()
        .ok_or_else(|| InstallRuntimeError::MissingLockedToolchain {
            package: package.to_string(),
            toolchain: "runtime",
        })?;
    let sdk = locked
        .sdk
        .as_ref()
        .ok_or_else(|| InstallRuntimeError::MissingLockedToolchain {
            package: package.to_string(),
            toolchain: "SDK",
        })?;

    require_verified_match(
        package,
        "runtime kind",
        &runtime.kind,
        &manifest.runtime.kind,
    )?;
    require_verified_match(package, "SDK family", &sdk.kind, &manifest.sdk.family)?;

    if !version_satisfies_requirement(&manifest.runtime.version, &runtime.version)? {
        return Err(InstallRuntimeError::ToolchainRequirementMismatch {
            package: package.to_string(),
            toolchain: "runtime",
            resolved: runtime.version.clone(),
            requirement: manifest.runtime.version.clone(),
        });
    }
    if !version_satisfies_requirement(&manifest.sdk.version, &sdk.version)? {
        return Err(InstallRuntimeError::ToolchainRequirementMismatch {
            package: package.to_string(),
            toolchain: "SDK",
            resolved: sdk.version.clone(),
            requirement: manifest.sdk.version.clone(),
        });
    }

    Ok(VerifiedPackageWorkerIdentity {
        package: package.to_string(),
        version: locked.version.clone(),
        artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
        rbe_abi_min: manifest.rbe_abi_min,
        rbe_abi_max: manifest.rbe_abi_max,
        // Protocol v1 uses the concrete runtime lane here (bun/node/rust/python)
        // while sdk_name remains the language package identity.
        sdk_language: runtime.kind.clone(),
        sdk_name: manifest.sdk.package.clone(),
        sdk_version: sdk.version.clone(),
        runtime_kind: runtime.kind.clone(),
        runtime_version: runtime.version.clone(),
        runtime_entry: manifest.runtime.entry.clone(),
        runtime_managed: manifest.runtime.managed,
    })
}

fn require_verified_match(
    package: &str,
    field: &'static str,
    locked: &str,
    artifact: &str,
) -> Result<(), InstallRuntimeError> {
    if locked != artifact {
        return Err(InstallRuntimeError::VerifiedPackageMetadataMismatch {
            package: package.to_string(),
            field,
            locked: locked.to_string(),
            artifact: artifact.to_string(),
        });
    }
    Ok(())
}

fn verify_locked_artifact(
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

    use rbe_project_package::LockedToolchain;

    use super::*;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "2.0.0"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 2

[sdk]
family = "javascript"
package = "@rbe/sdk"
version = "^0.1"

[runtime]
kind = "bun"
version = "^1.3"
managed = true
entry = "src/index.js"
"#;

    fn locked() -> LockedProjectPackage {
        LockedProjectPackage {
            version: "2.0.0".into(),
            resolved_from: "registry:advancenet".into(),
            artifact_url: "https://example.invalid/advancenet.rbe".into(),
            artifact_sha256: "a".repeat(64),
            manifest_sha256: format!("{:x}", Sha256::digest(MANIFEST.as_bytes())),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: Some(LockedToolchain {
                kind: "bun".into(),
                version: "1.3.7".into(),
            }),
            sdk: Some(LockedToolchain {
                kind: "javascript".into(),
                version: "0.1.9".into(),
            }),
        }
    }

    #[test]
    fn verified_worker_identity_keeps_exact_resolved_toolchains() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let locked = locked();
        let identity = identity_from_manifest(
            "advancenet",
            &locked,
            &manifest,
            &locked.manifest_sha256,
        )
        .unwrap();
        assert_eq!(identity.sdk_language, "bun");
        assert_eq!(identity.sdk_name, "@rbe/sdk");
        assert_eq!(identity.sdk_version, "0.1.9");
        assert_eq!(identity.runtime_kind, "bun");
        assert_eq!(identity.runtime_version, "1.3.7");
        assert_eq!(identity.runtime_entry, "src/index.js");
        assert_eq!((identity.rbe_abi_min, identity.rbe_abi_max), (1, 2));
    }

    #[test]
    fn resolved_toolchain_outside_manifest_requirement_is_rejected() {
        let manifest = LibraryManifest::parse(MANIFEST).unwrap();
        let mut locked = locked();
        locked.runtime.as_mut().unwrap().version = "2.0.0".into();
        assert!(matches!(
            identity_from_manifest(
                "advancenet",
                &locked,
                &manifest,
                &locked.manifest_sha256
            ),
            Err(InstallRuntimeError::ToolchainRequirementMismatch {
                toolchain: "runtime",
                ..
            })
        ));
    }
}
