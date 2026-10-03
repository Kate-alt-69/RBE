//! Bounded trusted ZIP operations for REL workspace/archive capabilities.
//!
//! REL never receives a raw host path. Backend resolves `$$/` / `??/` to
//! already-authorized absolute paths, then this module performs the archive I/O
//! with traversal, symlink, entry-count and byte limits. Other archive formats
//! remain fail-closed until a trusted decoder is wired here; no host `tar`/shell
//! fallback is permitted.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use atomic_io::AtomicIo;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const MAX_WORKSPACE_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_ENTRIES: usize = 4_096;
pub const MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_READ_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceArchiveEntry {
    pub path: String,
    pub directory: bool,
    pub size: u64,
    pub compressed_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceArchiveRead {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceArchiveWrite {
    pub archive_bytes: u64,
    pub entries: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceArchiveError {
    #[error("workspace archive path is not a regular non-symlink file: {0}")]
    InvalidArchive(String),
    #[error("workspace archive source is not a regular file or directory: {0}")]
    InvalidSource(String),
    #[error("workspace archive path traverses a symbolic link: {0}")]
    SymlinkedPath(String),
    #[error("workspace archive contains unsafe entry path {0:?}")]
    UnsafeEntry(String),
    #[error("workspace archive contains duplicate/case-colliding entry {0:?}")]
    DuplicateEntry(String),
    #[error("workspace archive contains a symlink or special entry {0:?}")]
    SpecialEntry(String),
    #[error("workspace archive exceeds {maximum} bytes")]
    ArchiveTooLarge { maximum: usize },
    #[error("workspace archive exceeds {maximum} entries")]
    TooManyEntries { maximum: usize },
    #[error("workspace archive entry {path:?} exceeds {maximum} bytes")]
    EntryTooLarge { path: String, maximum: u64 },
    #[error("workspace archive expanded bytes exceed {maximum}")]
    ExpandedTooLarge { maximum: u64 },
    #[error("workspace archive read of {path:?} exceeds REL read limit {maximum}")]
    ReadTooLarge { path: String, maximum: u64 },
    #[error("workspace archive entry {0:?} does not exist")]
    MissingEntry(String),
    #[error("workspace archive replacement entry {0:?} is a directory")]
    ReplaceDirectory(String),
    #[error("workspace archive output exceeds {maximum} bytes")]
    OutputTooLarge { maximum: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub fn list_zip(path: &Path) -> Result<Vec<WorkspaceArchiveEntry>, WorkspaceArchiveError> {
    let bytes = read_archive(path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    inspect_archive(&mut archive)
}

pub fn read_zip_entry(
    path: &Path,
    entry: &str,
) -> Result<WorkspaceArchiveRead, WorkspaceArchiveError> {
    let wanted = normalize_entry_path(entry, false)?;
    let bytes = read_archive(path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let _ = inspect_archive(&mut archive)?;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let normalized = normalize_entry_path(file.name(), file.is_dir())?;
        if normalized != wanted {
            continue;
        }
        if file.is_dir() {
            return Err(WorkspaceArchiveError::MissingEntry(wanted));
        }
        if file.size() > MAX_WORKSPACE_ARCHIVE_READ_BYTES {
            return Err(WorkspaceArchiveError::ReadTooLarge {
                path: wanted,
                maximum: MAX_WORKSPACE_ARCHIVE_READ_BYTES,
            });
        }
        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data)?;
        if data.len() as u64 != file.size() {
            return Err(WorkspaceArchiveError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ZIP entry size changed while reading",
            )));
        }
        return Ok(WorkspaceArchiveRead {
            path: normalized,
            size: data.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&data)),
            bytes: data,
        });
    }
    Err(WorkspaceArchiveError::MissingEntry(wanted))
}

pub fn extract_zip(
    path: &Path,
    entries: &[String],
    destination: &Path,
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    let requested = entries
        .iter()
        .map(|entry| normalize_entry_path(entry, false))
        .collect::<Result<BTreeSet<_>, _>>()?;
    prepare_destination_directory(destination)?;

    let bytes = read_archive(path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let metadata = inspect_archive(&mut archive)?;
    let known = metadata
        .iter()
        .filter(|entry| !entry.directory)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    for entry in &requested {
        if !known.contains(entry) {
            return Err(WorkspaceArchiveError::MissingEntry(entry.clone()));
        }
    }

    let mut written = 0_u64;
    let mut extracted = 0_usize;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = normalize_entry_path(file.name(), file.is_dir())?;
        if !requested.is_empty() && !requested.contains(&name) {
            continue;
        }
        let target = safe_join(destination, &name)?;
        if file.is_dir() {
            create_directory_tree(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            create_directory_tree(parent)?;
        }
        ensure_missing_or_regular_file(&target)?;
        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data)?;
        written = written
            .checked_add(data.len() as u64)
            .ok_or(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            })?;
        if written > MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES {
            return Err(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            });
        }
        AtomicIo::new().write_atomic(&target, &data)?;
        extracted += 1;
    }
    Ok(WorkspaceArchiveWrite {
        archive_bytes: written,
        entries: extracted,
    })
}

