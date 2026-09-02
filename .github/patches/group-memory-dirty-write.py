from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


path = Path("group-memory/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "const HEADER_LEN: usize = 64;\npub const FORMAT_VERSION: u32 = 1;",
    "const HEADER_LEN: usize = 64;\nconst WRITE_IN_PROGRESS_OFFSET: usize = 32;\npub const FORMAT_VERSION: u32 = 1;",
    "header constants",
)
source = replace_once(
    source,
    '''    #[error("group-memory generation counter overflowed")]
    GenerationOverflow,
''',
    '''    #[error("group-memory generation counter overflowed")]
    GenerationOverflow,
    #[error("group-memory generation {generation} was left by an interrupted writer; acquire repair_lease() before reading or writing")]
    UncleanWrite { generation: u64 },
''',
    "unclean write error",
)

old_read = '''    pub fn read_lease(&self) -> Result<GroupReadLease<'_>> {
        let lock = self.lock_shared()?;
        let generation = self.generation_unlocked();
        let payload = self.payload();
        Ok(GroupReadLease {
            _lock: lock,
            payload,
            generation,
        })
    }
'''
new_read = '''    pub fn read_lease(&self) -> Result<GroupReadLease<'_>> {
        let lock = self.lock_shared()?;
        let generation = self.generation_unlocked();
        if self.write_in_progress_unlocked() {
            return Err(GroupMemoryError::UncleanWrite { generation });
        }
        let payload = self.payload();
        Ok(GroupReadLease {
            _lock: lock,
            payload,
            generation,
        })
    }
'''
source = replace_once(source, old_read, new_read, "read lease")

start = source.find("    /// Acquire an exclusive payload lease. Generation advances while the\n")
end = source.find("    /// Read the payload for one closure-scoped shared lease.\n", start)
if start < 0 or end < 0:
    raise SystemExit("write lease block boundaries missing")
new_write_block = r'''    /// Acquire an exclusive payload lease. Generation advances and a persistent
    /// write-in-progress marker is set before mutable bytes are exposed.
    ///
    /// Explicit lease users must call [`GroupWriteLease::commit`] after their
    /// mutation is coherent. Dropping an uncommitted lease deliberately leaves
    /// the marker set, so task cancellation, panic unwinding, or process death
    /// cannot silently publish a partial payload as valid.
    pub fn write_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.begin_write_lease(false)
    }

    /// Acquire the exclusive recovery lease for a region left dirty by an
    /// interrupted writer. This is the only API that may intentionally mutate
    /// an unclean region. Repair still advances generation and must be
    /// explicitly committed before ordinary reads/writes are allowed again.
    pub fn repair_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.begin_write_lease(true)
    }

    fn begin_write_lease(&mut self, allow_unclean: bool) -> Result<GroupWriteLease<'_>> {
        if self.access != AccessMode::ReadWrite {
            return Err(GroupMemoryError::ReadOnly);
        }
        let lock = self.lock_exclusive()?;
        let current = self.generation_unlocked();
        if self.write_in_progress_unlocked() && !allow_unclean {
            return Err(GroupMemoryError::UncleanWrite {
                generation: current,
            });
        }
        let next = current
            .checked_add(1)
            .ok_or(GroupMemoryError::GenerationOverflow)?;
        let payload_len = self.payload_len;
        let Mapping::ReadWrite(mapping) = &mut self.mapping else {
            return Err(GroupMemoryError::ReadOnly);
        };
        let (header, payload_area) = mapping.split_at_mut(HEADER_LEN);
        header[24..32].copy_from_slice(&next.to_le_bytes());
        header[WRITE_IN_PROGRESS_OFFSET] = 1;
        let dirty_marker = &mut header[WRITE_IN_PROGRESS_OFFSET];
        let payload = &mut payload_area[..payload_len];
        Ok(GroupWriteLease {
            _lock: lock,
            payload,
            generation: next,
            dirty_marker,
            committed: false,
        })
    }

'''
source = source[:start] + new_write_block + source[end:]

source = replace_once(
    source,
    '''    pub fn with_write<T>(&mut self, write: impl FnOnce(&mut [u8]) -> T) -> Result<T> {
        let mut lease = self.write_lease()?;
        Ok(write(&mut lease))
    }
''',
    '''    pub fn with_write<T>(&mut self, write: impl FnOnce(&mut [u8]) -> T) -> Result<T> {
        let mut lease = self.write_lease()?;
        let result = write(&mut lease);
        lease.commit();
        Ok(result)
    }
''',
    "with_write commit",
)

