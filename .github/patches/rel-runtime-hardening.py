from pathlib import Path
import sys


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_count(path: str, old: str, new: str, expected: int, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != expected:
        raise SystemExit(f"{label}: expected {expected} anchors, found {count}")
    file.write_text(text.replace(old, new), encoding="utf-8")


def rel_hash() -> None:
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        "use std::collections::hash_map::DefaultHasher;\nuse std::collections::HashMap;\nuse std::fs::{self, OpenOptions};\nuse std::hash::{Hash, Hasher};\n",
        "use std::collections::HashMap;\nuse std::fs::{self, OpenOptions};\n",
        "remove probabilistic route hasher imports",
    )
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        "use crate::analyzer::{analyze, Severity};\n",
        "use sha2::{Digest, Sha256};\n\nuse crate::analyzer::{analyze, Severity};\n",
        "route SHA-256 import",
    )
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        '''pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {\n    let mut hasher = DefaultHasher::new();\n    bytes.hash(&mut hasher);\n    hasher.finish()\n}\n''',
        '''pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {\n    Sha256::digest(bytes).into()\n}\n''',
        "route content hash",
    )
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        "struct CacheEntry {\n    hash: u64,\n",
        "struct CacheEntry {\n    hash: [u8; 32],\n",
        "route cache hash width",
    )

    replace_once(
        "engine/crates/route-engine/src/cache.rs",
        "const CACHE_MANIFEST_VERSION: u64 = 3;\n",
        "const CACHE_MANIFEST_VERSION: u64 = 4;\n",
        "route cache manifest version",
    )
    replace_count(
        "engine/crates/route-engine/src/cache.rs",
        "current_hash: u64,",
        "current_hash: [u8; 32],",
        2,
        "route cache hash signatures",
    )
    replace_once(
        "engine/crates/route-engine/src/cache.rs",
        '''    if !artifact_path.is_file() {\n        return false;\n    }\n    let Ok(existing) = io.read(manifest_path) else {\n''',
        '''    if !artifact_path.is_file() {\n        return false;\n    }\n    let current_hash = hex::encode(current_hash);\n    let Ok(existing) = io.read(manifest_path) else {\n''',
        "route cache encoded SHA-256",
    )
    replace_once(
        "engine/crates/route-engine/src/cache.rs",
        "            != Some(current_hash.to_string().as_str())\n",
        "            != Some(current_hash.as_str())\n",
        "route manifest hash comparison",
    )
    replace_once(
        "engine/crates/route-engine/src/cache.rs",
        '''        "source_hash": current_hash.to_string(),\n''',
        '''        "source_hash": hex::encode(current_hash),\n''',
        "route manifest SHA-256 storage",
    )
    replace_once(
        "engine/crates/route-engine/src/cache.rs",
        '''        let first = sync(&io, &api_dir, &cache_root).unwrap();\n        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));\n        let second = sync(&io, &api_dir, &cache_root).unwrap();\n''',
        '''        let first = sync(&io, &api_dir, &cache_root).unwrap();\n        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));\n        let manifest: serde_json::Value = serde_json::from_slice(\n            &std::fs::read(&first[0].manifest_path).unwrap(),\n        )\n        .unwrap();\n        assert_eq!(manifest["version"].as_u64(), Some(CACHE_MANIFEST_VERSION));\n        let source_hash = manifest["source_hash"].as_str().unwrap();\n        assert_eq!(source_hash.len(), 64);\n        assert!(source_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));\n        assert_eq!(source_hash, hex::encode(hash_bytes(\n            b"class Route { get(req) { return true; } }"\n        )));\n        let second = sync(&io, &api_dir, &cache_root).unwrap();\n''',
        "route cache SHA-256 regression",
    )


