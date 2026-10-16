//! Bounded trusted ZIP operations for REL workspace/archive capabilities.
//!
//! REL never receives a raw host path. Backend resolves `$$/` / `??/` to
//! already-authorized absolute paths, then this module performs the archive I/O
//! with traversal, symlink, entry-count and byte limits. Other archive formats
//! remain fail-closed until a trusted decoder is wired here; no host `tar`/shell
//! fallback is permitted.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use atomic_io::AtomicIo;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

// These limits are for request-time work in Backend, not bulk publisher jobs.
pub const MAX_WORKSPACE_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_ENTRIES: usize = 4_096;
pub const MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_READ_BYTES: u64 = 1024 * 1024;
pub const MAX_WORKSPACE_ARCHIVE_COMPRESSION_RATIO: u64 = 200;
const MAX_ENTRY_PATH_BYTES: usize = 1024;
const MAX_ENTRY_DEPTH: usize = 64;

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
    #[error("workspace archive entry {0:?} exceeds the compression-ratio limit")]
    CompressionRatio(String),
    #[error("workspace archive source changed while reading")]
    ChangedSource,
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
        let data = read_entry_bounded(&mut file, MAX_WORKSPACE_ARCHIVE_READ_BYTES)?;
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

    prepare_destination_directory(destination)?;

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
        let data = read_entry_bounded(&mut file, MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES)?;
        written = written.checked_add(data.len() as u64).ok_or(
            WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            },
        )?;
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
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|_| WorkspaceArchiveError::InvalidSource(source.display().to_string()))?;
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
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|_| WorkspaceArchiveError::InvalidSource(source.display().to_string()))?;
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
    let bytes = AtomicIo::new().read_bounded(path, MAX_WORKSPACE_ARCHIVE_BYTES)?;
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
        if std::str::from_utf8(file.name_raw()).is_err() {
            return Err(WorkspaceArchiveError::UnsafeEntry(
                "entry name is not valid UTF-8".into(),
            ));
        }
        let name = normalize_entry_path(file.name(), file.is_dir())?;
        reject_special_entry(&file, &name)?;
        let collision = collision_key(name.trim_end_matches('/'));
        if !seen.insert(collision) {
            return Err(WorkspaceArchiveError::DuplicateEntry(name));
        }
        if file.size() > MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES {
            return Err(WorkspaceArchiveError::EntryTooLarge {
                path: name,
                maximum: MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES,
            });
        }
        if file.size()
            > file
                .compressed_size()
                .max(1)
                .saturating_mul(MAX_WORKSPACE_ARCHIVE_COMPRESSION_RATIO)
        {
            return Err(WorkspaceArchiveError::CompressionRatio(name));
        }
        total = total
            .checked_add(file.size())
            .ok_or(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            })?;
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
    let files = entries
        .iter()
        .filter(|entry| !entry.directory)
        .map(|entry| collision_key(&entry.path))
        .collect::<BTreeSet<_>>();
    for entry in &entries {
        let mut prefix = String::new();
        let parts = entry
            .path
            .trim_end_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&collision_key(part));
            if files.contains(&prefix) {
                return Err(WorkspaceArchiveError::DuplicateEntry(entry.path.clone()));
            }
        }
    }
    Ok(entries)
}