pub fn create_zip(
    source: &Path,
    destination: &Path,
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    let source_metadata = fs::symlink_metadata(source).map_err(|_| {
        WorkspaceArchiveError::InvalidSource(source.display().to_string())
    })?;
    ensure_no_symlink_components(source)?;
    if source_metadata.file_type().is_symlink()
        || (!source_metadata.is_file() && !source_metadata.is_dir())
    {
        return Err(WorkspaceArchiveError::InvalidSource(
            source.display().to_string(),
        ));
    }

    let mut files = Vec::<SourceEntry>::new();
    if source_metadata.is_file() {
        let name = source
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| WorkspaceArchiveError::UnsafeEntry(source.display().to_string()))?;
        files.push(SourceEntry {
            path: normalize_entry_path(name, false)?,
            source: source.to_path_buf(),
            directory: false,
            size: source_metadata.len(),
        });
    } else {
        collect_source_entries(source, source, &mut files)?;
    }
    write_new_zip(destination, &files)
}

pub fn replace_zip_entry(
    archive_path: &Path,
    entry: &str,
    source: &Path,
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    let wanted = normalize_entry_path(entry, false)?;
    let source_metadata = fs::symlink_metadata(source).map_err(|_| {
        WorkspaceArchiveError::InvalidSource(source.display().to_string())
    })?;
    ensure_no_symlink_components(source)?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(WorkspaceArchiveError::InvalidSource(
            source.display().to_string(),
        ));
    }
    if source_metadata.len() > MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES {
        return Err(WorkspaceArchiveError::EntryTooLarge {
            path: wanted.clone(),
            maximum: MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES,
        });
    }

    rewrite_zip(archive_path, Some((&wanted, source)), None)
}

pub fn remove_zip_entry(
    archive_path: &Path,
    entry: &str,
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    let wanted = normalize_entry_path(entry, false)?;
    rewrite_zip(archive_path, None, Some(&wanted))
}

#[derive(Debug)]
struct SourceEntry {
    path: String,
    source: PathBuf,
    directory: bool,
    size: u64,
}

fn read_archive(path: &Path) -> Result<Vec<u8>, WorkspaceArchiveError> {
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| WorkspaceArchiveError::InvalidArchive(path.display().to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkspaceArchiveError::InvalidArchive(
            path.display().to_string(),
        ));
    }
    if metadata.len() > MAX_WORKSPACE_ARCHIVE_BYTES as u64 {
        return Err(WorkspaceArchiveError::ArchiveTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    let bytes = AtomicIo::new().read(path)?;
    if bytes.len() > MAX_WORKSPACE_ARCHIVE_BYTES {
        return Err(WorkspaceArchiveError::ArchiveTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    Ok(bytes)
}

fn inspect_archive<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<Vec<WorkspaceArchiveEntry>, WorkspaceArchiveError> {
    if archive.len() > MAX_WORKSPACE_ARCHIVE_ENTRIES {
        return Err(WorkspaceArchiveError::TooManyEntries {
            maximum: MAX_WORKSPACE_ARCHIVE_ENTRIES,
        });
    }
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = normalize_entry_path(file.name(), file.is_dir())?;
        reject_special_entry(&file, &name)?;
        let collision = name.to_ascii_lowercase();
        if !seen.insert(collision) {
            return Err(WorkspaceArchiveError::DuplicateEntry(name));
        }
        if file.size() > MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES {
            return Err(WorkspaceArchiveError::EntryTooLarge {
                path: name,
                maximum: MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES,
            });
        }
        total = total.checked_add(file.size()).ok_or(
            WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            },
        )?;
        if total > MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES {
            return Err(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            });
        }
        entries.push(WorkspaceArchiveEntry {
            path: name,
            directory: file.is_dir(),
            size: file.size(),
            compressed_size: file.compressed_size(),
        });
    }
    Ok(entries)
}