def rel_tracker() -> None:
    replace_once(
        "engine/crates/route-engine/src/execution_tracker.rs",
        "use std::sync::{Arc, Mutex};\n",
        "use std::sync::{Arc, Mutex, MutexGuard};\n",
        "invocation tracker MutexGuard import",
    )
    replace_once(
        "engine/crates/route-engine/src/execution_tracker.rs",
        '''    pub fn new(policy: RecursionPolicy) -> Arc<Self> {\n        Arc::new(Self {\n            policy,\n            next_id: AtomicU64::new(1),\n            state: Mutex::new(TrackerState::default()),\n        })\n    }\n\n    pub fn begin(\n''',
        '''    pub fn new(policy: RecursionPolicy) -> Arc<Self> {\n        Arc::new(Self {\n            policy,\n            next_id: AtomicU64::new(1),\n            state: Mutex::new(TrackerState::default()),\n        })\n    }\n\n    fn lock_state(&self) -> MutexGuard<'_, TrackerState> {\n        match self.state.lock() {\n            Ok(state) => state,\n            Err(poisoned) => {\n                // Invocation bookkeeping is reconstructable request-local state.\n                // A panic while mutating it must not permanently poison REL or\n                // trigger a second panic from InvocationGuard::drop during unwind.\n                let mut state = poisoned.into_inner();\n                *state = TrackerState::default();\n                self.state.clear_poison();\n                state\n            }\n        }\n    }\n\n    pub fn begin(\n''',
        "invocation tracker poison recovery helper",
    )
    replace_count(
        "engine/crates/route-engine/src/execution_tracker.rs",
        'self.state.lock().expect("REL invocation tracker poisoned")',
        "self.lock_state()",
        7,
        "invocation tracker one-line locks",
    )
    replace_once(
        "engine/crates/route-engine/src/execution_tracker.rs",
        '''        self.state\n            .lock()\n            .expect("REL invocation tracker poisoned")\n            .invocations\n''',
        '''        self.lock_state()\n            .invocations\n''',
        "invocation tracker snapshot lock",
    )
    replace_once(
        "engine/crates/route-engine/src/execution_tracker.rs",
        '''    #[test]\n    fn legitimate_recursive_calls_are_allowed_within_budget() {\n''',
        '''    #[test]\n    fn poisoned_tracker_resets_without_panicking_guard_drop() {\n        let tracker = InvocationTracker::new(RecursionPolicy::default());\n        let guard = tracker.begin(None, symbol("root"), 1).unwrap();\n\n        let poison_target = tracker.clone();\n        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {\n            let _state = poison_target.state.lock().unwrap();\n            panic!("intentional invocation tracker poison");\n        }));\n        assert!(poisoned.is_err());\n        assert!(tracker.state.is_poisoned());\n\n        // This used to panic from Drop while an earlier panic was unwinding.\n        drop(guard);\n        assert!(!tracker.state.is_poisoned());\n\n        let recovered = tracker.begin(None, symbol("recovered"), 2).unwrap();\n        assert_eq!(tracker.snapshot(recovered.id()).unwrap().depth, 1);\n    }\n\n    #[test]\n    fn legitimate_recursive_calls_are_allowed_within_budget() {\n''',
        "invocation tracker poison regression",
    )


def rel_cache_poison() -> None:
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        "use std::sync::{Arc, Mutex};\n",
        "use std::sync::{Arc, Mutex, MutexGuard};\n",
        "route cache MutexGuard import",
    )
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        '''    pub fn new() -> Self {\n        Self::default()\n    }\n\n    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {\n''',
        '''    pub fn new() -> Self {\n        Self::default()\n    }\n\n    fn lock_entries(&self) -> MutexGuard<'_, HashMap<PathBuf, CacheEntry>> {\n        match self.entries.lock() {\n            Ok(entries) => entries,\n            Err(poisoned) => {\n                // Parsed Routes are disposable cache state. If a parser/cache\n                // thread panics, discard the possibly-partial cache and reparse\n                // from authoritative source instead of poisoning every reload.\n                let mut entries = poisoned.into_inner();\n                entries.clear();\n                self.entries.clear_poison();\n                entries\n            }\n        }\n    }\n\n    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {\n''',
        "route cache poison recovery helper",
    )
    replace_count(
        "engine/crates/route-engine/src/discovery.rs",
        "self.entries.lock().unwrap()",
        "self.lock_entries()",
        2,
        "route cache poisoned locks",
    )
    replace_once(
        "engine/crates/route-engine/src/discovery.rs",
        '''        Ok(file)\n    }\n}\n\npub(crate) fn collect_files(\n''',
        '''        Ok(file)\n    }\n}\n\n#[cfg(test)]\nmod route_cache_poison_tests {\n    use super::*;\n\n    #[test]\n    fn poisoned_route_cache_is_cleared_and_reparsed() {\n        let root = std::env::temp_dir().join(format!(\n            "rbe-route-cache-poison-{}",\n            std::process::id()\n        ));\n        let _ = fs::remove_dir_all(&root);\n        fs::create_dir_all(&root).unwrap();\n        let path = root.join("health.route");\n        fs::write(&path, "class Route { get(req) { return true; } }").unwrap();\n\n        let cache = RouteCache::new();\n        cache.load(&path).unwrap();\n        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {\n            let _entries = cache.entries.lock().unwrap();\n            panic!("intentional RouteCache poison");\n        }));\n        assert!(poisoned.is_err());\n        assert!(cache.entries.is_poisoned());\n\n        let changed = b"class Route { get(req) { return false; } }";\n        fs::write(&path, changed).unwrap();\n        cache.load(&path).unwrap();\n        assert!(!cache.entries.is_poisoned());\n        let entries = cache.lock_entries();\n        assert_eq!(entries.get(&path).unwrap().hash, hash_bytes(changed));\n        drop(entries);\n        let _ = fs::remove_dir_all(&root);\n    }\n}\n\npub(crate) fn collect_files(\n''',
        "route cache poison regression",
    )


