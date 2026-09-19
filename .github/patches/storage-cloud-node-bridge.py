from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# Shared, process-decoupled Storage -> Cloud Node ingest journal.
# ---------------------------------------------------------------------------
Path("storage-sync-journal/src").mkdir(parents=True, exist_ok=True)
Path("storage-sync-journal/Cargo.toml").write_text(
    '''[package]\nname = "storage-sync-journal"\nversion = "0.1.0"\nedition = "2021"\npublish = false\n\n[dependencies]\natomic-io = { path = "../atomic-io" }\nanyhow = "1"\nhex = "0.4"\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\nsha2 = "0.10"\n''',
    encoding="utf-8",
)
Path("storage-sync-journal/src/lib.rs").write_text(
    r'''//! Durable bridge between capability-bound `storage.write` and Cloud Node ingest.
//!
//! The producer and consumer are different OS processes/workspaces, so this
//! crate deliberately contains only a tiny on-disk contract. It knows nothing
//! about REL, providers, tunnels, or Cloud Node manifests.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone)]
pub struct PreparedStorageSyncIntent {
    intent: StorageSyncIntent,
    pending_path: PathBuf,
    ready_path: PathBuf,
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

    let pending = pending_dir(&root);
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
    remove_if_same(&ready_dir(&root).join(format!("{}.json", intent.id)), intent)
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
            write_intent(&ready_dir(root).join(format!("{}.json", intent.id)), &intent)?;
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
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("Storage sync logical path contains a root, dot, or parent component")
            }
        }
    }
    if segments.is_empty() || segments.first().is_some_and(|segment| segment == ".rbe") {
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
    root.join(".rbe")
        .join("cloud-node")
        .join("storage-ingest")
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
    fn newer_same_path_intent_cannot_be_deleted_by_older_writer() {
        let root = test_root("race");
        fs::create_dir_all(root.join("data")).unwrap();
        let old = prepare(&root, "data/state.json", 3, b"old").unwrap();
        let new = prepare(&root, "data/state.json", 1, b"new").unwrap();
        assert!(!cancel(&old).unwrap());
        fs::write(root.join("data/state.json"), b"new").unwrap();
        assert!(!commit(&old).unwrap());
        assert!(commit(&new).unwrap());
        let ready = ready_intents(&root).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].level, 1);
        assert_eq!(ready[0].content_sha256, hex::encode(Sha256::digest(b"new")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn internal_rbe_paths_are_reserved() {
        let root = test_root("reserved");
        let error = prepare(&root, ".rbe/cloud-node/tamper", 1, b"nope").unwrap_err();
        assert!(error.to_string().contains("internal state"));
        let _ = fs::remove_dir_all(root);
    }
}
''',
    encoding="utf-8",
)

# Cross-workspace dependencies.
replace_once(
    "container-runtime/crates/container-runtime-core/Cargo.toml",
    'atomic-io = { path = "../../../atomic-io" }\n',
    'atomic-io = { path = "../../../atomic-io" }\nstorage-sync-journal = { path = "../../../storage-sync-journal" }\n',
    "Container Storage sync journal dependency",
)
replace_once(
    "engine/crates/cloud-node/Cargo.toml",
    'atomic-io = { path = "../../../atomic-io" }\n',
    'atomic-io = { path = "../../../atomic-io" }\nstorage-sync-journal = { path = "../../../storage-sync-journal" }\n',
    "Cloud Node Storage sync journal dependency",
)

