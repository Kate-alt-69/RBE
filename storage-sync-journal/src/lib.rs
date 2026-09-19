//! Durable bridge between capability-bound `storage.write` and Cloud Node ingest.
//!
//! The producer and consumer are different OS processes/workspaces, so this
//! crate deliberately contains only a tiny on-disk contract. It knows nothing
//! about REL, providers, tunnels, or Cloud Node manifests.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const STORAGE_SYNC_INTENT_VERSION: u16 = 1;
const INTENT_ID_DOMAIN: &[u8] = b"RBE-STORAGE-SYNC-INTENT/1\0";
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSyncIntent {
    pub format_version: u16,
    pub id: String,
    pub logical_path: String,
    pub level: u8,
    pub content_sha256: String,
    pub created_unix_ms: u64,
}

#[derive(Debug)]
pub struct PreparedStorageSyncIntent {
    intent: StorageSyncIntent,
    pending_path: PathBuf,
    ready_path: PathBuf,
    // Held for the entire caller-side file replacement + publish sequence.
    // Persistent lock files are intentional: deleting a locked pathname can
    // create a second inode and split mutual exclusion on Unix.
    _path_lock: File,
}

impl PreparedStorageSyncIntent {
    pub fn intent(&self) -> &StorageSyncIntent {
        &self.intent
    }
}

pub fn prepare(
    project_root: &Path,
    logical_path: &str,
    level: u8,
    bytes: &[u8],
) -> anyhow::Result<PreparedStorageSyncIntent> {
    validate_level(level)?;
    let root = canonical_project_root(project_root)?;
    let logical_path = normalize_logical_path(logical_path)?;
    let id = intent_id(&logical_path);
    let content_sha256 = hex::encode(Sha256::digest(bytes));
    let intent = StorageSyncIntent {
        format_version: STORAGE_SYNC_INTENT_VERSION,
        id: id.clone(),
        logical_path,
        level,
        content_sha256,
        created_unix_ms: now_ms()?,
    };
    validate_intent(&intent)?;

    let locks = lock_dir(&root);
    let pending = pending_dir(&root);
    let ready = ready_dir(&root);
    fs::create_dir_all(&locks)?;
    fs::create_dir_all(&pending)?;
    fs::create_dir_all(&ready)?;

    let path_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
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
}

/// Publish a staged intent after the project file write commits.
///
/// `false` means a newer write to the same logical path superseded this staged
/// intent. That is not an error: the newer pending intent is the state that must
/// eventually be ingested.
pub fn commit(prepared: &PreparedStorageSyncIntent) -> anyhow::Result<bool> {
    let Some(current) = read_intent_if_present(&prepared.pending_path)? else {
        return Ok(false);
    };
    if current != prepared.intent {
        return Ok(false);
    }
    write_intent(&prepared.ready_path, &prepared.intent)?;
    remove_if_same(&prepared.pending_path, &prepared.intent)?;
    Ok(true)
}

/// Remove only this exact staged intent. A concurrent newer write is never
/// deleted by an older failed writer.
pub fn cancel(prepared: &PreparedStorageSyncIntent) -> anyhow::Result<bool> {
    remove_if_same(&prepared.pending_path, &prepared.intent)
}

/// Recover crash-left pending intents, validate ready entries against the
/// current project file, and return the consumable set in Data-Level order.
pub fn ready_intents(project_root: &Path) -> anyhow::Result<Vec<StorageSyncIntent>> {
    let root = canonical_project_root(project_root)?;
    let journal = journal_root(&root);
    if !journal.exists() {
        return Ok(Vec::new());
    }
    recover_pending(&root)?;

    let ready = ready_dir(&root);
    if !ready.exists() {
        return Ok(Vec::new());
    }
    let mut intents = Vec::new();
    for entry in fs::read_dir(&ready)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let intent = read_intent(&path)?;
        validate_filename(&path, &intent)?;
        if source_matches(&root, &intent)? {
            intents.push(intent);
        } else {
            fs::remove_file(path)?;
        }
    }
    intents.sort_by(|left, right| {
        left.level
            .cmp(&right.level)
            .then(left.created_unix_ms.cmp(&right.created_unix_ms))
            .then(left.logical_path.cmp(&right.logical_path))
            .then(left.id.cmp(&right.id))
    });
    Ok(intents)
}

/// Acknowledge only the exact ready intent the consumer actually ingested.
/// If a newer write has already replaced the slot, it remains queued.
pub fn acknowledge(project_root: &Path, intent: &StorageSyncIntent) -> anyhow::Result<bool> {
    validate_intent(intent)?;
    let root = canonical_project_root(project_root)?;
    remove_if_same(
        &ready_dir(&root).join(format!("{}.json", intent.id)),
        intent,
    )
}

pub fn source_path(project_root: &Path, intent: &StorageSyncIntent) -> anyhow::Result<PathBuf> {
    validate_intent(intent)?;
    let root = canonical_project_root(project_root)?;
    let candidate = root.join(&intent.logical_path);
    let resolved = candidate.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "Storage sync source {} could not be resolved: {error}",
            candidate.display()
        )
    })?;
    if !resolved.starts_with(&root) || !resolved.is_file() {
        anyhow::bail!(
            "Storage sync source escapes ProjectRoot or is not a file: {}",
            candidate.display()
        );
    }
    Ok(resolved)
}

