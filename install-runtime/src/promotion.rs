use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use rbe_install_executor::VerifiedDownload;
use sha2::{Digest, Sha256};

use crate::{ArtifactStage, InstallRuntimeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionDisposition {
    Published,
    ReusedVerifiedExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedArtifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub disposition: PromotionDisposition,
}

/// Publish one already-staged artifact into its content-addressed cache path.
///
/// The staged bytes are re-hashed immediately before publication. Existing
/// cache entries are never trusted by path: a matching winner is re-verified
/// and reused, while a conflicting entry is rejected unless the executor plan
/// explicitly allowed replacement. Publication itself is a same-filesystem
/// rename from the staging tree into the final cache tree.
pub fn promote_verified_artifact(
    stage: &ArtifactStage,
) -> Result<PromotedArtifact, InstallRuntimeError> {
    validate_plan(stage)?;
    verify_file(&stage.promotion.verified_partial, &stage.verified)?;

    if stage.promotion.create_final_dir {
        ensure_safe_directory(&stage.promotion.final_dir)?;
    } else {
        ensure_existing_safe_directory(&stage.promotion.final_dir)?;
    }
    ensure_no_symlink_components(&stage.promotion.final_artifact)?;

    if let Some(existing) = existing_regular_file(&stage.promotion.final_artifact)? {
        match verify_file(&stage.promotion.final_artifact, &stage.verified) {
            Ok(()) => {
                remove_verified_partial(&stage.promotion.verified_partial)?;
                return Ok(PromotedArtifact {
                    path: stage.promotion.final_artifact.clone(),
                    sha256: stage.verified.sha256.clone(),
                    size_bytes: existing,
                    disposition: PromotionDisposition::ReusedVerifiedExisting,
                });
            }
            Err(error) if !stage.promotion.replace_existing => {
                return Err(InstallRuntimeError::ExistingArtifactConflict {
                    path: stage.promotion.final_artifact.display().to_string(),
                    reason: error.to_string(),
                });
            }
            Err(_) => {}
        }
    }

    if stage.promotion.fsync_before_publish {
        File::open(&stage.promotion.verified_partial)?.sync_all()?;
    }

    match fs::rename(
        &stage.promotion.verified_partial,
        &stage.promotion.final_artifact,
    ) {
        Ok(()) => {}
        Err(error) if !stage.promotion.replace_existing && target_appeared(&error) => {
            verify_file(&stage.promotion.final_artifact, &stage.verified).map_err(|winner_error| {
                InstallRuntimeError::ExistingArtifactConflict {
                    path: stage.promotion.final_artifact.display().to_string(),
                    reason: winner_error.to_string(),
                }
            })?;
            remove_verified_partial_if_present(&stage.promotion.verified_partial)?;
            return Ok(PromotedArtifact {
                path: stage.promotion.final_artifact.clone(),
                sha256: stage.verified.sha256.clone(),
                size_bytes: stage.verified.size_bytes,
                disposition: PromotionDisposition::ReusedVerifiedExisting,
            });
        }
        Err(error) => return Err(error.into()),
    }

    if stage.promotion.fsync_parent_after_publish {
        sync_published_namespace(&stage.promotion.final_artifact)?;
    }
    verify_file(&stage.promotion.final_artifact, &stage.verified)?;

    Ok(PromotedArtifact {
        path: stage.promotion.final_artifact.clone(),
        sha256: stage.verified.sha256.clone(),
        size_bytes: stage.verified.size_bytes,
        disposition: PromotionDisposition::Published,
    })
}

fn validate_plan(stage: &ArtifactStage) -> Result<(), InstallRuntimeError> {
    let plan = &stage.promotion;
    if plan.verified_partial.as_os_str().is_empty()
        || plan.final_dir.as_os_str().is_empty()
        || plan.final_artifact.as_os_str().is_empty()
        || plan.verified_partial == plan.final_artifact
        || plan.final_artifact.parent() != Some(plan.final_dir.as_path())
    {
        return Err(InstallRuntimeError::InvalidPromotionPlan);
    }
    ensure_no_symlink_components(&plan.verified_partial)?;
    let metadata = fs::symlink_metadata(&plan.verified_partial)?;
    if metadata.file_type().is_symlink() {
        return Err(InstallRuntimeError::SymlinkedPath(
            plan.verified_partial.display().to_string(),
        ));
    }
    if !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeStagingEntry(
            plan.verified_partial.display().to_string(),
        ));
    }
    Ok(())
}

