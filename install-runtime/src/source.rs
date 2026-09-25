use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use rbe_install_executor::{
    ExtractionPlan, SourceFileDigest, SourceFileHasher, SourceSelection, SourceTreeDigest,
    StreamingVerifier, VerifiedDownload,
};
use rbe_library_package::{inspect_zip, ArchivePolicy, LibraryManifest};
use zip::ZipArchive;

use crate::{ArtifactStage, InstallRuntimeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSource {
    pub root: PathBuf,
    pub manifest: LibraryManifest,
    pub source_digest: SourceTreeDigest,
    pub extracted_files: usize,
    pub total_uncompressed_bytes: u64,
}

/// Materialize one promoted package artifact into a fresh source root and
/// compute the deterministic source identity selected by trusted orchestration.
///
/// The promoted cache path is not authority: its bytes are re-hashed against
/// the verified artifact identity immediately before archive inspection and
/// extraction. The destination root must be absolute and absent. Archive
/// permissions/timestamps are never restored and package symlinks remain
/// forbidden by the shared archive/extraction contracts.
pub fn prepare_promoted_source(
    stage: &ArtifactStage,
    destination_root: &Path,
    selection: &SourceSelection,
) -> Result<PreparedSource, InstallRuntimeError> {
    let mut artifact = open_verified_artifact(&stage.promotion.final_artifact, &stage.verified)?;
    let policy = ArchivePolicy::default();
    let inspected = inspect_zip(&mut artifact, policy)?;
    let plan = ExtractionPlan::from_inspected(&inspected, destination_root)?;

    create_fresh_root(&plan)?;
    artifact.seek(SeekFrom::Start(0))?;
    let (source_digest, extracted_files, total_uncompressed_bytes) =
        materialize_archive(artifact, &plan, selection)?;
    validate_runtime_entry(&plan.root, &inspected.manifest)?;

    Ok(PreparedSource {
        root: plan.root,
        manifest: inspected.manifest,
        source_digest,
        extracted_files,
        total_uncompressed_bytes,
    })
}

fn open_verified_artifact(
    path: &Path,
    expected: &VerifiedDownload,
) -> Result<File, InstallRuntimeError> {
    if !path.is_absolute() {
        return Err(InstallRuntimeError::PreparedArtifactMustBeAbsolute(
            path.display().to_string(),
        ));
    }
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeCacheEntry(
            path.display().to_string(),
        ));
    }

    let maximum_bytes = expected.size_bytes.max(1);
    let mut verifier = StreamingVerifier::new(
        expected.sha256.clone(),
        Some(expected.size_bytes),
        maximum_bytes,
    )?;
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read])?;
    }
    verifier.finish()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn create_fresh_root(plan: &ExtractionPlan) -> Result<(), InstallRuntimeError> {
    if !plan.hardening.require_fresh_root
        || !plan.hardening.reject_existing_destinations
        || plan.hardening.follow_symlinks
        || plan.hardening.preserve_archive_permissions
        || plan.hardening.preserve_archive_timestamps
    {
        return Err(InstallRuntimeError::UnsafeExtractionPolicy);
    }

    ensure_no_symlink_components(&plan.root)?;
    match fs::symlink_metadata(&plan.root) {
        Ok(_) => {
            return Err(InstallRuntimeError::ExtractionRootExists(
                plan.root.display().to_string(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let parent = plan.root.parent().ok_or_else(|| {
        InstallRuntimeError::ExtractionParentInvalid(plan.root.display().to_string())
    })?;
    ensure_no_symlink_components(parent)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            InstallRuntimeError::ExtractionParentInvalid(parent.display().to_string())
        } else {
            InstallRuntimeError::Io(error)
        }
    })?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(InstallRuntimeError::ExtractionParentInvalid(
            parent.display().to_string(),
        ));
    }

    fs::create_dir(&plan.root)?;
    ensure_no_symlink_components(&plan.root)?;
    Ok(())
}

fn materialize_archive(
    artifact: File,
    plan: &ExtractionPlan,
    selection: &SourceSelection,
) -> Result<(SourceTreeDigest, usize, u64), InstallRuntimeError> {
    let mut archive = ZipArchive::new(artifact)?;
    if archive.len() != plan.entries.len() {
        return Err(InstallRuntimeError::ArchiveChangedDuringPreparation);
    }

    let mut created_directories = BTreeSet::from([plan.root.clone()]);
    let mut source_files = Vec::<SourceFileDigest>::new();
    let mut extracted_files = 0usize;
    let mut total_uncompressed_bytes = 0u64;

    for (index, planned) in plan.entries.iter().enumerate() {
        let mut entry = archive.by_index(index)?;
        let canonical = canonical_zip_name(entry.name())?;
        if canonical != planned.archive_path
            || entry.size() != planned.size
            || entry.is_dir() != planned.directory
        {
            return Err(InstallRuntimeError::ExtractionEntryMismatch {
                expected: planned.archive_path.clone(),
                actual: canonical,
            });
        }

        if planned.directory {
            if planned.size != 0 {
                return Err(InstallRuntimeError::NonEmptyDirectoryEntry(
                    planned.archive_path.clone(),
                ));
            }
            ensure_directory(
                &plan.root,
                &planned.destination,
                &mut created_directories,
            )?;
            continue;
        }

        let parent = planned.destination.parent().ok_or_else(|| {
            InstallRuntimeError::ExtractionEntryMismatch {
                expected: planned.archive_path.clone(),
                actual: planned.destination.display().to_string(),
            }
        })?;
        ensure_directory(&plan.root, parent, &mut created_directories)?;
        ensure_no_symlink_components(&planned.destination)?;

        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&planned.destination)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    InstallRuntimeError::ExtractionDestinationExists(
                        planned.destination.display().to_string(),
                    )
                } else {
                    InstallRuntimeError::Io(error)
                }
            })?;

        let mut selected_hasher = selection
            .includes(&planned.archive_path)
            .then(|| SourceFileHasher::new(planned.archive_path.clone(), planned.size))
            .transpose()?;
        let mut observed = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .ok_or(InstallRuntimeError::ArtifactSizeOverflow)?;
            if observed > planned.size {
                return Err(InstallRuntimeError::ExtractedEntrySizeMismatch {
                    path: planned.archive_path.clone(),
                    expected: planned.size,
                    actual: observed,
                });
            }
            output.write_all(&buffer[..read])?;
            if let Some(hasher) = &mut selected_hasher {
                hasher.update(&buffer[..read])?;
            }
        }
        if observed != planned.size {
            return Err(InstallRuntimeError::ExtractedEntrySizeMismatch {
                path: planned.archive_path.clone(),
                expected: planned.size,
                actual: observed,
            });
        }
        output.sync_all()?;

        if let Some(hasher) = selected_hasher {
            source_files.push(hasher.finish()?);
        }
        extracted_files = extracted_files
            .checked_add(1)
            .ok_or(InstallRuntimeError::ArtifactSizeOverflow)?;
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(observed)
            .ok_or(InstallRuntimeError::ArtifactSizeOverflow)?;
    }

    if total_uncompressed_bytes != plan.total_uncompressed_bytes {
        return Err(InstallRuntimeError::ExtractedArchiveSizeMismatch {
            expected: plan.total_uncompressed_bytes,
            actual: total_uncompressed_bytes,
        });
    }

    let source_digest = SourceTreeDigest::from_files(selection, source_files)?;
    Ok((source_digest, extracted_files, total_uncompressed_bytes))
}