# ---------------------------------------------------------------------------
# Producer: stage intent before the atomic project write, publish after it.
# ---------------------------------------------------------------------------
replace_once(
    "container-runtime/crates/container-runtime-core/src/storage_capability.rs",
    '''    let target = resolve_project_write_path(project_root, &descriptor.path)?;\n    let (bytes, normalized_encoding) =\n        encode_project_write_data(&descriptor.data, &descriptor.encoding)?;\n    atomic_io::AtomicIo::new()\n        .write_atomic(&target, &bytes)\n        .map_err(|_| {\n            error(\n                "CAPABILITY_STORAGE_WRITE_FAILED",\n                "project-root Storage write failed",\n            )\n        })?;\n    Ok((bytes.len(), normalized_encoding))\n''',
    '''    let target = resolve_project_write_path(project_root, &descriptor.path)?;\n    let (bytes, normalized_encoding) =\n        encode_project_write_data(&descriptor.data, &descriptor.encoding)?;\n    let logical_path = descriptor.path.strip_prefix("$$/").ok_or_else(|| {\n        error(\n            "CAPABILITY_STORAGE_PATH_INVALID",\n            "project write path must start with $$/",\n        )\n    })?;\n    let prepared = storage_sync_journal::prepare(\n        project_root,\n        logical_path,\n        descriptor.level,\n        &bytes,\n    )\n    .map_err(|_| {\n        error(\n            "CAPABILITY_STORAGE_WRITE_FAILED",\n            "project-root Storage sync intent could not be staged",\n        )\n    })?;\n    if atomic_io::AtomicIo::new().write_atomic(&target, &bytes).is_err() {\n        let _ = storage_sync_journal::cancel(&prepared);\n        return Err(error(\n            "CAPABILITY_STORAGE_WRITE_FAILED",\n            "project-root Storage write failed",\n        ));\n    }\n    storage_sync_journal::commit(&prepared).map_err(|_| {\n        error(\n            "CAPABILITY_STORAGE_WRITE_FAILED",\n            "project-root Storage sync intent could not be published",\n        )\n    })?;\n    Ok((bytes.len(), normalized_encoding))\n''',
    "journal project write",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/storage_capability.rs",
    '''    let Some(file_name) = segments.pop() else {\n        return Err(error(\n            "CAPABILITY_STORAGE_PATH_INVALID",\n            "project write path must name a file",\n        ));\n    };\n''',
    '''    if segments\n        .first()\n        .is_some_and(|segment| segment.to_string_lossy() == ".rbe")\n    {\n        return Err(error(\n            "CAPABILITY_STORAGE_PATH_INVALID",\n            "project write path targets RBE internal state",\n        ));\n    }\n\n    let Some(file_name) = segments.pop() else {\n        return Err(error(\n            "CAPABILITY_STORAGE_PATH_INVALID",\n            "project write path must name a file",\n        ));\n    };\n''',
    "reserve internal RBE project state",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/storage_capability.rs",
    '''        assert_eq!(saved["name"], "Kate");\n        assert_eq!(saved["active"], true);\n        let _ = std::fs::remove_dir_all(root);\n''',
    '''        assert_eq!(saved["name"], "Kate");\n        assert_eq!(saved["active"], true);\n        let intents = storage_sync_journal::ready_intents(&root).unwrap();\n        assert_eq!(intents.len(), 1);\n        assert_eq!(intents[0].logical_path, "data/users/kate.json");\n        assert_eq!(intents[0].level, 1);\n        let _ = std::fs::remove_dir_all(root);\n''',
    "project write journal regression",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/storage_capability.rs",
    '''        assert_eq!(invalid_level.code, "CAPABILITY_STORAGE_ARGS_INVALID");\n        assert!(!root.join("data/nope.txt").exists());\n        let _ = std::fs::remove_dir_all(root);\n''',
    '''        assert_eq!(invalid_level.code, "CAPABILITY_STORAGE_ARGS_INVALID");\n        assert!(!root.join("data/nope.txt").exists());\n\n        let internal = dispatch_project(\n            &storage,\n            &root,\n            &target,\n            "write",\n            json!([{\n                "path":"$$/.rbe/cloud-node/tamper.json",\n                "data":"nope",\n                "encoding":"UTF8",\n                "level":1\n            }]),\n        )\n        .unwrap_err();\n        assert_eq!(internal.code, "CAPABILITY_STORAGE_PATH_INVALID");\n        let _ = std::fs::remove_dir_all(root);\n''',
    "project write internal path rejection",
)