# Replace generation mutator helper with dirty-state reader; write lease now
# updates generation and marker together after checking overflow.
start = source.find("    fn advance_generation_unlocked(&mut self) -> Result<u64> {\n")
end = source.find("    fn lock_shared(&self) -> Result<Option<OwnedFileLock>> {\n", start)
if start < 0 or end < 0:
    raise SystemExit("generation helper boundaries missing")
helper = r'''    fn write_in_progress_unlocked(&self) -> bool {
        let header = match &self.mapping {
            Mapping::ReadOnly(mapping) => &mapping[..HEADER_LEN],
            Mapping::ReadWrite(mapping) => &mapping[..HEADER_LEN],
        };
        header[WRITE_IN_PROGRESS_OFFSET] != 0
    }

'''
source = source[:start] + helper + source[end:]

# Tests: explicit leases now need commit; add dirty/repair regression.
source = replace_once(
    source,
    '''            let mut write = region.write_lease().unwrap();
            assert_eq!(write.generation(), 1);
            write[0] = 99;
            drop(write);
''',
    '''            let mut write = region.write_lease().unwrap();
            assert_eq!(write.generation(), 1);
            write[0] = 99;
            write.commit();
''',
    "explicit lease test commit",
)
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Group Memory tests tail missing")
test = r'''

    #[test]
    fn interrupted_write_fails_closed_until_explicit_repair() {
        let path = test_path("unclean-write");
        {
            let mut region = GroupMemoryRegion::create(&path, 16).unwrap();
            {
                let mut interrupted = region.write_lease().unwrap();
                interrupted[0] = 41;
                // Intentionally do not commit: this simulates cancellation or
                // process death after some bytes were already mutated.
            }

            assert!(matches!(
                region.read_lease(),
                Err(GroupMemoryError::UncleanWrite { generation: 1 })
            ));
            assert!(matches!(
                region.write_lease(),
                Err(GroupMemoryError::UncleanWrite { generation: 1 })
            ));

            let mut repair = region.repair_lease().unwrap();
            assert_eq!(repair.generation(), 2);
            repair.fill(0);
            repair[0] = 99;
            repair.commit();

            let read = region.read_lease().unwrap();
            assert_eq!(read.generation(), 2);
            assert_eq!(read[0], 99);
        }

        let reader = GroupMemoryRegion::open(&path, AccessMode::ReadOnly).unwrap();
        assert_eq!(reader.with_read(|payload| payload[0]).unwrap(), 99);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn closure_write_commits_only_after_callback_returns() {
        let path = test_path("closure-commit");
        let mut region = GroupMemoryRegion::create(&path, 8).unwrap();
        region.with_write(|payload| payload[0] = 7).unwrap();
        assert_eq!(region.with_read(|payload| payload[0]).unwrap(), 7);
        assert_eq!(region.generation().unwrap(), 1);
        let _ = fs::remove_file(path);
    }
'''
source = source[:tests_end] + test + source[tests_end:]
path.write_text(source)


path = Path("group-memory/src/lease.rs")
source = path.read_text()
source = replace_once(
    source,
    '''pub struct GroupWriteLease<'a> {
    pub(crate) _lock: Option<OwnedFileLock>,
    pub(crate) payload: &'a mut [u8],
    pub(crate) generation: u64,
}
''',
    '''pub struct GroupWriteLease<'a> {
    // Keep the lock field before the borrowed mapping fields so the custom Drop
    // runs while the OS lock is still unquestionably owned by this guard.
    pub(crate) _lock: Option<OwnedFileLock>,
    pub(crate) payload: &'a mut [u8],
    pub(crate) generation: u64,
    pub(crate) dirty_marker: &'a mut u8,
    pub(crate) committed: bool,
}
''',
    "write lease fields",
)
source = replace_once(
    source,
    '''impl GroupWriteLease<'_> {
    /// Generation reserved for writes performed through this lease.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}
''',
    '''impl GroupWriteLease<'_> {
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
''',
    "write lease commit",
)
path.write_text(source)


path = Path("group-memory/src/named.rs")
source = path.read_text()
source = replace_once(
    source,
    '''    pub fn write_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.region.write_lease()
    }
''',
    '''    pub fn write_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.region.write_lease()
    }

    pub fn repair_lease(&mut self) -> Result<GroupWriteLease<'_>> {
        self.region.repair_lease()
    }
''',
    "named repair lease",
)
source = source.replace(
    "    /// Mutating a segment advances its persisted generation after the closure\n    /// completes while the same exclusive mapping lock is still held.\n",
    "    /// Mutating a segment reserves a new generation before the closure and\n    /// commits that generation only after the closure returns normally.\n",
    1,
)
path.write_text(source)