fn ensure_directory(
    root: &Path,
    directory: &Path,
    created_directories: &mut BTreeSet<PathBuf>,
) -> Result<(), InstallRuntimeError> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        InstallRuntimeError::ExtractionDestinationEscaped(directory.display().to_string())
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(InstallRuntimeError::ExtractionDestinationEscaped(
                directory.display().to_string(),
            ));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && created_directories.contains(&current) => {}
            Ok(_) => {
                return Err(InstallRuntimeError::ExtractionDestinationExists(
                    current.display().to_string(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                created_directories.insert(current.clone());
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure_no_symlink_components(directory)?;
    Ok(())
}

fn validate_runtime_entry(
    root: &Path,
    manifest: &LibraryManifest,
) -> Result<(), InstallRuntimeError> {
    let mut entry = root.to_path_buf();
    for component in Path::new(&manifest.runtime.entry).components() {
        let Component::Normal(part) = component else {
            return Err(InstallRuntimeError::RuntimeEntryMissing(
                manifest.runtime.entry.clone(),
            ));
        };
        entry.push(part);
    }
    ensure_no_symlink_components(&entry)?;
    match fs::symlink_metadata(&entry) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        _ => Err(InstallRuntimeError::RuntimeEntryMissing(
            manifest.runtime.entry.clone(),
        )),
    }
}

fn canonical_zip_name(raw: &str) -> Result<String, InstallRuntimeError> {
    if raw.is_empty() || raw.contains('\\') || raw.starts_with('/') || raw.contains(':') {
        return Err(InstallRuntimeError::UnsafeArchiveEntry(raw.to_string()));
    }
    let directory = raw.ends_with('/');
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(InstallRuntimeError::UnsafeArchiveEntry(raw.to_string()));
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || matches!(part, "." | "..") {
            return Err(InstallRuntimeError::UnsafeArchiveEntry(raw.to_string()));
        }
        parts.push(part);
    }
    let mut canonical = parts.join("/");
    if directory {
        canonical.push('/');
    }
    Ok(canonical)
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
    use std::io::{Cursor, Write};

    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    use super::*;

    const MANIFEST: &str = r#"name = "advancenet"
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
"#;

    fn package_bytes() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file("library.toml", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(MANIFEST.as_bytes()).unwrap();
            writer
                .start_file("src/index.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"export default 1;").unwrap();
            writer
                .start_file("dist/prebuilt.bin", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"native-binary").unwrap();
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    fn promoted_stage(root: &Path, bytes: &[u8]) -> ArtifactStage {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let final_dir = root.join(".cache/library").join(&sha256);
        let final_artifact = final_dir.join("artifact.rbe");
        fs::create_dir_all(&final_dir).unwrap();
        fs::write(&final_artifact, bytes).unwrap();
        ArtifactStage {
            verified: VerifiedDownload {
                sha256,
                size_bytes: bytes.len() as u64,
            },
            promotion: PromotionPlan {
                verified_partial: root.join("unused.part"),
                final_dir,
                final_artifact,
                create_final_dir: true,
                replace_existing: false,
                fsync_before_publish: true,
                fsync_parent_after_publish: true,
            },
            resumed_from_bytes: 0,
        }
    }

    #[test]
    fn promoted_artifact_extracts_to_fresh_root_and_hashes_selected_source() {
        let temp = tempdir().unwrap();
        let bytes = package_bytes();
        let stage = promoted_stage(temp.path(), &bytes);
        let extraction_parent = temp.path().join("prepare");
        fs::create_dir(&extraction_parent).unwrap();
        let extraction_root = extraction_parent.join("advancenet");
        let selection = SourceSelection::new(["src"]).unwrap();

        let prepared =
            prepare_promoted_source(&stage, &extraction_root, &selection).unwrap();

        assert_eq!(prepared.root, extraction_root);
        assert_eq!(prepared.manifest.name, "advancenet");
        assert_eq!(prepared.source_digest.file_count, 1);
        assert_eq!(prepared.source_digest.total_bytes, 17);
        assert_eq!(prepared.extracted_files, 3);
        assert_eq!(fs::read(prepared.root.join("src/index.js")).unwrap(), b"export default 1;");
        assert_eq!(fs::read(prepared.root.join("dist/prebuilt.bin")).unwrap(), b"native-binary");
    }

    #[test]
    fn existing_extraction_root_is_rejected() {
        let temp = tempdir().unwrap();
        let bytes = package_bytes();
        let stage = promoted_stage(temp.path(), &bytes);
        let extraction_parent = temp.path().join("prepare");
        let extraction_root = extraction_parent.join("advancenet");
        fs::create_dir_all(&extraction_root).unwrap();
        let selection = SourceSelection::new(["src"]).unwrap();

        let error = prepare_promoted_source(&stage, &extraction_root, &selection).unwrap_err();
        assert!(matches!(error, InstallRuntimeError::ExtractionRootExists(_)));
    }

    #[test]
    fn promoted_cache_is_reverified_before_source_root_is_created() {
        let temp = tempdir().unwrap();
        let bytes = package_bytes();
        let stage = promoted_stage(temp.path(), &bytes);
        fs::write(&stage.promotion.final_artifact, b"corrupt").unwrap();
        let extraction_parent = temp.path().join("prepare");
        fs::create_dir(&extraction_parent).unwrap();
        let extraction_root = extraction_parent.join("advancenet");
        let selection = SourceSelection::new(["src"]).unwrap();

        assert!(prepare_promoted_source(&stage, &extraction_root, &selection).is_err());
        assert!(!extraction_root.exists());
    }
}