# ---------------------------------------------------------------------------
# Consumer and local priority metadata. Priority is deliberately NOT part of
# BlobManifest v1 or the canonical sync root.
# ---------------------------------------------------------------------------
Path("engine/crates/cloud-node/src/ingest.rs").write_text(
    r'''use std::path::Path;

use crate::store::CloudNodeStore;

impl CloudNodeStore {
    /// Consume durable project-write intents into Cloud Node's content-addressed
    /// store. Replay is idempotent; ACK removes only the exact intent consumed.
    pub fn ingest_storage_journal(&self, project_root: &Path) -> anyhow::Result<usize> {
        let intents = storage_sync_journal::ready_intents(project_root)?;
        let mut consumed = 0usize;
        for intent in intents {
            let source = storage_sync_journal::source_path(project_root, &intent)?;
            let stored = self.store_file_with_priority(&source, &intent.logical_path, intent.level)?;
            if stored.content_sha256 != intent.content_sha256 {
                // The project file changed after the journal scan. Never ACK a
                // version we did not ingest; the next pass will reconcile it.
                continue;
            }
            if storage_sync_journal::acknowledge(project_root, &intent)? {
                consumed = consumed.saturating_add(1);
            }
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::CloudNodeSettings;

    fn root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-cn-storage-ingest-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "node": {
                "id": "ingest-test",
                "mode": "primary",
                "storageRoot": root.join("cloud").to_string_lossy(),
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            },
            "replication": { "targets": [] }
        }))
        .unwrap()
    }

    #[test]
    fn ingest_is_idempotent_and_preserves_data_level_outside_sync_identity() {
        let project = root("project");
        let source = project.join("data/state.json");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let bytes = br#"{"state":"critical"}"#;
        let prepared = storage_sync_journal::prepare(&project, "data/state.json", 1, bytes).unwrap();
        fs::write(&source, bytes).unwrap();
        storage_sync_journal::commit(&prepared).unwrap();

        let cloud_root = root("store");
        let store = CloudNodeStore::open(&settings(&cloud_root)).unwrap();
        assert_eq!(store.ingest_storage_journal(&project).unwrap(), 1);
        assert_eq!(store.ingest_storage_journal(&project).unwrap(), 0);

        let plan = store.sync_plan().unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].priority, 1);
        assert_eq!(plan.files[0].content_sha256, Sha256::digest(bytes).into());
        let root_before = plan.root_sha256;
        store
            .store_file_with_priority(&source, "data/state.json", 3)
            .unwrap();
        let reprioritized = store.sync_plan().unwrap();
        assert_eq!(reprioritized.files[0].priority, 3);
        assert_eq!(reprioritized.root_sha256, root_before);

        let _ = fs::remove_dir_all(project);
        let _ = fs::remove_dir_all(cloud_root);
    }
}
''',
    encoding="utf-8",
)
replace_once(
    "engine/crates/cloud-node/src/lib.rs",
    "mod format;\nmod protocol;",
    "mod format;\nmod ingest;\nmod protocol;",
    "Cloud Node ingest module",
)
replace_once(
    "engine/crates/cloud-node/src/store.rs",
    '''    pub fn store_file(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {\n        self.store_regular(source, logical_path, BlobKind::File)\n    }\n\n    pub fn store_video(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {\n''',
    '''    pub fn store_file(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {\n        self.store_regular(source, logical_path, BlobKind::File)\n    }\n\n    pub fn store_file_with_priority(\n        &self,\n        source: &Path,\n        logical_path: &str,\n        level: u8,\n    ) -> anyhow::Result<StoredObject> {\n        validate_replication_priority(level)?;\n        let stored = self.store_regular(source, logical_path, BlobKind::File)?;\n        self.set_replication_priority(&stored.object_key, level)?;\n        Ok(stored)\n    }\n\n    pub(crate) fn replication_priority(&self, object_key: &[u8; 32]) -> anyhow::Result<u8> {\n        let path = self\n            .root\n            .join("priority")\n            .join(format!("{}.level", hex::encode(object_key)));\n        let bytes = match fs::read(&path) {\n            Ok(bytes) => bytes,\n            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(2),\n            Err(error) => return Err(error.into()),\n        };\n        if bytes.len() != 1 {\n            anyhow::bail!(\n                "Cloud Node replication priority sidecar is malformed: {}",\n                path.display()\n            );\n        }\n        validate_replication_priority(bytes[0])?;\n        Ok(bytes[0])\n    }\n\n    fn set_replication_priority(&self, object_key: &str, level: u8) -> anyhow::Result<()> {\n        validate_replication_priority(level)?;\n        if object_key.len() != 64 || !object_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {\n            anyhow::bail!("Cloud Node object key is invalid for replication priority metadata");\n        }\n        let path = self\n            .root\n            .join("priority")\n            .join(format!("{}.level", object_key.to_ascii_lowercase()));\n        atomic_write(&path, &[level])\n    }\n\n    pub fn store_video(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {\n''',
    "Cloud Node file priority API",
)
# Add validator near logical path validation.
replace_once(
    "engine/crates/cloud-node/src/store.rs",
    "\nfn normalize_logical_path(value: &str) -> anyhow::Result<String> {",
    '''\nfn validate_replication_priority(level: u8) -> anyhow::Result<()> {\n    if !(1..=3).contains(&level) {\n        anyhow::bail!("Cloud Node replication priority must be 1, 2, or 3");\n    }\n    Ok(())\n}\n\nfn normalize_logical_path(value: &str) -> anyhow::Result<String> {''',
    "Cloud Node priority validator",
)