fn recover_pending(root: &Path) -> anyhow::Result<()> {
    let pending = pending_dir(root);
    if !pending.exists() {
        return Ok(());
    }
    fs::create_dir_all(ready_dir(root))?;
    for entry in fs::read_dir(&pending)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let intent = read_intent(&path)?;
        validate_filename(&path, &intent)?;
        if source_matches(root, &intent)? {
            write_intent(
                &ready_dir(root).join(format!("{}.json", intent.id)),
                &intent,
            )?;
        }
        remove_if_same(&path, &intent)?;
    }
    Ok(())
}

fn source_matches(root: &Path, intent: &StorageSyncIntent) -> anyhow::Result<bool> {
    let path = match source_path(root, intent) {
        Ok(path) => path,
        Err(_) => return Ok(false),
    };
    Ok(hex::encode(sha256_file(&path)?) == intent.content_sha256)
}

fn remove_if_same(path: &Path, expected: &StorageSyncIntent) -> anyhow::Result<bool> {
    let Some(current) = read_intent_if_present(path)? else {
        return Ok(false);
    };
    if current != *expected {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn write_intent(path: &Path, intent: &StorageSyncIntent) -> anyhow::Result<()> {
    validate_intent(intent)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(intent)?;
    atomic_io::AtomicIo::new().write_atomic(path, &bytes)?;
    Ok(())
}

fn read_intent(path: &Path) -> anyhow::Result<StorageSyncIntent> {
    let bytes = fs::read(path)?;
    let intent: StorageSyncIntent = serde_json::from_slice(&bytes)?;
    validate_intent(&intent)?;
    Ok(intent)
}

fn read_intent_if_present(path: &Path) -> anyhow::Result<Option<StorageSyncIntent>> {
    match fs::read(path) {
        Ok(bytes) => {
            let intent: StorageSyncIntent = serde_json::from_slice(&bytes)?;
            validate_intent(&intent)?;
            Ok(Some(intent))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_intent(intent: &StorageSyncIntent) -> anyhow::Result<()> {
    if intent.format_version != STORAGE_SYNC_INTENT_VERSION {
        anyhow::bail!(
            "unsupported Storage sync intent version {}",
            intent.format_version
        );
    }
    validate_level(intent.level)?;
    let logical = normalize_logical_path(&intent.logical_path)?;
    if logical != intent.logical_path || intent.id != intent_id(&logical) {
        anyhow::bail!("Storage sync intent identity does not match its logical path");
    }
    if intent.content_sha256.len() != 64
        || !intent
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("Storage sync intent contains an invalid SHA-256 digest");
    }
    Ok(())
}

fn validate_filename(path: &Path, intent: &StorageSyncIntent) -> anyhow::Result<()> {
    let expected = format!("{}.json", intent.id);
    if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        anyhow::bail!("Storage sync intent filename does not match its identity");
    }
    Ok(())
}

fn validate_level(level: u8) -> anyhow::Result<()> {
    if !(1..=3).contains(&level) {
        anyhow::bail!("Storage sync Data-Level must be 1, 2, or 3");
    }
    Ok(())
}

fn normalize_logical_path(value: &str) -> anyhow::Result<String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("invalid Storage sync logical path {value:?}");
    }
    let mut segments = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_string_lossy().to_string()),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                anyhow::bail!("Storage sync logical path contains a root, dot, or parent component")
            }
        }
    }
    if segments.is_empty()
        || segments
            .first()
            .is_some_and(|segment| segment.eq_ignore_ascii_case(".rbe"))
    {
        anyhow::bail!("Storage sync logical path targets RBE internal state");
    }
    Ok(segments.join("/"))
}

fn intent_id(logical_path: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(INTENT_ID_DOMAIN);
    digest.update(logical_path.as_bytes());
    hex::encode(digest.finalize())
}

fn canonical_project_root(project_root: &Path) -> anyhow::Result<PathBuf> {
    let root = project_root.canonicalize()?;
    if !root.is_dir() {
        anyhow::bail!("Storage sync ProjectRoot is not a directory");
    }
    Ok(root)
}

fn journal_root(root: &Path) -> PathBuf {
    root.join(".rbe").join("cloud-node").join("storage-ingest")
}

fn lock_dir(root: &Path) -> PathBuf {
    journal_root(root).join("locks")
}

fn pending_dir(root: &Path) -> PathBuf {
    journal_root(root).join("pending")
}

fn ready_dir(root: &Path) -> PathBuf {
    journal_root(root).join("ready")
}

fn sha256_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Storage sync timestamp range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-storage-sync-journal-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn crash_pending_is_promoted_only_when_file_matches() {
        let root = test_root("recover");
        let bytes = br#"{"ok":true}"#;
        let prepared = prepare(&root, "data/state.json", 1, bytes).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("data/state.json"), bytes).unwrap();

        let ready = ready_intents(&root).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].level, 1);
        assert_eq!(ready[0].logical_path, "data/state.json");
        assert!(!prepared.pending_path.exists());
        assert!(acknowledge(&root, &ready[0]).unwrap());
        assert!(ready_intents(&root).unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
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
        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = worker.join().unwrap();
        cancel(&second).unwrap();
        drop(second);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn internal_rbe_paths_are_reserved() {
        let root = test_root("reserved");
        let error = prepare(&root, ".rbe/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(error.to_string().contains("internal state"));
        let mixed_case = prepare(&root, ".RBE/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(mixed_case.to_string().contains("internal state"));
        let _ = fs::remove_dir_all(root);
    }
}