fn reject_special_entry(
    file: &zip::read::ZipFile<'_>,
    name: &str,
) -> Result<(), WorkspaceArchiveError> {
    if file.encrypted()
        || !matches!(
            file.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        )
    {
        return Err(WorkspaceArchiveError::InvalidArchive(
            "encrypted or unsupported ZIP entry".into(),
        ));
    }
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
    let mut entries = fs::read_dir(directory)?
        .take(MAX_WORKSPACE_ARCHIVE_ENTRIES + 1)
        .collect::<Result<Vec<_>, _>>()?;
    if entries.len() > MAX_WORKSPACE_ARCHIVE_ENTRIES {
        return Err(WorkspaceArchiveError::TooManyEntries {
            maximum: MAX_WORKSPACE_ARCHIVE_ENTRIES,
        });
    }
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
    let mut writer = ZipWriter::new(BoundedOutput::default());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for entry in entries {
        if entry.directory {
            writer.add_directory(&entry.path, options)?;
            continue;
        }
        total = total
            .checked_add(entry.size)
            .ok_or(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            })?;
        if total > MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES {
            return Err(WorkspaceArchiveError::ExpandedTooLarge {
                maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
            });
        }
        writer.start_file(&entry.path, options)?;
        ensure_no_symlink_components(&entry.source)?;
        let mut file = fs::File::open(&entry.source)?;
        copy_exact_bounded(&mut file, &mut writer, entry.size)?;
    }
    let bytes = writer.finish()?.into_inner();
    if bytes.len() > MAX_WORKSPACE_ARCHIVE_BYTES {
        return Err(WorkspaceArchiveError::OutputTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    inspect_archive(&mut ZipArchive::new(Cursor::new(&bytes))?)?;
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
    let mut writer = ZipWriter::new(BoundedOutput::default());
    let mut total = 0_u64;
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
                let size = source_file.metadata()?.len();
                add_expanded(&mut total, size)?;
                copy_exact_bounded(&mut source_file, &mut writer, size)?;
                count += 1;
                continue;
            }
        }
        if file.is_dir() {
            writer.add_directory(&name, options)?;
        } else {
            writer.start_file(&name, options)?;
            let size = file.size();
            add_expanded(&mut total, size)?;
            copy_exact_bounded(&mut file, &mut writer, size)?;
        }
        count += 1;
    }

    if !found {
        let wanted = replacement
            .map(|value| value.0)
            .or(remove)
            .unwrap_or_default();
        return Err(WorkspaceArchiveError::MissingEntry(wanted.to_string()));
    }
    let bytes = writer.finish()?.into_inner();
    if bytes.len() > MAX_WORKSPACE_ARCHIVE_BYTES {
        return Err(WorkspaceArchiveError::OutputTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_BYTES,
        });
    }
    inspect_archive(&mut ZipArchive::new(Cursor::new(&bytes))?)?;
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
        || trimmed.len() > MAX_ENTRY_PATH_BYTES
        || trimmed.split('/').count() > MAX_ENTRY_DEPTH
        || trimmed.starts_with('/')
        || trimmed.contains('\0')
        || trimmed.contains(':')
        || trimmed.split('/').any(|segment| {
            segment.is_empty() || matches!(segment, "." | "..") || unsafe_windows_component(segment)
        })
    {
        return Err(WorkspaceArchiveError::UnsafeEntry(value.to_string()));
    }
    if directory {
        Ok(format!("{trimmed}/"))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Compare canonical spellings and case without rewriting the visible entry
/// name. This rejects aliases on case/normalization-insensitive filesystems.
fn collision_key(path: &str) -> String {
    path.nfc()
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .nfc()
        .collect()
}

fn unsafe_windows_component(segment: &str) -> bool {
    if segment.ends_with(['.', ' ']) {
        return true;
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device_suffix = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    let short_alias = stem.rsplit_once('~').is_some_and(|(prefix, digits)| {
        !prefix.is_empty()
            && stem.len() <= 8
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
    });
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device_suffix.is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
        || short_alias
}

/// Limit actual decoded bytes rather than trusting central-directory claims.
fn read_entry_bounded(
    file: &mut zip::read::ZipFile<'_>,
    limit: u64,
) -> Result<Vec<u8>, WorkspaceArchiveError> {
    let size = file.size();
    if size > limit {
        return Err(WorkspaceArchiveError::ReadTooLarge {
            path: file.name().into(),
            maximum: limit,
        });
    }
    let mut bytes = Vec::new();
    file.take(size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(WorkspaceArchiveError::ChangedSource);
    }
    Ok(bytes)
}
fn copy_exact_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    size: u64,
) -> Result<(), WorkspaceArchiveError> {
    if size > MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES {
        return Err(WorkspaceArchiveError::ExpandedTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES,
        });
    }
    let copied = io::copy(&mut reader.take(size + 1), writer)?;
    if copied != size {
        return Err(WorkspaceArchiveError::ChangedSource);
    }
    Ok(())
}
fn add_expanded(total: &mut u64, size: u64) -> Result<(), WorkspaceArchiveError> {
    *total = total
        .checked_add(size)
        .filter(|v| *v <= MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES)
        .ok_or(WorkspaceArchiveError::ExpandedTooLarge {
            maximum: MAX_WORKSPACE_ARCHIVE_UNCOMPRESSED_BYTES,
        })?;
    Ok(())
}