fn verify_file(path: &Path, expected: &VerifiedDownload) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(InstallRuntimeError::SymlinkedPath(path.display().to_string()));
    }
    if !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeStagingEntry(
            path.display().to_string(),
        ));
    }
    if metadata.len() != expected.size_bytes {
        return Err(InstallRuntimeError::PromotedArtifactSizeMismatch {
            path: path.display().to_string(),
            expected: expected.size_bytes,
            actual: metadata.len(),
        });
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read as u64)
            .ok_or(InstallRuntimeError::PromotedArtifactSizeOverflow)?;
        if observed > expected.size_bytes {
            return Err(InstallRuntimeError::PromotedArtifactSizeMismatch {
                path: path.display().to_string(),
                expected: expected.size_bytes,
                actual: observed,
            });
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected.sha256 {
        return Err(InstallRuntimeError::PromotedArtifactHashMismatch {
            path: path.display().to_string(),
            expected: expected.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

fn existing_regular_file(path: &Path) -> Result<Option<u64>, InstallRuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(InstallRuntimeError::SymlinkedPath(
            path.display().to_string(),
        )),
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(InstallRuntimeError::UnsafeStagingEntry(
            path.display().to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ensure_safe_directory(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    fs::create_dir_all(path)?;
    ensure_existing_safe_directory(path)
}

fn ensure_existing_safe_directory(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(InstallRuntimeError::SymlinkedPath(path.display().to_string()));
    }
    if !metadata.is_dir() {
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

fn remove_verified_partial(path: &Path) -> Result<(), InstallRuntimeError> {
    fs::remove_file(path)?;
    sync_removed_namespace(path)
}

fn remove_verified_partial_if_present(path: &Path) -> Result<(), InstallRuntimeError> {
    match fs::remove_file(path) {
        Ok(()) => sync_removed_namespace(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn target_appeared(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(unix)]
fn sync_published_namespace(path: &Path) -> Result<(), InstallRuntimeError> {
    let parent = path.parent().ok_or(InstallRuntimeError::InvalidPromotionPlan)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_published_namespace(path: &Path) -> Result<(), InstallRuntimeError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_removed_namespace(path: &Path) -> Result<(), InstallRuntimeError> {
    let parent = path.parent().ok_or(InstallRuntimeError::InvalidPromotionPlan)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_removed_namespace(_path: &Path) -> Result<(), InstallRuntimeError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use tempfile::tempdir;

    use super::*;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn stage(root: &Path, bytes: &[u8]) -> ArtifactStage {
        let staging_dir = root.join(".cache/library/.staging").join(ABC_SHA256);
        let final_dir = root.join(".cache/library").join(ABC_SHA256);
        fs::create_dir_all(&staging_dir).unwrap();
        let partial = staging_dir.join("artifact.rbe.part");
        fs::write(&partial, bytes).unwrap();
        ArtifactStage {
            verified: VerifiedDownload {
                sha256: ABC_SHA256.into(),
                size_bytes: 3,
            },
            promotion: PromotionPlan {
                verified_partial: partial,
                final_artifact: final_dir.join("artifact.rbe"),
                final_dir,
                create_final_dir: true,
                replace_existing: false,
                fsync_before_publish: true,
                fsync_parent_after_publish: true,
            },
            resumed_from_bytes: 3,
        }
    }

    #[test]
    fn verified_partial_is_published_to_content_addressed_cache() {
        let temp = tempdir().unwrap();
        let staged = stage(temp.path(), b"abc");
        let promoted = promote_verified_artifact(&staged).unwrap();

        assert_eq!(promoted.disposition, PromotionDisposition::Published);
        assert_eq!(fs::read(&promoted.path).unwrap(), b"abc");
        assert!(!staged.promotion.verified_partial.exists());
    }

    #[test]
    fn matching_existing_cache_entry_is_reverified_and_reused() {
        let temp = tempdir().unwrap();
        let staged = stage(temp.path(), b"abc");
        fs::create_dir_all(&staged.promotion.final_dir).unwrap();
        fs::write(&staged.promotion.final_artifact, b"abc").unwrap();

        let promoted = promote_verified_artifact(&staged).unwrap();

        assert_eq!(
            promoted.disposition,
            PromotionDisposition::ReusedVerifiedExisting
        );
        assert!(!staged.promotion.verified_partial.exists());
        assert_eq!(fs::read(&promoted.path).unwrap(), b"abc");
    }

    #[test]
    fn conflicting_existing_cache_entry_is_not_trusted_or_replaced() {
        let temp = tempdir().unwrap();
        let staged = stage(temp.path(), b"abc");
        fs::create_dir_all(&staged.promotion.final_dir).unwrap();
        fs::write(&staged.promotion.final_artifact, b"bad").unwrap();

        let error = promote_verified_artifact(&staged).unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::ExistingArtifactConflict { .. }
        ));
        assert_eq!(fs::read(&staged.promotion.verified_partial).unwrap(), b"abc");
        assert_eq!(fs::read(&staged.promotion.final_artifact).unwrap(), b"bad");
    }

    #[test]
    fn staged_bytes_are_reverified_immediately_before_publish() {
        let temp = tempdir().unwrap();
        let staged = stage(temp.path(), b"abc");
        fs::write(&staged.promotion.verified_partial, b"xyz").unwrap();

        let error = promote_verified_artifact(&staged).unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::PromotedArtifactHashMismatch { .. }
        ));
        assert!(!staged.promotion.final_artifact.exists());
    }
}