# Sync plan: retain canonical content/root order, expose priority scheduling order.
replace_once(
    "engine/crates/cloud-node/src/sync.rs",
    '''    pub logical_size: u64,\n    pub manifest_path: PathBuf,\n''',
    '''    pub logical_size: u64,\n    /// Local scheduling metadata only; intentionally excluded from sync-root identity.\n    pub priority: u8,\n    pub manifest_path: PathBuf,\n''',
    "SyncObject priority field",
)
replace_once(
    "engine/crates/cloud-node/src/sync.rs",
    '''    pub fn scan(store: &CloudNodeStore) -> anyhow::Result<Self> {\n        let storage = store.summary().storage;\n        Self::scan_storage(&storage)\n    }\n''',
    '''    pub fn scan(store: &CloudNodeStore) -> anyhow::Result<Self> {\n        let storage = store.summary().storage;\n        let mut plan = Self::scan_storage(&storage)?;\n        for object in &mut plan.folders {\n            object.priority = store.replication_priority(&object.object_key)?;\n        }\n        for object in &mut plan.videos {\n            object.priority = store.replication_priority(&object.object_key)?;\n        }\n        for object in &mut plan.files {\n            object.priority = store.replication_priority(&object.object_key)?;\n        }\n        Ok(plan)\n    }\n''',
    "apply local priorities to sync plan",
)
replace_once(
    "engine/crates/cloud-node/src/sync.rs",
    '''    pub fn ordered(&self) -> impl Iterator<Item = &SyncObject> {\n        self.folders\n            .iter()\n            .chain(self.videos.iter())\n            .chain(self.files.iter())\n    }\n\n    pub fn header(&self) -> anyhow::Result<SyncPlanHeader> {\n''',
    '''    pub fn ordered(&self) -> impl Iterator<Item = &SyncObject> {\n        self.folders\n            .iter()\n            .chain(self.videos.iter())\n            .chain(self.files.iter())\n    }\n\n    /// Outbound replication order. Required topology phases remain\n    /// folder -> video -> file, while Data-Level 1 precedes 2 and 3 inside\n    /// each phase. `storage.write` objects are files, so their levels order\n    /// directly against one another without changing canonical snapshot identity.\n    pub fn priority_ordered(&self) -> impl Iterator<Item = &SyncObject> {\n        let sort = |objects: &[SyncObject]| {\n            let mut values = objects.iter().collect::<Vec<_>>();\n            values.sort_by(|left, right| {\n                left.priority\n                    .cmp(&right.priority)\n                    .then(left.logical_path.cmp(&right.logical_path))\n                    .then(left.object_key.cmp(&right.object_key))\n            });\n            values\n        };\n        sort(&self.folders)\n            .into_iter()\n            .chain(sort(&self.videos))\n            .chain(sort(&self.files))\n    }\n\n    pub fn header(&self) -> anyhow::Result<SyncPlanHeader> {\n''',
    "priority replication order",
)
replace_once(
    "engine/crates/cloud-node/src/sync.rs",
    '''        logical_path: manifest.logical_path,\n        logical_size: manifest.logical_size,\n        manifest_path,\n''',
    '''        logical_path: manifest.logical_path,\n        logical_size: manifest.logical_size,\n        priority: 2,\n        manifest_path,\n''',
    "default sync priority",
)

# Peer and provider outbound transfers honor local Data-Level without changing
# the canonical plan used for root/hash/recovery verification.
replace_once(
    "engine/crates/cloud-node/src/client.rs",
    '''    let client = http_client()?;\n    for object in plan.ordered() {\n''',
    '''    let client = http_client()?;\n    for object in plan.priority_ordered() {\n''',
    "peer priority transfer order",
)
replace_once(
    "engine/crates/cloud-node/src/provider_sync.rs",
    '''    let mut resources = Vec::new();\n    for object in plan.ordered() {\n''',
    '''    let mut resources = Vec::new();\n    for object in plan.priority_ordered() {\n''',
    "provider priority upload order",
)
replace_once(
    "engine/crates/cloud-node/src/provider_sync.rs",
    '''                logical_path: "db/users.db".into(),\n                logical_size: 0,\n                manifest_path: PathBuf::from("unused/file.blob.cn"),\n''',
    '''                logical_path: "db/users.db".into(),\n                logical_size: 0,\n                priority: 2,\n                manifest_path: PathBuf::from("unused/file.blob.cn"),\n''',
    "provider test SyncObject priority",
)

# Add a sync-plan regression proving priority does not mutate identity and is
# applied inside the file phase.
sync_path = Path("engine/crates/cloud-node/src/sync.rs")
sync_text = sync_path.read_text(encoding="utf-8")
insert_anchor = "\n    #[test]\n    fn sync_header_round_trip() {"
insert_at = sync_text.find(insert_anchor)
if insert_at < 0:
    raise SystemExit("sync priority test insertion anchor missing")
