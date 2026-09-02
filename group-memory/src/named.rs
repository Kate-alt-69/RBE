//! Named group/segment layer built on [`GroupMemoryRegion`].
//!
//! This layer is optional. Consumers that only need a raw anonymous or
//! file-backed mapping can use `GroupMemoryRegion` directly.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::{AccessMode, GroupMemoryRegion, GroupReadLease, GroupWriteLease, Result};

const MAX_NAME_LEN: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError {
    kind: &'static str,
    value: String,
    reason: &'static str,
}

impl NameError {
    fn new(kind: &'static str, value: &str, reason: &'static str) -> Self {
        Self {
            kind,
            value: value.to_string(),
            reason,
        }
    }
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid group-memory {} name {:?}: {}",
            self.kind, self.value, self.reason
        )
    }
}

impl std::error::Error for NameError {}

macro_rules! memory_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl AsRef<str>) -> std::result::Result<Self, NameError> {
                let value = value.as_ref();
                validate_name($kind, value)?;
                Ok(Self(value.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl TryFrom<&str> for $name {
            type Error = NameError;

            fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

memory_id!(GroupId, "group");
memory_id!(SegmentId, "segment");

/// Root namespace for named group-memory files.
///
/// The store is only path/layout policy. It does not own a daemon, process,
/// registry thread, or global singleton.
#[derive(Debug, Clone)]
pub struct GroupMemoryStore {
    root: PathBuf,
}

impl GroupMemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn segment_path(&self, group: &GroupId, segment: &SegmentId) -> PathBuf {
        self.root
            .join(group.as_str())
            .join(format!("{}.dat", segment.as_str()))
    }

    pub fn create_segment(
        &self,
        group: GroupId,
        segment: SegmentId,
        payload_len: usize,
    ) -> Result<GroupSegment> {
        let path = self.segment_path(&group, &segment);
        let region = GroupMemoryRegion::create(&path, payload_len)?;
        Ok(GroupSegment {
            group,
            segment,
            region,
        })
    }

    pub fn open_segment(
        &self,
        group: GroupId,
        segment: SegmentId,
        access: AccessMode,
    ) -> Result<GroupSegment> {
        let path = self.segment_path(&group, &segment);
        let region = GroupMemoryRegion::open(&path, access)?;
        Ok(GroupSegment {
            group,
            segment,
            region,
        })
    }
}

/// A named segment plus its mapped region.
pub struct GroupSegment {
    group: GroupId,
    segment: SegmentId,
    region: GroupMemoryRegion,
}

impl GroupSegment {
    pub fn group(&self) -> &GroupId {
        &self.group
    }

    pub fn segment(&self) -> &SegmentId {
        &self.segment
    }

    pub fn payload_len(&self) -> usize {
        self.region.payload_len()
    }

    pub fn generation(&self) -> Result<u64> {
        self.region.generation()
    }

    pub fn read_lease(&self) -> Result<GroupReadLease<'_>> {
        self.region.read_lease()
    }

    pub fn write_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.region.write_lease()
    }

    pub fn repair_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.region.repair_lease()
    }

    pub fn with_read<T>(&self, read: impl FnOnce(&[u8]) -> T) -> Result<T> {
        self.region.with_read(read)
    }

    /// Mutating a segment reserves a new generation before the closure and
    /// commits that generation only after the closure returns normally.
    pub fn with_write<T>(&mut self, write: impl FnOnce(&mut [u8]) -> T) -> Result<T> {
        self.region.with_write(write)
    }

    pub fn flush(&self) -> Result<()> {
        self.region.flush()
    }

    pub fn into_region(self) -> GroupMemoryRegion {
        self.region
    }
}

fn validate_name(kind: &'static str, value: &str) -> std::result::Result<(), NameError> {
    if value.is_empty() {
        return Err(NameError::new(kind, value, "name cannot be empty"));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(NameError::new(
            kind,
            value,
            "name exceeds the 96-byte limit",
        ));
    }
    if matches!(value, "." | "..") {
        return Err(NameError::new(
            kind,
            value,
            "relative path components are forbidden",
        ));
    }

    let mut chars = value.chars();
    let first = chars.next().expect("empty names returned above");
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return Err(NameError::new(
            kind,
            value,
            "name must start with an ASCII letter, digit, or underscore",
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')) {
        return Err(NameError::new(
            kind,
            value,
            "only ASCII letters, digits, underscore, hyphen, and dot are allowed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_path_traversal_and_separators() {
        assert!(GroupId::new("accounts").is_ok());
        assert!(SegmentId::new("users-index.v1").is_ok());
        assert!(GroupId::new("../accounts").is_err());
        assert!(GroupId::new("accounts/users").is_err());
        assert!(SegmentId::new("users\\admin").is_err());
        assert!(SegmentId::new(".").is_err());
        assert!(SegmentId::new("..").is_err());
    }

    #[test]
    fn named_segments_preserve_identity_and_generation() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-group-store-test-{}-{nonce}",
            std::process::id()
        ));
        let store = GroupMemoryStore::new(&root);
        let group = GroupId::new("accounts").unwrap();
        let segment = SegmentId::new("users-index").unwrap();

        let mut writer = store
            .create_segment(group.clone(), segment.clone(), 32)
            .unwrap();
        assert_eq!(writer.group(), &group);
        assert_eq!(writer.segment(), &segment);
        assert_eq!(writer.generation().unwrap(), 0);
        writer.with_write(|payload| payload[0] = 55).unwrap();
        assert_eq!(writer.generation().unwrap(), 1);

        let reader = store
            .open_segment(group, segment, AccessMode::ReadOnly)
            .unwrap();
        assert_eq!(reader.generation().unwrap(), 1);
        assert_eq!(reader.with_read(|payload| payload[0]).unwrap(), 55);

        drop(reader);
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }
}
