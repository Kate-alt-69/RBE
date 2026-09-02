//! Explicit read/write lease guards for group-memory payloads.
//!
//! Leases are process-agnostic. File-backed regions keep their cooperative OS
//! file lock for the lifetime of the guard; anonymous mappings simply borrow
//! the payload for the same API shape.

use std::ops::{Deref, DerefMut};

use crate::OwnedFileLock;

/// Shared payload lease.
///
/// For a file-backed region `_lock` owns a cloned handle to the same OS file,
/// keeping its shared lock alive until this value is dropped.
pub struct GroupReadLease<'a> {
    pub(crate) _lock: Option<OwnedFileLock>,
    pub(crate) payload: &'a [u8],
    pub(crate) generation: u64,
}

impl GroupReadLease<'_> {
    /// Generation observed while the shared lease was acquired.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Deref for GroupReadLease<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.payload
    }
}

/// Exclusive payload lease.
///
/// Acquiring this lease reserves/increments the region generation before the
/// mutable payload is exposed. For file-backed regions the exclusive OS file
/// lock stays alive until the guard is dropped.
pub struct GroupWriteLease<'a> {
    // Keep the lock field before the borrowed mapping fields so the custom Drop
    // runs while the OS lock is still unquestionably owned by this guard.
    pub(crate) _lock: Option<OwnedFileLock>,
    pub(crate) payload: &'a mut [u8],
    pub(crate) generation: u64,
    pub(crate) dirty_marker: &'a mut u8,
    pub(crate) committed: bool,
}

impl GroupWriteLease<'_> {
    /// Generation reserved for writes performed through this lease.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Publish the current mutation as a coherent generation. This clears the
    /// shared write-in-progress marker while the exclusive OS lock is still
    /// held. It does not imply power-loss durability; call `GroupMemoryRegion::flush`
    /// separately when durable backing-file persistence is required.
    pub fn commit(mut self) {
        *self.dirty_marker = 0;
        self.committed = true;
    }
}

impl Drop for GroupWriteLease<'_> {
    fn drop(&mut self) {
        // An uncommitted lease deliberately leaves the dirty marker set. This
        // turns panic/cancellation/process death into a fail-closed recovery
        // condition instead of silently exposing partially updated bytes.
        if self.committed {
            *self.dirty_marker = 0;
        }
    }
}

impl Deref for GroupWriteLease<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.payload
    }
}

impl DerefMut for GroupWriteLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.payload
    }
}
