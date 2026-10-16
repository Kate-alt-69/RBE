use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use rbe_install_executor::{ExtractionPlan, SourceFileDigest, SourceFileHasher};
use rbe_library_package::{inspect_zip, ArchivePolicy};
use rbe_project_package::ProjectCacheLayout;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::VerifiedRpxRootSnapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorkerSourcePlan {
    pub package: String,
    pub artifact_sha256: String,
    pub artifact_path: PathBuf,
    pub source_root: PathBuf,
    pub entrypoint: PathBuf,
    pub extraction: ExtractionPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedWorkerSource {
    pub package: String,
    pub artifact_sha256: String,
    pub source_root: PathBuf,
    pub entrypoint: PathBuf,
    pub files: Vec<SourceFileDigest>,
}

/// Build a worker execution-tree plan from the same verified root snapshot used
/// by REL linking and Library Host identity binding.
///
/// The archive is re-hashed and re-inspected while planning. Callers choose a
/// fresh absolute `source_root`; the package cannot influence that location.
pub fn prepare_verified_worker_source(
    project_root: impl AsRef<Path>,
    snapshot: &VerifiedRpxRootSnapshot,
    source_root: impl AsRef<Path>,
) -> Result<VerifiedWorkerSourcePlan, WorkerSourceError> {
    let project_root = project_root.as_ref();
    let source_root = source_root.as_ref();
    if !project_root.is_absolute() {
        return Err(WorkerSourceError::ProjectRootMustBeAbsolute);
    }
    if !source_root.is_absolute() {
        return Err(WorkerSourceError::SourceRootMustBeAbsolute);
    }

    let layout = ProjectCacheLayout::new(project_root);
    let artifact_path = layout
        .library_artifact_dir(&snapshot.artifact_sha256)?
        .join("artifact.rbe");
    verify_artifact(&artifact_path, &snapshot.artifact_sha256)?;

    let policy = ArchivePolicy::default();
    let inspected = inspect_zip(File::open(&artifact_path)?, policy)?;
    verify_snapshot_manifest(snapshot, &inspected.manifest)?;
    let extraction = ExtractionPlan::from_inspected(&inspected, source_root)?;
    require_safe_extraction(&extraction, source_root)?;

    let runtime_entry = &snapshot.worker.runtime_entry;
    let entry = extraction
        .entries
        .iter()
        .find(|entry| !entry.directory && entry.archive_path == *runtime_entry)
        .ok_or_else(|| WorkerSourceError::MissingRuntimeEntrypoint {
            package: snapshot.package.clone(),
            entry: runtime_entry.clone(),
        })?;

    Ok(VerifiedWorkerSourcePlan {
        package: snapshot.package.clone(),
        artifact_sha256: snapshot.artifact_sha256.to_ascii_lowercase(),
        artifact_path,
        source_root: source_root.to_path_buf(),
        entrypoint: entry.destination.clone(),
        extraction,
    })
}

impl VerifiedWorkerSourcePlan {
    /// Materialize a fresh worker tree from the pinned package archive.
    ///
    /// The archive hash, manifest identity, extraction plan, entry sizes, and
    /// destination policy are all rechecked immediately before/during writes.
    /// Existing roots are rejected instead of being reused as trusted state.
    pub fn materialize(
        &self,
        snapshot: &VerifiedRpxRootSnapshot,
    ) -> Result<MaterializedWorkerSource, WorkerSourceError> {
        if snapshot.package != self.package
            || !snapshot
                .artifact_sha256
                .eq_ignore_ascii_case(&self.artifact_sha256)
        {
            return Err(WorkerSourceError::SnapshotDrift(self.package.clone()));
        }

        verify_artifact(&self.artifact_path, &self.artifact_sha256)?;
        let policy = ArchivePolicy::default();
        let inspected = inspect_zip(File::open(&self.artifact_path)?, policy)?;
        verify_snapshot_manifest(snapshot, &inspected.manifest)?;
        let fresh = ExtractionPlan::from_inspected(&inspected, &self.source_root)?;
        if fresh != self.extraction {
            return Err(WorkerSourceError::ExtractionPlanDrift(self.package.clone()));
        }
        require_safe_extraction(&fresh, &self.source_root)?;
        require_safe_fresh_parent(&self.source_root)?;
        if std::fs::symlink_metadata(&self.source_root).is_ok() {
            return Err(WorkerSourceError::SourceRootAlreadyExists(
                self.source_root.clone(),
            ));
        }
        std::fs::create_dir(&self.source_root)?;

        let result = materialize_entries(self);
        if result.is_err() {
            cleanup_partial_root(&self.source_root);
        }
        let files = result?;

        let metadata = std::fs::symlink_metadata(&self.entrypoint).map_err(|_| {
            WorkerSourceError::MissingRuntimeEntrypoint {
                package: self.package.clone(),
                entry: snapshot.worker.runtime_entry.clone(),
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            cleanup_partial_root(&self.source_root);
            return Err(WorkerSourceError::UnsafeRuntimeEntrypoint(
                self.entrypoint.clone(),
            ));
        }

        Ok(MaterializedWorkerSource {
            package: self.package.clone(),
            artifact_sha256: self.artifact_sha256.clone(),
            source_root: self.source_root.clone(),
            entrypoint: self.entrypoint.clone(),
            files,
        })
    }
}

fn materialize_entries(
    plan: &VerifiedWorkerSourcePlan,
) -> Result<Vec<SourceFileDigest>, WorkerSourceError> {
    let mut archive = ZipArchive::new(File::open(&plan.artifact_path)?)?;
    let mut files = Vec::new();

    for entry in &plan.extraction.entries {
        let mut source = archive.by_name(&entry.archive_path)?;
        if source.is_dir() != entry.directory || source.size() != entry.size {
            return Err(WorkerSourceError::ArchiveEntryDrift {
                package: plan.package.clone(),
                path: entry.archive_path.clone(),
            });
        }
        if !entry.destination.starts_with(&plan.source_root) {
            return Err(WorkerSourceError::UnsafeDestination(
                entry.destination.clone(),
            ));
        }

        if entry.directory {
            ensure_parent_directories(&plan.source_root, &entry.destination)?;
            ensure_directory(&entry.destination)?;
            continue;
        }

        ensure_parent_directories(&plan.source_root, &entry.destination)?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&entry.destination)?;
        let mut hasher = SourceFileHasher::new(&entry.archive_path, entry.size)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read])?;
            destination.write_all(&buffer[..read])?;
        }
        destination.flush()?;
        files.push(hasher.finish()?);
    }

    Ok(files)
}