def io_poison() -> None:
    replace_once(
        "atomic-io/src/lib.rs",
        "use std::sync::{Arc, Mutex};\n",
        "use std::sync::{Arc, Mutex, MutexGuard};\n",
        "AtomicIo MutexGuard import",
    )
    replace_once(
        "atomic-io/src/lib.rs",
        '''    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {\n        let key = path.to_path_buf();\n        let mut locks = self.inner.locks.lock().unwrap();\n        locks\n            .entry(key)\n            .or_insert_with(|| Arc::new(Mutex::new(())))\n            .clone()\n    }\n''',
        '''    fn lock_registry(&self) -> MutexGuard<'_, HashMap<PathBuf, Arc<Mutex<()>>>> {\n        match self.inner.locks.lock() {\n            Ok(locks) => locks,\n            Err(poisoned) => {\n                // The registry is only a cache of per-path locks. Throw it away\n                // after a panic rather than making all future I/O panic forever.\n                let mut locks = poisoned.into_inner();\n                locks.clear();\n                self.inner.locks.clear_poison();\n                locks\n            }\n        }\n    }\n\n    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {\n        let key = path.to_path_buf();\n        let mut locks = self.lock_registry();\n        locks\n            .entry(key)\n            .or_insert_with(|| Arc::new(Mutex::new(())))\n            .clone()\n    }\n''',
        "AtomicIo lock registry recovery",
    )
    replace_count(
        "atomic-io/src/lib.rs",
        "path_lock.lock().unwrap()",
        "lock_recover(&path_lock)",
        3,
        "AtomicIo per-path poisoned locks",
    )
    replace_once(
        "atomic-io/src/lib.rs",
        "        let mut locks = self.inner.locks.lock().unwrap();\n        locks.retain(|_, lock| Arc::strong_count(lock) > 1);\n",
        "        let mut locks = self.lock_registry();\n        locks.retain(|_, lock| Arc::strong_count(lock) > 1);\n",
        "AtomicIo sweep poisoned registry",
    )
    replace_once(
        "atomic-io/src/lib.rs",
        '''}\n\nfn parent_dir(path: &Path) -> &Path {\n''',
        '''}\n\nfn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {\n    match mutex.lock() {\n        Ok(guard) => guard,\n        Err(poisoned) => {\n            mutex.clear_poison();\n            poisoned.into_inner()\n        }\n    }\n}\n\nfn parent_dir(path: &Path) -> &Path {\n''',
        "AtomicIo path lock recovery helper",
    )
    replace_once(
        "atomic-io/src/lib.rs",
        '''    #[test]\n    fn sweep_locks_removes_unreferenced_entries() {\n''',
        '''    #[test]\n    fn poisoned_path_lock_does_not_disable_future_io() {\n        let dir = temp_dir("poison-path");\n        let io = AtomicIo::new();\n        let path = dir.join("file.txt");\n        let path_lock = io.lock_for(&path);\n        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {\n            let _guard = path_lock.lock().unwrap();\n            panic!("intentional path lock poison");\n        }));\n        assert!(poisoned.is_err());\n        assert!(path_lock.is_poisoned());\n\n        io.write_atomic(&path, b"recovered").unwrap();\n        assert!(!path_lock.is_poisoned());\n        assert_eq!(io.read(&path).unwrap(), b"recovered");\n        let _ = fs::remove_dir_all(&dir);\n    }\n\n    #[test]\n    fn poisoned_registry_is_discarded_instead_of_panicking() {\n        let dir = temp_dir("poison-registry");\n        let io = AtomicIo::new();\n        let path = dir.join("file.txt");\n        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {\n            let _guard = io.inner.locks.lock().unwrap();\n            panic!("intentional registry poison");\n        }));\n        assert!(poisoned.is_err());\n        assert!(io.inner.locks.is_poisoned());\n\n        io.write_atomic(&path, b"recovered").unwrap();\n        assert!(!io.inner.locks.is_poisoned());\n        assert_eq!(io.read(&path).unwrap(), b"recovered");\n        let _ = fs::remove_dir_all(&dir);\n    }\n\n    #[test]\n    fn sweep_locks_removes_unreferenced_entries() {\n''',
        "AtomicIo poison regressions",
    )
    replace_once(
        "atomic-io/src/lib.rs",
        "        let locks = io.inner.locks.lock().unwrap();\n",
        "        let locks = io.lock_registry();\n",
        "AtomicIo sweep test poison-safe inspection",
    )


STAGES = {
    "rel-hash": rel_hash,
    "rel-tracker": rel_tracker,
    "rel-cache-poison": rel_cache_poison,
    "io-poison": io_poison,
}

if len(sys.argv) != 2 or sys.argv[1] not in STAGES:
    raise SystemExit("usage: rel-runtime-hardening.py <rel-hash|rel-tracker|rel-cache-poison|io-poison>")

STAGES[sys.argv[1]]()