priority_test = r'''

    #[test]
    fn data_level_orders_files_without_changing_sync_root() {
        let root = test_root();
        let settings = settings(&root);
        let store = CloudNodeStore::open(&settings).unwrap();
        let high = root.join("high.json");
        let low = root.join("low.json");
        fs::write(&high, b"high").unwrap();
        fs::write(&low, b"low").unwrap();
        store.store_file_with_priority(&low, "data/low.json", 3).unwrap();
        store.store_file_with_priority(&high, "data/high.json", 1).unwrap();

        let plan = store.sync_plan().unwrap();
        let root_before = plan.root_sha256;
        let files = plan
            .priority_ordered()
            .filter(|object| object.kind == BlobKind::File)
            .map(|object| (object.logical_path.clone(), object.priority))
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![("data/high.json".into(), 1), ("data/low.json".into(), 3)]
        );

        store.store_file_with_priority(&high, "data/high.json", 3).unwrap();
        assert_eq!(store.sync_plan().unwrap().root_sha256, root_before);
        let _ = fs::remove_dir_all(root);
    }
'''
sync_text = sync_text[:insert_at] + priority_test + sync_text[insert_at:]
sync_path.write_text(sync_text, encoding="utf-8")

# ---------------------------------------------------------------------------
# cloud_node consumes project-write intents before evaluating/syncing. Explicit
# RBE_PROJECT_ROOT wins; otherwise the settings file's parent is the project root.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/cloud-node/src/main.rs",
    '''    let settings = CloudNodeSettings::load(&config_path)?;\n    let store = CloudNodeStore::open(&settings)?;\n    match command {\n''',
    '''    let settings = CloudNodeSettings::load(&config_path)?;\n    let store = CloudNodeStore::open(&settings)?;\n    let project_root = cloud_node_project_root(&config_path)?;\n    let ingested = store.ingest_storage_journal(&project_root)?;\n    if ingested > 0 {\n        eprintln!(\n            "cloud_node: ingested {ingested} project-root Storage write(s) from {}",\n            project_root.display()\n        );\n    }\n    match command {\n''',
    "Cloud Node journal ingestion",
)
replace_once(
    "engine/crates/cloud-node/src/main.rs",
    "\nfn default_config_path() -> PathBuf {",
    r'''
fn cloud_node_project_root(config_path: &Path) -> anyhow::Result<PathBuf> {
    let candidate = std::env::var_os("RBE_PROJECT_ROOT")
        .map(PathBuf::from)
        .or_else(|| config_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let root = candidate.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "Cloud Node ProjectRoot {} could not be canonicalized: {error}",
            candidate.display()
        )
    })?;
    if !root.is_dir() {
        anyhow::bail!("Cloud Node ProjectRoot is not a directory: {}", root.display());
    }
    Ok(root)
}

fn default_config_path() -> PathBuf {''',
    "Cloud Node ProjectRoot resolver",
)

# Documentation now reflects the implemented bridge while keeping local priority
# separate from canonical content identity.
replace_once(
    "doc/storage.md",
    '''**Current implementation boundary:** Storage validates and carries the level through the trusted write request and response, but Cloud Node does **not yet automatically ingest `$$` project writes or schedule sync by this level**. Cloud Node currently receives objects only through its own ingest/store paths. Until the Storage-to-Cloud-Node bridge is implemented, Data-Level must not be described as active replication scheduling.\n\nThere is currently no public `durability[...]` descriptor, and `$env/` / `$tmp/` roots are not part of the locked Storage language contract.\n''',
    '''**Current implementation:** successful project-root writes are bridged to Cloud Node through a durable internal outbox under `.rbe/cloud-node/storage-ingest`. The write intent is staged before the atomic file replacement and published after it, so crash recovery can verify the expected content hash before ingestion. Cloud Node ingests the current file idempotently, records Data-Level as local scheduling metadata, and acknowledges only the exact intent it consumed.\n\nData-Level does **not** change `BlobManifest` v1 or the canonical sync-root hash. Replication keeps the required folder -> video -> file topology phases, then orders objects by level (`1`, `2`, `3`) inside each phase. Because `storage.write` produces regular file objects, Level 1 project writes are transferred before Level 2/3 project writes. Provider and peer uploads use the same priority ordering.\n\nThe `.rbe/` project subtree is reserved for RBE internal state and cannot be targeted by `storage.write`. `RBE_PROJECT_ROOT` can explicitly tell `cloud_node` where to consume the outbox; otherwise the parent directory of `setting.node.cn.json` is used.\n\nThere is currently no public `durability[...]` descriptor, and `$env/` / `$tmp/` roots are not part of the locked Storage language contract.\n''',
    "Storage Data-Level implementation docs",
)
