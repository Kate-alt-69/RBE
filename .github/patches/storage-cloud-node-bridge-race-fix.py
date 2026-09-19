from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Same-path project writes must serialize from journal prepare through the actual
# atomic file replacement and journal publish. Otherwise prepare order and write
# order can diverge, causing the winning file version to lose its sync intent.
replace_once(
    "storage-sync-journal/Cargo.toml",
    'hex = "0.4"\n',
    'hex = "0.4"\nfs2 = "0.4"\n',
    "journal filesystem lock dependency",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
''',
    '''use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
''',
    "journal OpenOptions import",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
''',
    '''use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
''',
    "journal fs2 FileExt import",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''#[derive(Debug, Clone)]
pub struct PreparedStorageSyncIntent {
    intent: StorageSyncIntent,
    pending_path: PathBuf,
    ready_path: PathBuf,
}
''',
    '''#[derive(Debug)]
pub struct PreparedStorageSyncIntent {
    intent: StorageSyncIntent,
    pending_path: PathBuf,
    ready_path: PathBuf,
    // Held for the entire caller-side file replacement + publish sequence.
    // Persistent lock files are intentional: deleting a locked pathname can
    // create a second inode and split mutual exclusion on Unix.
    _path_lock: File,
}
''',
    "journal prepared lock guard",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''    let pending = pending_dir(&root);
    let ready = ready_dir(&root);
    fs::create_dir_all(&pending)?;
    fs::create_dir_all(&ready)?;
    let pending_path = pending.join(format!("{id}.json"));
    let ready_path = ready.join(format!("{id}.json"));
    write_intent(&pending_path, &intent)?;
    Ok(PreparedStorageSyncIntent {
        intent,
        pending_path,
        ready_path,
    })
''',
    '''    let locks = lock_dir(&root);
    let pending = pending_dir(&root);
    let ready = ready_dir(&root);
    fs::create_dir_all(&locks)?;
    fs::create_dir_all(&pending)?;
    fs::create_dir_all(&ready)?;

    let path_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(locks.join(format!("{id}.lock")))?;
    path_lock.lock_exclusive()?;

    let pending_path = pending.join(format!("{id}.json"));
    let ready_path = ready.join(format!("{id}.json"));
    write_intent(&pending_path, &intent)?;
    Ok(PreparedStorageSyncIntent {
        intent,
        pending_path,
        ready_path,
        _path_lock: path_lock,
    })
''',
    "journal same-path lock acquisition",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''fn pending_dir(root: &Path) -> PathBuf {
    journal_root(root).join("pending")
}

fn ready_dir(root: &Path) -> PathBuf {
''',
    '''fn lock_dir(root: &Path) -> PathBuf {
    journal_root(root).join("locks")
}

fn pending_dir(root: &Path) -> PathBuf {
    journal_root(root).join("pending")
}

fn ready_dir(root: &Path) -> PathBuf {
''',
    "journal lock directory",
)

# Reserve RBE internal state case-insensitively. This is required on Windows,
# where .RBE and .rbe refer to the same directory, and is a harmless stronger
# reservation on case-sensitive platforms.
replace_once(
    "storage-sync-journal/src/lib.rs",
    '''    if segments.is_empty() || segments.first().is_some_and(|segment| segment == ".rbe") {
        anyhow::bail!("Storage sync logical path targets RBE internal state");
    }
''',
    '''    if segments.is_empty()
        || segments
            .first()
            .is_some_and(|segment| segment.eq_ignore_ascii_case(".rbe"))
    {
        anyhow::bail!("Storage sync logical path targets RBE internal state");
    }
''',
    "journal case-insensitive internal path reservation",
)

replace_once(
    "container-runtime/crates/container-runtime-core/src/storage_capability.rs",
    '''    if segments
        .first()
        .is_some_and(|segment| segment.to_string_lossy() == ".rbe")
    {
''',
    '''    if segments.first().is_some_and(|segment| {
        segment
            .to_string_lossy()
            .eq_ignore_ascii_case(".rbe")
    }) {
''',
    "Container case-insensitive internal path reservation",
)

# Add a deterministic concurrency regression. The second thread announces that
# it is about to prepare, then must remain blocked until the first guard drops.
replace_once(
    "storage-sync-journal/src/lib.rs",
    '''    #[test]
    fn internal_rbe_paths_are_reserved() {
''',
    '''    #[test]
    fn same_logical_path_prepare_is_serialized_until_guard_drops() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let root = test_root("same-path-lock");
        let first = prepare(&root, "data/state.json", 2, b"first").unwrap();
        let worker_root = root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let second = prepare(&worker_root, "data/state.json", 1, b"second").unwrap();
            acquired_tx.send(()).unwrap();
            second
        });

        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(150)).is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = worker.join().unwrap();
        cancel(&second).unwrap();
        drop(second);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn internal_rbe_paths_are_reserved() {
''',
    "journal same-path serialization regression",
)

replace_once(
    "storage-sync-journal/src/lib.rs",
    '''        let error = prepare(&root, ".rbe/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(error.to_string().contains("internal state"));
''',
    '''        let error = prepare(&root, ".rbe/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(error.to_string().contains("internal state"));
        let mixed_case = prepare(&root, ".RBE/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(mixed_case.to_string().contains("internal state"));
''',
    "journal mixed-case internal path regression",
)

replace_once(
    "doc/storage.md",
    '''The `.rbe/` project subtree is reserved for RBE internal state and cannot be targeted by `storage.write`. `RBE_PROJECT_ROOT` can explicitly tell `cloud_node` where to consume the outbox; otherwise the parent directory of `setting.node.cn.json` is used.''',
    '''The `.rbe/` project subtree is reserved case-insensitively for RBE internal state and cannot be targeted by `storage.write`. Writes to the same logical `$$` path are serialized from intent staging through the atomic file replacement and intent publication, so concurrent callers cannot lose the intent for the file version that actually wins. `RBE_PROJECT_ROOT` can explicitly tell `cloud_node` where to consume the outbox; otherwise the parent directory of `setting.node.cn.json` is used.''',
    "Storage same-path serialization docs",
)
