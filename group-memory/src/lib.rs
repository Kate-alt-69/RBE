//! General-purpose RBE group memory.
//!
//! This crate owns no process hierarchy and knows nothing about the backend,
//! Service Mother, `.service`, Video Manager, or container runtime. It provides
//! a small common primitive those systems can all use: private anonymous memory
//! or file-backed memory mapped into one or more cooperating processes.
//!
//! File-backed regions rely on the operating system's virtual-memory/page-cache
//! machinery for RAM-versus-disk residency. The crate does not manually copy
//! "cold" pages between heap allocations and `.dat` files.

use std::fs::{self, File, OpenOptions as FsOpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use memmap2::{Mmap, MmapMut, MmapOptions};
use thiserror::Error;

const MAGIC: [u8; 8] = *b"RBEGRP01";
const HEADER_LEN: usize = 64;
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingKind {
    Anonymous,
    FileBacked,
}

#[derive(Debug, Error)]
pub enum GroupMemoryError {
    #[error("group memory payload length must be greater than zero")]
    EmptyPayload,
    #[error("group memory region size overflow")]
    SizeOverflow,
    #[error("group memory region is read-only")]
    ReadOnly,
    #[error("file is not an RBE group-memory region")]
    InvalidMagic,
    #[error("unsupported group-memory format version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("group-memory file is truncated: expected at least {expected} bytes, found {actual}")]
    Truncated { expected: u64, actual: u64 },
    #[error("group-memory payload length does not fit this platform")]
    PayloadTooLarge,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GroupMemoryError>;

/// One mapped byte region. File-backed instances may be opened by multiple
/// processes; all cooperating users should use `with_read` / `with_write` so
/// the advisory file lock serializes writes and protects reads from writers.
pub struct GroupMemoryRegion {
    path: Option<PathBuf>,
    file: Option<File>,
    access: AccessMode,
    mapping: Mapping,
    payload_len: usize,
}

enum Mapping {
    ReadOnly(Mmap),
    ReadWrite(MmapMut),
}

impl GroupMemoryRegion {
    /// Create a new file-backed group-memory region.
    ///
    /// The file is created exclusively; existing data is never truncated by
    /// this constructor. Call `open` for an existing region.
    pub fn create(path: impl AsRef<Path>, payload_len: usize) -> Result<Self> {
        ensure_payload_len(payload_len)?;
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let total_len = total_len(payload_len)?;
        let file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(total_len as u64)?;

        let mut mapping = unsafe {
            // SAFETY: the file is held open by the returned region, its length
            // is set before mapping, and this constructor has exclusive file
            // creation ownership while the header is initialized.
            MmapOptions::new().len(total_len).map_mut(&file)?
        };
        write_header(&mut mapping[..HEADER_LEN], payload_len as u64);
        mapping.flush()?;

        Ok(Self {
            path: Some(path.to_path_buf()),
            file: Some(file),
            access: AccessMode::ReadWrite,
            mapping: Mapping::ReadWrite(mapping),
            payload_len,
        })
    }

    /// Open an existing file-backed region and validate its format header.
    pub fn open(path: impl AsRef<Path>, access: AccessMode) -> Result<Self> {
        let path = path.as_ref();
        let file = match access {
            AccessMode::ReadOnly => FsOpenOptions::new().read(true).open(path)?,
            AccessMode::ReadWrite => FsOpenOptions::new().read(true).write(true).open(path)?,
        };
        let (payload_len, total_len) = read_and_validate_header(&file)?;

        let mapping = match access {
            AccessMode::ReadOnly => Mapping::ReadOnly(unsafe {
                // SAFETY: the validated file length covers `total_len`, and
                // the File remains owned by this region for the mapping life.
                MmapOptions::new().len(total_len).map(&file)?
            }),
            AccessMode::ReadWrite => Mapping::ReadWrite(unsafe {
                // SAFETY: same lifetime/length guarantee as above, and the
                // file was opened with write permission.
                MmapOptions::new().len(total_len).map_mut(&file)?
            }),
        };

        Ok(Self {
            path: Some(path.to_path_buf()),
            file: Some(file),
            access,
            mapping,
            payload_len,
        })
    }

    /// Create process-private anonymous group memory. It uses the same header
    /// layout as file-backed regions but has no path and no cross-process lock.
    pub fn anonymous(payload_len: usize) -> Result<Self> {
        ensure_payload_len(payload_len)?;
        let total_len = total_len(payload_len)?;
        let mut mapping = MmapOptions::new().len(total_len).map_anon()?;
        write_header(&mut mapping[..HEADER_LEN], payload_len as u64);
        Ok(Self {
            path: None,
            file: None,
            access: AccessMode::ReadWrite,
            mapping: Mapping::ReadWrite(mapping),
            payload_len,
        })
    }

    pub fn payload_len(&self) -> usize {
        self.payload_len
    }

    pub fn access_mode(&self) -> AccessMode {
        self.access
    }

    pub fn backing_kind(&self) -> BackingKind {
        if self.file.is_some() {
            BackingKind::FileBacked
        } else {
            BackingKind::Anonymous
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Read the payload while holding a cooperative shared lock for a
    /// file-backed mapping. Anonymous mappings do not need an OS file lock.
    pub fn with_read<T>(&self, read: impl FnOnce(&[u8]) -> T) -> Result<T> {
        let _guard = self.lock_shared()?;
        Ok(read(self.payload()))
    }

    /// Mutate the payload while holding a cooperative exclusive lock.
    /// Changes become visible through other mappings according to the OS's
    /// normal shared-mapping coherence rules. Call `flush` when disk durability
    /// is required at a specific point.
    pub fn with_write<T>(&mut self, write: impl FnOnce(&mut [u8]) -> T) -> Result<T> {
        if self.access != AccessMode::ReadWrite {
            return Err(GroupMemoryError::ReadOnly);
        }
        let _guard = self.lock_exclusive()?;
        let payload = self.payload_mut().ok_or(GroupMemoryError::ReadOnly)?;
        Ok(write(payload))
    }

    /// Flush dirty writable pages to their backing file. Anonymous regions are
    /// already memory-only and treat this as a no-op.
    pub fn flush(&self) -> Result<()> {
        if let Mapping::ReadWrite(mapping) = &self.mapping {
            mapping.flush()?;
        }
        Ok(())
    }

    fn payload(&self) -> &[u8] {
        match &self.mapping {
            Mapping::ReadOnly(mapping) => &mapping[HEADER_LEN..HEADER_LEN + self.payload_len],
            Mapping::ReadWrite(mapping) => &mapping[HEADER_LEN..HEADER_LEN + self.payload_len],
        }
    }

    fn payload_mut(&mut self) -> Option<&mut [u8]> {
        match &mut self.mapping {
            Mapping::ReadOnly(_) => None,
            Mapping::ReadWrite(mapping) => {
                Some(&mut mapping[HEADER_LEN..HEADER_LEN + self.payload_len])
            }
        }
    }

    fn lock_shared(&self) -> Result<Option<OwnedFileLock>> {
        self.file.as_ref().map(OwnedFileLock::shared).transpose()
    }

    fn lock_exclusive(&self) -> Result<Option<OwnedFileLock>> {
        self.file.as_ref().map(OwnedFileLock::exclusive).transpose()
    }
}

struct OwnedFileLock {
    file: File,
}

impl OwnedFileLock {
    fn shared(file: &File) -> Result<Self> {
        let file = file.try_clone()?;
        FileExt::lock_shared(&file)?;
        Ok(Self { file })
    }

    fn exclusive(file: &File) -> Result<Self> {
        let file = file.try_clone()?;
        FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for OwnedFileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn ensure_payload_len(payload_len: usize) -> Result<()> {
    if payload_len == 0 {
        Err(GroupMemoryError::EmptyPayload)
    } else {
        Ok(())
    }
}

fn total_len(payload_len: usize) -> Result<usize> {
    HEADER_LEN
        .checked_add(payload_len)
        .ok_or(GroupMemoryError::SizeOverflow)
}

fn write_header(header: &mut [u8], payload_len: u64) {
    header.fill(0);
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[16..24].copy_from_slice(&payload_len.to_le_bytes());
}

fn read_and_validate_header(file: &File) -> Result<(usize, usize)> {
    let actual_len = file.metadata()?.len();
    if actual_len < HEADER_LEN as u64 {
        return Err(GroupMemoryError::Truncated {
            expected: HEADER_LEN as u64,
            actual: actual_len,
        });
    }

    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header)?;

    if header[..8] != MAGIC {
        return Err(GroupMemoryError::InvalidMagic);
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("fixed header slice"));
    if version != FORMAT_VERSION {
        return Err(GroupMemoryError::UnsupportedVersion {
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    let payload_u64 = u64::from_le_bytes(
        header[16..24]
            .try_into()
            .expect("fixed group-memory header slice"),
    );
    let payload_len =
        usize::try_from(payload_u64).map_err(|_| GroupMemoryError::PayloadTooLarge)?;
    ensure_payload_len(payload_len)?;
    let total_len = total_len(payload_len)?;
    if actual_len < total_len as u64 {
        return Err(GroupMemoryError::Truncated {
            expected: total_len as u64,
            actual: actual_len,
        });
    }
    Ok((payload_len, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-group-memory-{name}-{}-{nonce}.dat",
            std::process::id()
        ))
    }

    #[test]
    fn file_backed_regions_observe_the_same_payload() {
        let path = test_path("shared");
        {
            let mut writer = GroupMemoryRegion::create(&path, 32).unwrap();
            writer
                .with_write(|payload| payload[..5].copy_from_slice(b"hello"))
                .unwrap();
            writer.flush().unwrap();

            let reader = GroupMemoryRegion::open(&path, AccessMode::ReadOnly).unwrap();
            let observed = reader.with_read(|payload| payload[..5].to_vec()).unwrap();
            assert_eq!(observed, b"hello");
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn second_writable_mapping_observes_updates() {
        let path = test_path("coherent");
        {
            let mut first = GroupMemoryRegion::create(&path, 16).unwrap();
            let second = GroupMemoryRegion::open(&path, AccessMode::ReadWrite).unwrap();
            first.with_write(|payload| payload[7] = 91).unwrap();
            assert_eq!(second.with_read(|payload| payload[7]).unwrap(), 91);
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn read_only_mapping_rejects_mutation() {
        let path = test_path("readonly");
        {
            let creator = GroupMemoryRegion::create(&path, 8).unwrap();
            creator.flush().unwrap();
            drop(creator);

            let mut reader = GroupMemoryRegion::open(&path, AccessMode::ReadOnly).unwrap();
            assert!(matches!(
                reader.with_write(|payload| payload[0] = 1),
                Err(GroupMemoryError::ReadOnly)
            ));
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn anonymous_regions_are_private_and_memory_only() {
        let mut region = GroupMemoryRegion::anonymous(12).unwrap();
        assert_eq!(region.backing_kind(), BackingKind::Anonymous);
        assert!(region.path().is_none());
        region.with_write(|payload| payload[3] = 44).unwrap();
        assert_eq!(region.with_read(|payload| payload[3]).unwrap(), 44);
        region.flush().unwrap();
    }

    #[test]
    fn rejects_non_group_memory_files() {
        let path = test_path("invalid");
        fs::write(&path, vec![0_u8; HEADER_LEN]).unwrap();
        assert!(matches!(
            GroupMemoryRegion::open(&path, AccessMode::ReadOnly),
            Err(GroupMemoryError::InvalidMagic)
        ));
        let _ = fs::remove_file(path);
    }
}
