use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rbe_install_executor::VerifiedDownload;
use sha2::{Digest, Sha256};

use crate::{ArtifactStage, InstallRuntimeError, VerifiedRootGraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPromotionState {
    Published,
    ReusedExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPromotionResult {
    pub final_artifact: PathBuf,
    pub state: ArtifactPromotionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootGraphPromotion {
    pub root: String,
    pub published: usize,
    pub reused_existing: usize,
    pub artifacts: Vec<(String, ArtifactPromotionResult)>,
}

/// Publish one already-verified staged artifact into its content-addressed cache.
///
/// Publication is deliberately no-clobber. A matching cache winner is reused
/// only after its bytes are independently re-hashed. A conflicting winner is
/// never replaced. The staging and final paths live under the same project
/// cache, so a hard-link gives us atomic name creation without exposing a
/// partially copied cache artifact.
pub fn promote_artifact(
    stage: &ArtifactStage,
) -> Result<ArtifactPromotionResult, InstallRuntimeError> {
    let plan = &stage.promotion;
    validate_promotion_shape(plan.verified_partial.as_path(), &plan.final_dir, &plan.final_artifact)?;
    verify_regular_file(&plan.verified_partial, &stage.verified)?;

    ensure_safe_directory(&plan.final_dir)?;
    if let Some(result) = reuse_existing(stage)? {
        return Ok(result);
    }

    if plan.fsync_before_publish {
        sync_regular_file(&plan.verified_partial)?;
    }

    match fs::hard_link(&plan.verified_partial, &plan.final_artifact) {
        Ok(()) => {
            if let Err(error) = verify_regular_file(&plan.final_artifact, &stage.verified) {
                let _ = fs::remove_file(&plan.final_artifact);
                return Err(error);
            }
            sync_regular_file(&plan.final_artifact)?;
            if plan.fsync_parent_after_publish {
                sync_directory_if_supported(&plan.final_dir)?;
            }
            fs::remove_file(&plan.verified_partial)?;
            if let Some(staging_parent) = plan.verified_partial.parent() {
                sync_directory_if_supported(staging_parent)?;
            }
            Ok(ArtifactPromotionResult {
                final_artifact: plan.final_artifact.clone(),
                state: ArtifactPromotionState::Published,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            reuse_existing(stage)?.ok_or_else(|| InstallRuntimeError::PromotionRace {
                path: plan.final_artifact.display().to_string(),
            })
        }
        Err(error) => Err(error.into()),
    }
}

pub fn promote_verified_graph(
    graph: &VerifiedRootGraph,
) -> Result<RootGraphPromotion, InstallRuntimeError> {
    let mut published = 0usize;
    let mut reused_existing = 0usize;
    let mut artifacts = Vec::with_capacity(graph.install_order.len());

    for package in &graph.install_order {
        let verified = graph
            .packages
            .get(package)
            .ok_or_else(|| InstallRuntimeError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package.clone(),
            })?;
        let result = promote_artifact(&verified.stage)?;
        match result.state {
            ArtifactPromotionState::Published => published += 1,
            ArtifactPromotionState::ReusedExisting => reused_existing += 1,
        }
        artifacts.push((package.clone(), result));
    }

    Ok(RootGraphPromotion {
        root: graph.root.clone(),
        published,
        reused_existing,
        artifacts,
    })
}

fn reuse_existing(
    stage: &ArtifactStage,
) -> Result<Option<ArtifactPromotionResult>, InstallRuntimeError> {
    let plan = &stage.promotion;
    match fs::symlink_metadata(&plan.final_artifact) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(InstallRuntimeError::UnsafeCacheEntry(
                    plan.final_artifact.display().to_string(),
                ));
            }
            verify_regular_file(&plan.final_artifact, &stage.verified).map_err(|_| {
                InstallRuntimeError::ExistingArtifactMismatch {
                    path: plan.final_artifact.display().to_string(),
                    expected_sha256: stage.verified.sha256.clone(),
                }
            })?;
            if plan.verified_partial != plan.final_artifact {
                match fs::remove_file(&plan.verified_partial) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some(ArtifactPromotionResult {
                final_artifact: plan.final_artifact.clone(),
                state: ArtifactPromotionState::ReusedExisting,
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_promotion_shape(
    verified_partial: &Path,
    final_dir: &Path,
    final_artifact: &Path,
) -> Result<(), InstallRuntimeError> {
    if verified_partial.as_os_str().is_empty()
        || final_dir.as_os_str().is_empty()
        || final_artifact.as_os_str().is_empty()
        || final_artifact.parent() != Some(final_dir)
        || verified_partial == final_artifact
    {
        return Err(InstallRuntimeError::InvalidPromotionPlan);
    }
    ensure_no_symlink_components(verified_partial)?;
    ensure_no_symlink_components(final_dir)?;
    ensure_no_symlink_components(final_artifact)?;
    Ok(())
}

fn verify_regular_file(
    path: &Path,
    expected: &VerifiedDownload,
) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeCacheEntry(
            path.display().to_string(),
        ));
    }
    if metadata.len() != expected.size_bytes {
        return Err(InstallRuntimeError::PromotedArtifactVerification {
            path: path.display().to_string(),
            expected_sha256: expected.sha256.clone(),
        });
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut observed = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read as u64)
            .ok_or(InstallRuntimeError::ArtifactSizeOverflow)?;
        if observed > expected.size_bytes {
            return Err(InstallRuntimeError::PromotedArtifactVerification {
                path: path.display().to_string(),
                expected_sha256: expected.sha256.clone(),
            });
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if observed != expected.size_bytes || actual != expected.sha256 {
        return Err(InstallRuntimeError::PromotedArtifactVerification {
            path: path.display().to_string(),
            expected_sha256: expected.sha256.clone(),
        });
    }
    Ok(())
}

fn sync_regular_file(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.sync_all()?;
    Ok(())
}

fn ensure_safe_directory(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    fs::create_dir_all(path)?;
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InstallRuntimeError::UnsafeCacheEntry(
            path.display().to_string(),
        ));
    }
    if let Some(parent) = path.parent() {
        sync_directory_if_supported(parent)?;
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

#[cfg(unix)]
fn sync_directory_if_supported(path: &Path) -> Result<(), InstallRuntimeError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory_if_supported(_path: &Path) -> Result<(), InstallRuntimeError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use rbe_install_executor::{ArtifactDownloadPlan, DownloadLimits, ResumePolicy};
    use url::Url;

    use super::*;
    use crate::stage_artifact;

    fn plan(root: &Path) -> ArtifactDownloadPlan {
        let sha256 = format!("{:x}", Sha256::digest(b"abc"));
        let staging_dir = root.join(".cache/library/.staging").join(&sha256);
        let final_dir = root.join(".cache/library").join(&sha256);
        ArtifactDownloadPlan {
            package: "demo".into(),
            version: "1.0.0".into(),
            source: Url::parse("https://example.com/demo.rbe").unwrap(),
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

    fn staged(root: &Path) -> ArtifactStage {
        let plan = plan(root);
        fs::create_dir_all(&plan.staging_dir).unwrap();
        fs::write(&plan.partial_path, b"abc").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(stage_artifact(&plan)).unwrap()
    }

    #[test]
    fn verified_partial_is_published_without_clobber() {
        let temp = tempfile::tempdir().unwrap();
        let stage = staged(temp.path());
        let partial = stage.promotion.verified_partial.clone();
        let result = promote_artifact(&stage).unwrap();

        assert_eq!(result.state, ArtifactPromotionState::Published);
        assert_eq!(fs::read(&result.final_artifact).unwrap(), b"abc");
        assert!(!partial.exists());
    }

    #[test]
    fn matching_existing_cache_winner_is_reused() {
        let temp = tempfile::tempdir().unwrap();
        let stage = staged(temp.path());
        fs::create_dir_all(&stage.promotion.final_dir).unwrap();
        fs::write(&stage.promotion.final_artifact, b"abc").unwrap();

        let result = promote_artifact(&stage).unwrap();
        assert_eq!(result.state, ArtifactPromotionState::ReusedExisting);
        assert_eq!(fs::read(&result.final_artifact).unwrap(), b"abc");
        assert!(!stage.promotion.verified_partial.exists());
    }

    #[test]
    fn conflicting_existing_cache_winner_is_never_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let stage = staged(temp.path());
        fs::create_dir_all(&stage.promotion.final_dir).unwrap();
        fs::write(&stage.promotion.final_artifact, b"xyz").unwrap();

        let error = promote_artifact(&stage).unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::ExistingArtifactMismatch { .. }
        ));
        assert_eq!(fs::read(&stage.promotion.final_artifact).unwrap(), b"xyz");
        assert!(stage.promotion.verified_partial.exists());
    }
}
