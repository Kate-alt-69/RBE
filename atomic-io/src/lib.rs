//! Global atomic I/O layer: "all writes/reads happen through the
//! atomic writer." Shared between the main engine and the container
//! runtime's sandboxed environments — two separate Cargo workspaces,
//! same crate via relative path dependency. One implementation of
//! "write this safely" to get right and keep correct, not two
//! separate copies to keep in sync.
//!
//! Two real guarantees, scoped honestly:
//! - [`AtomicIo::write_atomic`]: full-file replace is genuinely
//!   atomic — write to a temp file in the same directory, `sync_all`,
//!   then `rename` over the target. `rename` is atomic at the OS level
//!   on both POSIX and Windows for same-volume renames, so a reader
//!   never observes a partially-written file, and a crash mid-write
//!   leaves the OLD file intact (or an orphaned temp file), never a
//!   corrupted target.
//! - [`AtomicIo::append_locked`] / [`AtomicIo::read`]: serialized via
//!   an in-process per-path lock registry, which prevents corruption
//!   from concurrent writers WITHIN THIS PROCESS. This does **not**
//!   provide cross-process atomicity for appends — there's no OS
//!   primitive for "atomically append N bytes" the way `rename` gives
//!   full-replacement. Don't oversell this: append-style writes (a
//!   log file) are safe against races between threads/tasks in this
//!   process, not against a second process writing the same file at
//!   the same time.
//!
//! [`AtomicIo::stats`] exposes running totals (bytes/ops) — the
//! numbers the container environments' disk-abuse detection watches.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone, Copy)]
pub struct IoStats {
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub write_ops: u64,
    pub read_ops: u64,
}

struct Counters {
    bytes_written: AtomicU64,
    bytes_read: AtomicU64,
    write_ops: AtomicU64,
    read_ops: AtomicU64,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            bytes_written: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            write_ops: AtomicU64::new(0),
            read_ops: AtomicU64::new(0),
        }
    }
}

/// The global atomic I/O gate. One instance is meant to be shared
/// (it's cheap to `Clone` — an `Arc` underneath) across everything
/// that touches disk in a given process: the engine's vault and
/// error-reporter, and each container environment's disk-write path.
#[derive(Clone)]
pub struct AtomicIo {
    inner: Arc<Inner>,
}

struct Inner {
    counters: Counters,
    // Per-path lock registry — serializes concurrent writers to the
    // SAME file within this process. A `Mutex<HashMap<...>>` guards
    // the registry itself; each entry is its own `Mutex<()>` so
    // unrelated paths don't block each other. "Atomic GLOBAL
    // reader/writer" means one shared gate for the whole process's
    // I/O — not literally one lock serializing every unrelated file
    // operation, which would make disk I/O from completely unrelated
    // subsystems block on each other for no reason.
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl Default for AtomicIo {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicIo {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                counters: Counters::default(),
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn stats(&self) -> IoStats {
        IoStats {
            bytes_written: self.inner.counters.bytes_written.load(Ordering::Relaxed),
            bytes_read: self.inner.counters.bytes_read.load(Ordering::Relaxed),
            write_ops: self.inner.counters.write_ops.load(Ordering::Relaxed),
            read_ops: self.inner.counters.read_ops.load(Ordering::Relaxed),
        }
    }

    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let key = path.to_path_buf();
        let mut locks = self.inner.locks.lock().unwrap();
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Atomically replaces the entire contents of `path` with `bytes`.
    /// Creates the parent directory if it doesn't exist.
    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let path_lock = self.lock_for(path);
        let _guard = path_lock.lock().unwrap();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = tmp_path_for(path);
        {
            let mut tmp_file = fs::File::create(&tmp_path)?;
            tmp_file.write_all(bytes)?;
            tmp_file.sync_all()?;
        }
        fs::rename(&tmp_path, path)?;

        self.inner
            .counters
            .bytes_written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.inner.counters.write_ops.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Appends `bytes` to `path`, creating it if needed. Serialized
    /// against other appends/writes to the same path *within this
    /// process* — see the module doc comment's scoping note.
    pub fn append_locked(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let path_lock = self.lock_for(path);
        let _guard = path_lock.lock().unwrap();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(bytes)?;

        self.inner
            .counters
            .bytes_written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.inner.counters.write_ops.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Reads the full contents of `path`, through the same lock
    /// registry as writes so a read can't observe a half-written
    /// `append_locked` call from another task in this process.
    pub fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let path_lock = self.lock_for(path);
        let _guard = path_lock.lock().unwrap();

        let bytes = fs::read(path)?;

        self.inner
            .counters
            .bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.inner.counters.read_ops.fetch_add(1, Ordering::Relaxed);

        Ok(bytes)
    }

    /// Opportunistic cleanup of the lock registry so a long-running
    /// process touching many distinct paths doesn't grow it
    /// unbounded. Call periodically from a supervised task — same
    /// pattern as the engine's rate-limiter/IP-strike `sweep()`
    /// methods. Only removes entries nothing is actively holding.
    pub fn sweep_locks(&self) {
        let mut locks = self.inner.locks.lock().unwrap();
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "tmp".to_string());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(".{file_name}.{unique}.tmp");
    match path.parent() {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atomic-io-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = temp_dir("roundtrip");
        let io = AtomicIo::new();
        let path = dir.join("file.txt");
        io.write_atomic(&path, b"hello world").unwrap();
        let read_back = io.read(&path).unwrap();
        assert_eq!(read_back, b"hello world");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_leaves_no_tmp_file_behind_on_success() {
        let dir = temp_dir("no-tmp-leftover");
        let io = AtomicIo::new();
        let path = dir.join("file.txt");
        io.write_atomic(&path, b"data").unwrap();
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the final file should remain, no .tmp");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_locked_accumulates() {
        let dir = temp_dir("append");
        let io = AtomicIo::new();
        let path = dir.join("log.txt");
        io.append_locked(&path, b"line1\n").unwrap();
        io.append_locked(&path, b"line2\n").unwrap();
        let contents = io.read(&path).unwrap();
        assert_eq!(contents, b"line1\nline2\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_track_bytes_and_ops() {
        let dir = temp_dir("stats");
        let io = AtomicIo::new();
        let path = dir.join("file.txt");
        io.write_atomic(&path, b"12345").unwrap();
        io.read(&path).unwrap();
        let stats = io.stats();
        assert_eq!(stats.bytes_written, 5);
        assert_eq!(stats.bytes_read, 5);
        assert_eq!(stats.write_ops, 1);
        assert_eq!(stats.read_ops, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_locks_removes_unreferenced_entries() {
        let dir = temp_dir("sweep");
        let io = AtomicIo::new();
        let path = dir.join("file.txt");
        io.write_atomic(&path, b"x").unwrap();
        io.sweep_locks();
        let locks = io.inner.locks.lock().unwrap();
        assert!(locks.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