fn verify_snapshot_manifest(
    snapshot: &VerifiedRpxRootSnapshot,
    manifest: &rbe_library_package::LibraryManifest,
) -> Result<(), WorkerSourceError> {
    if manifest.name != snapshot.package
        || manifest.version != snapshot.version
        || manifest.runtime.kind != snapshot.worker.runtime_kind
        || manifest.runtime.entry != snapshot.worker.runtime_entry
        || manifest.runtime.managed != snapshot.worker.runtime_managed
    {
        return Err(WorkerSourceError::SnapshotDrift(snapshot.package.clone()));
    }
    Ok(())
}

fn verify_artifact(path: &Path, expected_sha256: &str) -> Result<(), WorkerSourceError> {
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkerSourceError::UnsafeArtifact(path.to_path_buf()));
    }

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
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(WorkerSourceError::ArtifactHashMismatch {
            package: path.display().to_string(),
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

fn require_safe_extraction(
    extraction: &ExtractionPlan,
    source_root: &Path,
) -> Result<(), WorkerSourceError> {
    let hardening = extraction.hardening;
    if extraction.root != source_root
        || !hardening.require_fresh_root
        || !hardening.reject_existing_destinations
        || hardening.follow_symlinks
        || hardening.preserve_archive_permissions
        || hardening.preserve_archive_timestamps
    {
        return Err(WorkerSourceError::UnsafeExtractionPolicy);
    }
    Ok(())
}

fn require_safe_fresh_parent(root: &Path) -> Result<(), WorkerSourceError> {
    let parent = root
        .parent()
        .ok_or_else(|| WorkerSourceError::UnsafeSourceParent(root.to_path_buf()))?;
    ensure_no_symlink_components(parent)?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|_| WorkerSourceError::UnsafeSourceParent(parent.to_path_buf()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkerSourceError::UnsafeSourceParent(parent.to_path_buf()));
    }
    Ok(())
}

fn ensure_parent_directories(root: &Path, destination: &Path) -> Result<(), WorkerSourceError> {
    let parent = destination
        .parent()
        .ok_or_else(|| WorkerSourceError::UnsafeDestination(destination.to_path_buf()))?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| WorkerSourceError::UnsafeDestination(destination.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(WorkerSourceError::UnsafeDestination(
                destination.to_path_buf(),
            ));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(WorkerSourceError::UnsafeDestination(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), WorkerSourceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(WorkerSourceError::UnsafeDestination(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), WorkerSourceError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorkerSourceError::SymlinkedPath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn cleanup_partial_root(root: &Path) {
    if let Ok(metadata) = std::fs::symlink_metadata(root) {
        if !metadata.file_type().is_symlink() && metadata.is_dir() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerSourceError {
    #[error(transparent)]
    ProjectPackage(#[from] rbe_project_package::ProjectPackageError),
    #[error(transparent)]
    Archive(#[from] rbe_library_package::ArchiveError),
    #[error(transparent)]
    Source(#[from] rbe_install_executor::SourceStageError),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error("worker project root must be absolute")]
    ProjectRootMustBeAbsolute,
    #[error("worker source root must be absolute")]
    SourceRootMustBeAbsolute,
    #[error("worker package snapshot drifted before materialization: {0:?}")]
    SnapshotDrift(String),
    #[error("worker package artifact is not a safe regular file: {0}")]
    UnsafeArtifact(PathBuf),
    #[error(
        "worker package artifact hash mismatch at {package}: expected {expected}, got {actual}"
    )]
    ArtifactHashMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    #[error("worker package {package:?} is missing runtime entrypoint {entry:?}")]
    MissingRuntimeEntrypoint { package: String, entry: String },
    #[error("worker extraction plan drifted before materialization for {0:?}")]
    ExtractionPlanDrift(String),
    #[error("worker extraction plan does not enforce the required hardening policy")]
    UnsafeExtractionPolicy,
    #[error("worker source parent is unavailable or unsafe: {0}")]
    UnsafeSourceParent(PathBuf),
    #[error("worker source root already exists: {0}")]
    SourceRootAlreadyExists(PathBuf),
    #[error("worker extraction destination is unsafe: {0}")]
    UnsafeDestination(PathBuf),
    #[error("worker runtime entrypoint is not a regular non-symlink file: {0}")]
    UnsafeRuntimeEntrypoint(PathBuf),
    #[error("worker package archive entry changed before extraction: package={package:?}, path={path:?}")]
    ArchiveEntryDrift { package: String, path: String },
    #[error("worker filesystem path traverses a symbolic link: {0}")]
    SymlinkedPath(PathBuf),
    #[error("worker source materialization I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rbe_project_package::ProjectCacheLayout;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::VerifiedPackageWorkerIdentity;

    const MANIFEST: &str = r#"
name = "advancenet"
version = "2.0.0"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 1

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

    fn stage_artifact(project_root: &Path, include_entry: bool) -> (String, PathBuf) {
        let temporary = project_root.join("artifact.tmp");
        let file = File::create(&temporary).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        archive.start_file("library.toml", options).unwrap();
        archive.write_all(MANIFEST.as_bytes()).unwrap();
        if include_entry {
            archive.start_file("src/index.js", options).unwrap();
            archive.write_all(b"export default {};\n").unwrap();
        }
        archive.finish().unwrap();

        let sha256 = format!("{:x}", Sha256::digest(std::fs::read(&temporary).unwrap()));
        let layout = ProjectCacheLayout::new(project_root);
        let artifact_dir = layout.library_artifact_dir(&sha256).unwrap();
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let artifact = artifact_dir.join("artifact.rbe");
        std::fs::rename(temporary, &artifact).unwrap();
        (sha256, artifact)
    }

    fn snapshot(sha256: &str) -> VerifiedRpxRootSnapshot {
        VerifiedRpxRootSnapshot {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: sha256.into(),
            index_json: "{}".into(),
            worker: VerifiedPackageWorkerIdentity {
                package: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: sha256.into(),
                rbe_abi_min: 1,
                rbe_abi_max: 1,
                sdk_language: "bun".into(),
                sdk_name: "@rbe/sdk".into(),
                sdk_version: "0.1.9".into(),
                runtime_kind: "bun".into(),
                runtime_version: "1.3.7".into(),
                runtime_entry: "src/index.js".into(),
                runtime_managed: true,
            },
        }
    }

    #[test]
    fn materializes_verified_archive_into_fresh_worker_tree() {
        let project = tempfile::tempdir().unwrap();
        let (sha256, _) = stage_artifact(project.path(), true);
        let snapshot = snapshot(&sha256);
        let parent = project.path().join(".cache/rbe/library-host");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("session-a");

        let plan = prepare_verified_worker_source(project.path(), &snapshot, &root).unwrap();
        let materialized = plan.materialize(&snapshot).unwrap();
        assert_eq!(materialized.entrypoint, root.join("src/index.js"));
        assert_eq!(
            std::fs::read_to_string(&materialized.entrypoint).unwrap(),
            "export default {};\n"
        );
        assert!(materialized
            .files
            .iter()
            .any(|file| file.path == "src/index.js"));
    }

    #[test]
    fn missing_runtime_entry_is_rejected_before_materialization() {
        let project = tempfile::tempdir().unwrap();
        let (sha256, _) = stage_artifact(project.path(), false);
        let snapshot = snapshot(&sha256);
        let parent = project.path().join(".cache/rbe/library-host");
        std::fs::create_dir_all(&parent).unwrap();
        let error =
            prepare_verified_worker_source(project.path(), &snapshot, parent.join("session-b"))
                .unwrap_err();
        assert!(matches!(
            error,
            WorkerSourceError::MissingRuntimeEntrypoint { .. }
        ));
    }

    #[test]
    fn artifact_change_after_plan_is_rejected_and_source_root_stays_absent() {
        let project = tempfile::tempdir().unwrap();
        let (sha256, artifact) = stage_artifact(project.path(), true);
        let snapshot = snapshot(&sha256);
        let parent = project.path().join(".cache/rbe/library-host");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("session-c");
        let plan = prepare_verified_worker_source(project.path(), &snapshot, &root).unwrap();

        std::fs::write(&artifact, b"tampered").unwrap();
        let error = plan.materialize(&snapshot).unwrap_err();
        assert!(matches!(
            error,
            WorkerSourceError::ArtifactHashMismatch { .. }
        ));
        assert!(!root.exists());
    }

    #[test]
    fn existing_worker_root_is_never_reused_as_trusted_state() {
        let project = tempfile::tempdir().unwrap();
        let (sha256, _) = stage_artifact(project.path(), true);
        let snapshot = snapshot(&sha256);
        let parent = project.path().join(".cache/rbe/library-host");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("session-d");
        let plan = prepare_verified_worker_source(project.path(), &snapshot, &root).unwrap();
        std::fs::create_dir(&root).unwrap();

        let error = plan.materialize(&snapshot).unwrap_err();
        assert!(matches!(
            error,
            WorkerSourceError::SourceRootAlreadyExists(_)
        ));
    }
}
