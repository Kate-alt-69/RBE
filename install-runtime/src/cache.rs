use std::fs;
use std::path::{Path, PathBuf};

use rbe_install_executor::ArtifactDownloadPlan;

use crate::artifact::{stage_artifact as stage_network_artifact, ArtifactStage};
use crate::InstallRuntimeError;

/// Prefer an already-promoted content-addressed artifact, but never trust its
/// path alone. Existing final bytes are streamed through the same pinned
/// verifier as a network download before they are allowed to seed staging.
pub async fn stage_artifact_cached(
    plan: &ArtifactDownloadPlan,
) -> Result<ArtifactStage, InstallRuntimeError> {
    match fs::symlink_metadata(&plan.final_artifact_path) {
        Ok(metadata) => {
            ensure_no_symlink_components(&plan.final_artifact_path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(InstallRuntimeError::UnsafeCacheEntry(
                    plan.final_artifact_path.display().to_string(),
                ));
            }
            verify_cached_final(plan).map_err(|_| InstallRuntimeError::ExistingArtifactMismatch {
                path: plan.final_artifact_path.display().to_string(),
                expected_sha256: plan.expected_sha256.clone(),
            })?;
            seed_staging_from_cache(plan)?;
            stage_network_artifact(plan).await
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            stage_network_artifact(plan).await
        }
        Err(error) => Err(error.into()),
    }
}

fn verify_cached_final(plan: &ArtifactDownloadPlan) -> Result<(), InstallRuntimeError> {
    let mut verifier = plan.verifier()?;
    let mut file = fs::File::open(&plan.final_artifact_path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        use std::io::Read;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read])?;
    }
    verifier.finish()?;
    Ok(())
}

fn seed_staging_from_cache(plan: &ArtifactDownloadPlan) -> Result<(), InstallRuntimeError> {
    ensure_safe_directory(&plan.staging_dir)?;
    match fs::symlink_metadata(&plan.partial_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(InstallRuntimeError::UnsafeStagingEntry(
                    plan.partial_path.display().to_string(),
                ));
            }
            // Another attempt already owns reusable staged bytes. Let the
            // ordinary verifier/resume path decide whether they are complete.
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    match fs::hard_link(&plan.final_artifact_path, &plan.partial_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_safe_directory(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    fs::create_dir_all(path)?;
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InstallRuntimeError::UnsafeStagingEntry(
            path.display().to_string(),
        ));
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), InstallRuntimeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
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

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use url::Url;

    use super::*;
    use rbe_install_executor::{DownloadLimits, ResumePolicy};

    fn plan(root: &Path) -> ArtifactDownloadPlan {
        let sha256 = format!("{:x}", Sha256::digest(b"abc"));
        let staging_dir = root.join(".cache/library/.staging").join(&sha256);
        let final_dir = root.join(".cache/library").join(&sha256);
        ArtifactDownloadPlan {
            package: "demo".into(),
            version: "1.0.0".into(),
            source: Url::parse("https://example.invalid/demo.rbe").unwrap(),
            expected_sha256: sha256,
            expected_size_bytes: Some(3),
            partial_path: staging_dir.join("artifact.rbe.part"),
            staging_dir,
            final_artifact_path: final_dir.join("artifact.rbe"),
            final_dir,
            limits: DownloadLimits::default(),
            resume: ResumePolicy::default(),
        }
    }

    #[tokio::test]
    async fn verified_final_cache_seeds_complete_staging_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan(temp.path());
        fs::create_dir_all(&plan.final_dir).unwrap();
        fs::write(&plan.final_artifact_path, b"abc").unwrap();

        let stage = stage_artifact_cached(&plan).await.unwrap();
        assert_eq!(stage.verified.sha256, plan.expected_sha256);
        assert_eq!(stage.verified.size_bytes, 3);
        assert_eq!(stage.resumed_from_bytes, 3);
        assert_eq!(fs::read(&plan.partial_path).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn corrupt_final_cache_is_rejected_without_network_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan(temp.path());
        fs::create_dir_all(&plan.final_dir).unwrap();
        fs::write(&plan.final_artifact_path, b"xyz").unwrap();

        let error = stage_artifact_cached(&plan).await.unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::ExistingArtifactMismatch { .. }
        ));
        assert!(!plan.partial_path.exists());
    }
}