/// Enforce the compressed-output limit during every encoder write/seek, not
/// after Vec has already grown. AtomicIo only receives a completed safe ZIP.
#[derive(Default)]
struct BoundedOutput(Cursor<Vec<u8>>);
impl BoundedOutput {
    fn into_inner(self) -> Vec<u8> {
        self.0.into_inner()
    }
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.position().saturating_add(bytes.len() as u64) > MAX_WORKSPACE_ARCHIVE_BYTES as u64
        {
            return Err(io::Error::other("ZIP output exceeds its byte limit"));
        }
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Seek for BoundedOutput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let previous = self.0.position();
        let next = self.0.seek(position)?;
        if next > MAX_WORKSPACE_ARCHIVE_BYTES as u64 {
            self.0.set_position(previous);
            return Err(io::Error::other("ZIP output seek exceeds its byte limit"));
        }
        Ok(next)
    }
}

fn relative_path_string(path: &Path) -> Result<String, WorkspaceArchiveError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(WorkspaceArchiveError::UnsafeEntry(
                path.display().to_string(),
            ));
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
        return Err(WorkspaceArchiveError::InvalidSource(
            path.display().to_string(),
        ));
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
        // A Windows volume prefix is not an inspectable root until the
        // following RootDir component has supplied its trailing separator.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceArchiveError::SymlinkedPath(
                current.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn ensure_no_symlink_components_allow_missing(path: &Path) -> Result<(), WorkspaceArchiveError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
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
        assert_eq!(
            fs::read(extracted.join("nested/world.txt")).unwrap(),
            b"world"
        );

        let replacement = temp.path().join("replacement.txt");
        fs::write(&replacement, b"updated").unwrap();
        replace_zip_entry(&archive, "hello.txt", &replacement).unwrap();
        assert_eq!(
            read_zip_entry(&archive, "hello.txt").unwrap().bytes,
            b"updated"
        );

        remove_zip_entry(&archive, "nested/world.txt").unwrap();
        assert!(matches!(
            read_zip_entry(&archive, "nested/world.txt"),
            Err(WorkspaceArchiveError::MissingEntry(_))
        ));
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    fn raw_zip(path: &Path, names: &[&str], data: &[u8], method: CompressionMethod) {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for name in names {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
            writer.write_all(data).unwrap();
        }
        fs::write(path, writer.finish().unwrap().into_inner()).unwrap();
    }
    #[test]
    fn unsafe_names_and_file_directory_aliases_fail_before_extraction() {
        let temp = crate::tempdir().unwrap();
        let archive = temp.path().join("unsafe.zip");
        let destination = temp.path().join("output");
        for names in [
            vec!["../outside"],
            vec!["C:/outside"],
            vec!["/outside"],
            vec![".. /outside"],
            vec!["CON.txt"],
            vec!["COM¹.txt"],
            vec!["LONGFI~1.TXT"],
            vec!["file", "FILE"],
            vec!["é.txt", "e\u{301}.txt"],
            vec!["σ.txt", "ς.txt"],
            vec!["file", "file/nested"],
            vec!["file/nested", "file"],
            vec!["directory/", "DIRECTORY"],
        ] {
            raw_zip(&archive, &names, b"data", CompressionMethod::Stored);
            assert!(list_zip(&archive).is_err(), "accepted {names:?}");
            assert!(extract_zip(&archive, &[], &destination).is_err());
            assert!(!destination.exists());
        }
    }
    #[test]
    fn high_ratio_and_too_many_entries_are_rejected() {
        let temp = crate::tempdir().unwrap();
        let archive = temp.path().join("bomb.zip");
        raw_zip(
            &archive,
            &["zeros"],
            &vec![0; 256 * 1024],
            CompressionMethod::Deflated,
        );
        assert!(matches!(
            list_zip(&archive),
            Err(WorkspaceArchiveError::CompressionRatio(_))
        ));
        let names = (0..=MAX_WORKSPACE_ARCHIVE_ENTRIES)
            .map(|n| format!("{n}.txt"))
            .collect::<Vec<_>>();
        raw_zip(
            &archive,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            b"",
            CompressionMethod::Stored,
        );
        assert!(matches!(
            list_zip(&archive),
            Err(WorkspaceArchiveError::TooManyEntries { .. })
        ));
    }
    #[test]
    fn malformed_sizes_and_crc_do_not_return_unverified_data() {
        let temp = crate::tempdir().unwrap();
        let archive = temp.path().join("corrupt.zip");
        raw_zip(
            &archive,
            &["entry"],
            b"test payload",
            CompressionMethod::Stored,
        );
        let mut bytes = fs::read(&archive).unwrap();
        let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
        fs::write(&archive, bytes).unwrap();
        assert!(read_zip_entry(&archive, "entry").is_err());
        raw_zip(
            &archive,
            &["entry"],
            b"test payload",
            CompressionMethod::Stored,
        );
        let mut bytes = fs::read(&archive).unwrap();
        let payload = bytes
            .windows(12)
            .position(|w| w == b"test payload")
            .unwrap();
        bytes[payload] ^= 1;
        fs::write(&archive, bytes).unwrap();
        assert!(read_zip_entry(&archive, "entry").is_err());
    }
    #[test]
    fn read_limits_and_rejected_rewrites_preserve_the_previous_archive() {
        let temp = crate::tempdir().unwrap();
        let archive = temp.path().join("bundle.zip");
        raw_zip(
            &archive,
            &["big"],
            &vec![42; MAX_WORKSPACE_ARCHIVE_READ_BYTES as usize + 1],
            CompressionMethod::Stored,
        );
        assert!(matches!(
            read_zip_entry(&archive, "big"),
            Err(WorkspaceArchiveError::ReadTooLarge { .. })
        ));
        let previous = fs::read(&archive).unwrap();
        let oversized = temp.path().join("replacement");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_WORKSPACE_ARCHIVE_ENTRY_BYTES + 1)
            .unwrap();
        assert!(replace_zip_entry(&archive, "big", &oversized).is_err());
        assert_eq!(fs::read(&archive).unwrap(), previous);
        assert!(remove_zip_entry(&archive, "absent").is_err());
        assert_eq!(fs::read(&archive).unwrap(), previous);
        assert!(copy_exact_bounded(&mut Cursor::new(b"longer"), &mut Vec::new(), 2).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn archive_links_and_filesystem_links_cannot_escape_the_destination() {
        use std::os::unix::fs::symlink;
        let temp = crate::tempdir().unwrap();
        let archive = temp.path().join("bundle.zip");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link", "../outside", SimpleFileOptions::default())
            .unwrap();
        fs::write(&archive, writer.finish().unwrap().into_inner()).unwrap();
        assert!(matches!(
            list_zip(&archive),
            Err(WorkspaceArchiveError::SpecialEntry(_))
        ));
        raw_zip(
            &archive,
            &["nested/entry"],
            b"data",
            CompressionMethod::Stored,
        );
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let destination = temp.path().join("output");
        fs::create_dir(&destination).unwrap();
        symlink(&outside, destination.join("nested")).unwrap();
        assert!(extract_zip(&archive, &[], &destination).is_err());
        assert!(!outside.join("entry").exists());
    }
}