fn reject_special_entry<R: Read>(
    file: &zip::read::ZipFile<'_, R>,
    name: &str,
) -> Result<(), WorkspaceArchiveError> {
    if let Some(mode) = file.unix_mode() {
        let kind = mode & 0o170000;
        if kind != 0 && kind != 0o100000 && kind != 0o040000 {
            return Err(WorkspaceArchiveError::SpecialEntry(name.to_string()));
        }
    }
    Ok(())
}

fn collect_source_entries(
    root: &Path,
    directory: &Path,
    output: &mut Vec<SourceEntry>,
) -> Result<(), WorkspaceArchiveError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if output.len() >= MAX_WORKSPACE_ARCHIVE_ENTRIES {
            return Err(WorkspaceArchiveError::TooManyEntries {
                maximum: MAX_WORKSPACE_ARCHIVE_ENTRIES,
            });
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceArchiveError::SymlinkedPath(
                path.display().to_string(),
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| WorkspaceArchiveError::UnsafeEntry(path.display().to_string()))?;
        let name = relative_path_string(relative)?;
        if metadata.is_dir() {
            output.push(SourceEntry {
                path: format!("{name}/"),
                source: path.clone(),
                directory: true,
                size: 0,
            });
            collect_source_entries(root, &path, output)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES {
                return Err(WorkspaceArchiveError::EntryTooLarge {
                    path: name,
                    maximum: MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES,
                });
            }
            output.push(SourceEntry {
                path: name,
                source: path,
                directory: false,
                size: metadata.len(),
            });
        } else {
            return Err(WorkspaceArchiveError::InvalidSource(
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn write_new_zip(
    destination: &Path,
    entries: &[SourceEntry],
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    if entries.len() > MAX_WORKSPACE_ARCHIVE_ENTRIES {
        return Err(WorkspaceArchiveError::TooManyEntries {
            maximum: MAX_WORKSPACE_ARCHIVE_ENTRIES,
        });
    }
    let mut total = 0_u64;
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for entry in entries {
        if entry.directory {
            writer.add_directory(&entry.path, options)?;
            continue;
        }
        total = total.checked_add(entry.size).ok_or(
            WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            },
        )?;
        if total > MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES {
            return Err(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            });
        }
        writer.start_file(&entry.path, options)?;
        let mut file = fs::File::open(&entry.source)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
            if writer.get_ref().get_ref().len() > MAX_WORKSPACE_ARCHIVE_BYTES {
                return Err(WorkspaceArchiveError::OutputTooLarge {
                    maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
                });
            }
        }
    }
    let bytes = writer.finish()?.into_inner();
    if bytes.len() > MAX_WORKSPACE_ARCHIVE_BYTES {
        return Err(WorkspaceArchiveError::OutputTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    prepare_output_parent(destination)?;
    AtomicIo::new().write_atomic(destination, &bytes)?;
    Ok(WorkspaceArchiveWrite {
        archive_bytes: bytes.len() as u64,
        entries: entries.len(),
    })
}

fn rewrite_zip(
    archive_path: &Path,
    replacement: Option<(&str, &Path)>,
    remove: Option<&str>,
) -> Result<WorkspaceArchiveWrite, WorkspaceArchiveError> {
    let bytes = read_archive(archive_path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let _ = inspect_archive(&mut archive)?;
    let mut output = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(&mut output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut found = false;
    let mut count = 0_usize;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = normalize_entry_path(file.name(), file.is_dir())?;
        if remove.is_some_and(|wanted| wanted == name) {
            found = true;
            continue;
        }
        if let Some((wanted, source)) = replacement {
            if wanted == name {
                if file.is_dir() {
                    return Err(WorkspaceArchiveError::ReplaceDirectory(name));
                }
                found = true;
                writer.start_file(wanted, options)?;
                let mut source_file = fs::File::open(source)?;
                std::io::copy(&mut source_file, &mut writer)?;
                count += 1;
                continue;
            }
        }
        if file.is_dir() {
            writer.add_directory(&name, options)?;
        } else {
            writer.start_file(&name, options)?;
            std::io::copy(&mut file, &mut writer)?;
        }
        count += 1;
        if output.get_ref().len() > MAX_WORKSPACE_ARCHIVE_BYTES {
            return Err(WorkspaceArchiveError::OutputTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
            });
        }
    }

    if !found {
        let wanted = replacement.map(|value| value.0).or(remove).unwrap_or_default();
        return Err(WorkspaceArchiveError::MissingEntry(wanted.to_string()));
    }
    writer.finish()?;
    let bytes = output.into_inner();
    if bytes.len() > MAX_WORKSPACE_ARCHIVE_BYTES {
        return Err(WorkspaceArchiveError::OutputTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    AtomicIo::new().write_atomic(archive_path, &bytes)?;
    Ok(WorkspaceArchiveWrite {
        archive_bytes: bytes.len() as u64,
        entries: count,
    })
}

fn normalize_entry_path(value: &str, directory: bool) -> Result<String, WorkspaceArchiveError> {
    let normalized = value.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\0')
        || trimmed.contains(':')
        || trimmed
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(WorkspaceArchiveError::UnsafeEntry(value.to_string()));
    }
    if directory {
        Ok(format!("{trimmed}/"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn relative_path_string(path: &Path) -> Result<String, WorkspaceArchiveError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(WorkspaceArchiveError::UnsafeEntry(path.display().to_string()));
        };
        let value = value
            .to_str()
            .ok_or_else(|| WorkspaceArchiveError::UnsafeEntry(path.display().to_string()))?;
        parts.push(value);
    }
    normalize_entry_path(&parts.join("/"), false)
}

fn safe_join(root: &Path, entry: &str) -> Result<PathBuf, WorkspaceArchiveError> {
    let normalized = normalize_entry_path(entry, entry.ends_with('/'))?;
    let mut path = root.to_path_buf();
    for segment in normalized.trim_end_matches('/').split('/') {
        path.push(segment);
    }
    if !path.starts_with(root) {
        return Err(WorkspaceArchiveError::UnsafeEntry(entry.to_string()));
    }
    Ok(path)
}

fn prepare_destination_directory(path: &Path) -> Result<(), WorkspaceArchiveError> {
    ensure_no_symlink_components_allow_missing(path)?;
    create_directory_tree(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceArchiveError::InvalidSource(path.display().to_string()));
    }
    Ok(())
}

fn prepare_output_parent(path: &Path) -> Result<(), WorkspaceArchiveError> {
    ensure_no_symlink_components_allow_missing(path)?;
    if let Some(parent) = path.parent() {
        create_directory_tree(parent)?;
    }
    ensure_missing_or_regular_file(path)
}

fn create_directory_tree(path: &Path) -> Result<(), WorkspaceArchiveError> {
    ensure_no_symlink_components_allow_missing(path)?;
    fs::create_dir_all(path)?;
    ensure_no_symlink_components(path)?;
    Ok(())
}

fn ensure_missing_or_regular_file(path: &Path) -> Result<(), WorkspaceArchiveError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            WorkspaceArchiveError::InvalidSource(path.display().to_string()),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), WorkspaceArchiveError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceArchiveError::SymlinkedPath(
                current.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn ensure_no_symlink_components_allow_missing(
    path: &Path,
) -> Result<(), WorkspaceArchiveError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorkspaceArchiveError::SymlinkedPath(
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
    use super::*;

    #[test]
    fn rejects_zip_traversal_and_drive_paths() {
        assert!(normalize_entry_path("../secret", false).is_err());
        assert!(normalize_entry_path("folder/../../secret", false).is_err());
        assert!(normalize_entry_path("C:/secret", false).is_err());
        assert_eq!(
            normalize_entry_path("folder/file.txt", false).unwrap(),
            "folder/file.txt"
        );
    }

    #[test]
    fn zip_create_list_read_extract_replace_remove_round_trip() {
        let temp = crate::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("hello.txt"), b"hello").unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/world.txt"), b"world").unwrap();
        let archive = temp.path().join("bundle.zip");

        create_zip(&source, &archive).unwrap();
        let listed = list_zip(&archive).unwrap();
        assert!(listed.iter().any(|entry| entry.path == "hello.txt"));
        assert!(listed.iter().any(|entry| entry.path == "nested/world.txt"));

        let read = read_zip_entry(&archive, "hello.txt").unwrap();
        assert_eq!(read.bytes, b"hello");

        let extracted = temp.path().join("extracted");
        extract_zip(&archive, &["nested/world.txt".into()], &extracted).unwrap();
        assert_eq!(fs::read(extracted.join("nested/world.txt")).unwrap(), b"world");

        let replacement = temp.path().join("replacement.txt");
        fs::write(&replacement, b"updated").unwrap();
        replace_zip_entry(&archive, "hello.txt", &replacement).unwrap();
        assert_eq!(read_zip_entry(&archive, "hello.txt").unwrap().bytes, b"updated");

        remove_zip_entry(&archive, "nested/world.txt").unwrap();
        assert!(matches!(
            read_zip_entry(&archive, "nested/world.txt"),
            Err(WorkspaceArchiveError::MissingEntry(_))
        ));
    }
}
