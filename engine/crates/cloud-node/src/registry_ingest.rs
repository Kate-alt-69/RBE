use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use fs2::FileExt;

#[cfg(feature = "client")]
use crate::provider_sync::{synchronize_provider, ProviderSyncResult};
#[cfg(feature = "client")]
use crate::CloudNodeSettings;
use crate::{durable, BlobKind, CloudNodeStore, StoredObject, SyncObject, SyncPlan};

const REGISTRY_LOGICAL_ROOT: &str = "registry";
const REQUIRED_INDEX_SNAPSHOT: &str = "index/snapshot.json";
const REGISTRY_INGEST_LOCK: &str = ".registry-ingest.lock";
const REGISTRY_INGEST_PENDING: &str = ".registry-ingest.pending";
const REGISTRY_INGEST_MARKER: &[u8] = b"RBE-CN-REGISTRY-INGEST/1\n";
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "index",
    "packages",
    "ownership",
    "releases",
    "history",
    "analytics",
    "artifacts",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryIngestResult {
    pub files: usize,
    pub metadata_files: usize,
    pub artifact_files: usize,
    pub removed_files: usize,
    pub stored: Vec<RegistryStoredObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryStoredObject {
    pub logical_path: String,
    pub object_key: String,
    pub content_sha256: String,
}

#[cfg(feature = "client")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySyncResult {
    pub ingest: RegistryIngestResult,
    pub provider: ProviderSyncResult,
}

#[derive(Debug)]
struct ValidatedRegistryFile {
    source: PathBuf,
    logical_path: String,
    is_artifact: bool,
}

struct RegistryIngestLock {
    file: fs::File,
}

impl RegistryIngestLock {
    fn acquire(store: &CloudNodeStore) -> anyhow::Result<Self> {
        let path = store.summary().root.join(REGISTRY_INGEST_LOCK);
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to open Cloud Node registry ingest lock {}: {error}",
                    path.display()
                )
            })?;
        file.try_lock_exclusive().map_err(|error| {
            anyhow::anyhow!(
                "Cloud Node registry ingest is already active, or its lock is unavailable: {error}"
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for RegistryIngestLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Refuse to publish or negotiate a Cloud Node snapshot while a previous
/// registry replacement is known to have stopped before its commit point.
pub(crate) fn ensure_registry_ingest_committed(store: &CloudNodeStore) -> anyhow::Result<()> {
    let marker = registry_pending_path(store);
    if marker.exists() {
        anyhow::bail!(
            "Cloud Node registry replacement is incomplete at {}; run ingest-registry or sync-registry again with a complete trusted export before synchronization",
            marker.display()
        );
    }
    Ok(())
}

/// Ingest a complete trusted Kastrick/RPX registry export into Cloud Node's
/// normal content-addressed store.
///
/// The export is treated as a frozen snapshot, not an overlay: active
/// `registry/` objects that are absent from the validated export are removed
/// after all replacement files have been stored. Cloud Node backup history is
/// intentionally retained.
///
/// Only one local registry replacement transaction may run at a time. The OS
/// releases the lock automatically if the owning process exits or crashes. A
/// durable pending marker is written before active storage mutation and is
/// removed only after the final active registry path set has been verified.
///
/// The resulting objects participate in the existing provider snapshot and
/// revision-history model, so registry state gets the same local-ahead/push,
/// remote-ahead/pull, and equal/no-op behavior as every other Cloud Node
/// object. Published `.rbe.zip` artifacts are treated as opaque immutable
/// bytes here; package archive and `package.rbe.yaml` verification belongs to
/// the trusted registry publisher before the export reaches Cloud Node.
pub fn ingest_registry_export(
    store: &CloudNodeStore,
    export_root: &Path,
) -> anyhow::Result<RegistryIngestResult> {
    let _lock = RegistryIngestLock::acquire(store)?;
    ingest_registry_export_locked(store, export_root)
}

/// Replace the local registry snapshot and synchronize it through the normal
/// provider-history transaction while holding the registry ingest lock for the
/// complete operation. Package semantics remain outside Cloud Node.
#[cfg(feature = "client")]
pub async fn synchronize_registry_export(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    export_root: &Path,
) -> anyhow::Result<RegistrySyncResult> {
    if settings.provider.is_none() {
        anyhow::bail!("registry synchronization requires Cloud Node provider mode");
    }
    let _lock = RegistryIngestLock::acquire(store)?;
    let ingest = ingest_registry_export_locked(store, export_root)?;
    let provider = synchronize_provider(settings, store).await.map_err(|error| {
        anyhow::anyhow!(
            "registry export was ingested locally but provider synchronization failed: {error:#}"
        )
    })?;
    Ok(RegistrySyncResult { ingest, provider })
}

fn ingest_registry_export_locked(
    store: &CloudNodeStore,
    export_root: &Path,
) -> anyhow::Result<RegistryIngestResult> {
    if !export_root.is_dir() {
        anyhow::bail!(
            "Cloud Node registry export root is not a directory: {}",
            export_root.display()
        );
    }

    let mut files = Vec::new();
    collect_registry_files(export_root, export_root, &mut files)?;
    files.sort();

    // Validate the complete export before mutating Cloud Node state. This
    // prevents a malformed trailing path from leaving a partially replaced
    // registry snapshot behind.
    let mut validated = Vec::with_capacity(files.len());
    let mut expected_logical_paths = BTreeSet::new();
    let mut has_index_snapshot = false;
    for source in files {
        let relative = source
            .strip_prefix(export_root)
            .map_err(|_| anyhow::anyhow!("Cloud Node registry export path escaped its root"))?;
        let normalized = validate_registry_relative_path(relative)?;
        if normalized == REQUIRED_INDEX_SNAPSHOT {
            has_index_snapshot = true;
        }
        let logical_path = format!("{REGISTRY_LOGICAL_ROOT}/{normalized}");
        if !expected_logical_paths.insert(logical_path.clone()) {
            anyhow::bail!(
                "Cloud Node registry export contains duplicate logical path {logical_path:?}"
            );
        }
        validated.push(ValidatedRegistryFile {
            source,
            logical_path,
            is_artifact: normalized.ends_with(".rbe.zip"),
        });
    }
    if !has_index_snapshot {
        anyhow::bail!(
            "Cloud Node registry export is incomplete: required {REQUIRED_INDEX_SNAPSHOT} is missing"
        );
    }

    begin_registry_transaction(store)?;

    // A previous process may have died with a pending marker. Scan the active
    // storage directly while repairing it; public sync-plan generation remains
    // fail-closed until this transaction reaches its commit point.
    let before = SyncPlan::scan_storage(&store.summary().storage)?;

    let mut result = RegistryIngestResult::default();
    for file in validated {
        let stored = store.store_file(&file.source, &file.logical_path)?;
        result.files += 1;
        if file.is_artifact {
            result.artifact_files += 1;
        } else {
            result.metadata_files += 1;
        }
        result
            .stored
            .push(stored_registry_object(file.logical_path, stored));
    }

    let stale = before
        .folders
        .into_iter()
        .chain(before.videos)
        .chain(before.files)
        .filter(|object| {
            object.logical_path.starts_with("registry/")
                && (object.kind != BlobKind::File
                    || !expected_logical_paths.contains(&object.logical_path))
        })
        .collect::<Vec<_>>();
    for object in stale {
        remove_active_registry_object(store, &object)?;
        result.removed_files += 1;
    }

    verify_active_registry_paths(store, &expected_logical_paths)?;
    finish_registry_transaction(store)?;
    Ok(result)
}

fn begin_registry_transaction(store: &CloudNodeStore) -> anyhow::Result<()> {
    let marker = registry_pending_path(store);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&marker)
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to create Cloud Node registry transaction marker {}: {error}",
                marker.display()
            )
        })?;
    file.write_all(REGISTRY_INGEST_MARKER)?;
    file.sync_all()?;
    durable::sync_parent(&marker)?;
    Ok(())
}

fn finish_registry_transaction(store: &CloudNodeStore) -> anyhow::Result<()> {
    let marker = registry_pending_path(store);
    if marker.exists() {
        durable::remove_file(&marker)?;
    }
    Ok(())
}

fn registry_pending_path(store: &CloudNodeStore) -> PathBuf {
    store.summary().root.join(REGISTRY_INGEST_PENDING)
}

fn verify_active_registry_paths(
    store: &CloudNodeStore,
    expected: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let plan = SyncPlan::scan_storage(&store.summary().storage)?;
    let mut actual = BTreeSet::new();
    for object in plan
        .folders
        .into_iter()
        .chain(plan.videos)
        .chain(plan.files)
        .filter(|object| object.logical_path.starts_with("registry/"))
    {
        if object.kind != BlobKind::File {
            anyhow::bail!(
                "Cloud Node registry snapshot contains non-file active object {:?}",
                object.logical_path
            );
        }
        actual.insert(object.logical_path);
    }
    if &actual != expected {
        anyhow::bail!(
            "Cloud Node registry snapshot path verification failed: expected {} active paths, found {}",
            expected.len(),
            actual.len()
        );
    }
    Ok(())
}

fn stored_registry_object(logical_path: String, stored: StoredObject) -> RegistryStoredObject {
    RegistryStoredObject {
        logical_path,
        object_key: stored.object_key,
        content_sha256: stored.content_sha256,
    }
}

fn remove_active_registry_object(store: &CloudNodeStore, object: &SyncObject) -> anyhow::Result<()> {
    if !object.logical_path.starts_with("registry/") {
        anyhow::bail!(
            "refusing to remove non-registry Cloud Node object {:?}",
            object.logical_path
        );
    }
    let summary = store.summary();
    let object_dir = object
        .manifest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("registry object manifest has no parent directory"))?;
    if object_dir.parent() != Some(summary.storage.as_path()) {
        anyhow::bail!(
            "registry object directory escaped Cloud Node storage root: {}",
            object_dir.display()
        );
    }

    // Removing the active object makes it disappear from the next sync root.
    // The backup tree is deliberately preserved so retained local history is
    // not destroyed merely because a later complete registry snapshot no
    // longer exposes this path.
    durable::remove_dir_all(object_dir)?;

    let priority = summary
        .root
        .join("priority")
        .join(format!("{}.level", hex::encode(object.object_key)));
    if priority.exists() {
        durable::remove_file(priority)?;
    }
    Ok(())
}

fn collect_registry_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(current).map_err(|error| {
        anyhow::anyhow!(
            "failed to read Cloud Node registry export directory {}: {error}",
            current.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Cloud Node registry export cannot contain symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_registry_files(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            anyhow::bail!(
                "Cloud Node registry export contains a non-regular entry: {}",
                path.display()
            );
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow::anyhow!("Cloud Node registry export path escaped its root"))?;
        validate_registry_relative_path(relative)?;
        files.push(path);
    }
    Ok(())
}

fn validate_registry_relative_path(path: &Path) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Cloud Node registry paths must be valid UTF-8")
                })?;
                if value.is_empty() || value == "." || value == ".." || value.contains('\\') {
                    anyhow::bail!("Cloud Node registry path contains an unsafe segment");
                }
                parts.push(value.to_owned());
            }
            _ => anyhow::bail!("Cloud Node registry paths must be relative and normalized"),
        }
    }

    if parts.len() < 2 {
        anyhow::bail!("Cloud Node registry files must live below a recognized registry collection");
    }
    if !ALLOWED_TOP_LEVEL.contains(&parts[0].as_str()) {
        anyhow::bail!(
            "Cloud Node registry collection {:?} is not supported",
            parts[0]
        );
    }
    if parts.last().is_some_and(|name| name == "package.rbe.json") {
        anyhow::bail!(
            "package.rbe.json is an RBE application/project manifest, not registry archive metadata"
        );
    }

    let normalized = parts.join("/");
    let is_json = normalized.ends_with(".json");
    let is_artifact = normalized.ends_with(".rbe.zip");
    if parts[0] == "artifacts" {
        if !is_json && !is_artifact {
            anyhow::bail!(
                "Cloud Node registry artifacts may contain only metadata .json and .rbe.zip payloads"
            );
        }
    } else if !is_json {
        anyhow::bail!("Cloud Node registry metadata collections may contain only .json files");
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudNodeSettings;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cn-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": "registry-test",
                "storageRoot": root,
                "backupVersions": 5,
                "preserveOriginal": true
            }
        }))
        .unwrap()
    }

    fn write_export_file(export: &Path, relative: &str, bytes: &[u8]) {
        let path = export.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn registry_ingest_accepts_all_frozen_server_collections() {
        let root = temp_root("layout");
        let export = root.join("export");
        let files = [
            ("index/snapshot.json", b"{}".as_slice()),
            ("packages/demo.json", b"{}".as_slice()),
            ("ownership/demo.json", b"{}".as_slice()),
            ("releases/demo/1.0.0.json", b"{}".as_slice()),
            ("history/demo/publish-1.json", b"{}".as_slice()),
            ("analytics/demo/snapshot-1.json", b"{}".as_slice()),
            ("artifacts/demo/1.0.0/metadata.json", b"{}".as_slice()),
            (
                "artifacts/demo/1.0.0/demo.rbe.zip",
                b"PK\x03\x04".as_slice(),
            ),
        ];
        for (relative, bytes) in files {
            write_export_file(&export, relative, bytes);
        }

        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let result = ingest_registry_export(&store, &export).unwrap();
        assert_eq!(result.files, 8);
        assert_eq!(result.metadata_files, 7);
        assert_eq!(result.artifact_files, 1);
        assert_eq!(result.removed_files, 0);
        assert!(!registry_pending_path(&store).exists());
        assert!(result
            .stored
            .iter()
            .any(|stored| stored.logical_path == "registry/index/snapshot.json"));
        assert!(result
            .stored
            .iter()
            .any(|stored| stored.logical_path.ends_with("demo.rbe.zip")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_ingest_lock_rejects_second_local_writer() {
        let root = temp_root("lock");
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let first = RegistryIngestLock::acquire(&store).unwrap();
        assert!(RegistryIngestLock::acquire(&store).is_err());
        drop(first);
        assert!(RegistryIngestLock::acquire(&store).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_registry_transaction_is_repaired_by_next_complete_ingest() {
        let root = temp_root("pending");
        let export = root.join("export");
        write_export_file(&export, REQUIRED_INDEX_SNAPSHOT, b"{}");
        let store = CloudNodeStore::open(&settings(&root)).unwrap();

        begin_registry_transaction(&store).unwrap();
        assert!(ensure_registry_ingest_committed(&store).is_err());
        assert!(registry_pending_path(&store).is_file());

        ingest_registry_export(&store, &export).unwrap();
        assert!(!registry_pending_path(&store).exists());
        assert!(ensure_registry_ingest_committed(&store).is_ok());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaying_same_registry_export_keeps_sync_root_identity() {
        let root = temp_root("replay");
        let export = root.join("export");
        let snapshot = export.join(REQUIRED_INDEX_SNAPSHOT);
        write_export_file(&export, REQUIRED_INDEX_SNAPSHOT, b"{\"revision\":\"sha256:one\"}");

        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        ingest_registry_export(&store, &export).unwrap();
        let first_root = store.sync_plan().unwrap().root_sha256;

        ingest_registry_export(&store, &export).unwrap();
        let replay_root = store.sync_plan().unwrap().root_sha256;
        assert_eq!(replay_root, first_root);

        fs::write(&snapshot, b"{\"revision\":\"sha256:two\"}").unwrap();
        ingest_registry_export(&store, &export).unwrap();
        assert_ne!(store.sync_plan().unwrap().root_sha256, first_root);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_registry_snapshot_removes_stale_active_paths_only() {
        let root = temp_root("replace");
        let export = root.join("export");
        write_export_file(&export, REQUIRED_INDEX_SNAPSHOT, b"{\"revision\":1}");
        write_export_file(&export, "packages/old.json", b"{\"name\":\"old\"}");

        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let unrelated = root.join("outside.txt");
        fs::write(&unrelated, b"keep me").unwrap();
        store.store_file(&unrelated, "app/outside.txt").unwrap();

        let first = ingest_registry_export(&store, &export).unwrap();
        let old = first
            .stored
            .iter()
            .find(|stored| stored.logical_path == "registry/packages/old.json")
            .unwrap();
        let old_backup = store.summary().backup.join(&old.object_key);
        assert!(old_backup.is_dir());

        fs::remove_file(export.join("packages/old.json")).unwrap();
        fs::write(export.join(REQUIRED_INDEX_SNAPSHOT), b"{\"revision\":2}").unwrap();
        let second = ingest_registry_export(&store, &export).unwrap();
        assert_eq!(second.removed_files, 1);

        let paths = store
            .sync_plan()
            .unwrap()
            .files
            .into_iter()
            .map(|object| object.logical_path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("registry/index/snapshot.json"));
        assert!(!paths.contains("registry/packages/old.json"));
        assert!(paths.contains("app/outside.txt"));
        assert!(old_backup.is_dir());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_registry_snapshot_removes_non_file_registry_objects() {
        let root = temp_root("wrong-kind");
        let export = root.join("export");
        write_export_file(&export, REQUIRED_INDEX_SNAPSHOT, b"{}");
        let store = CloudNodeStore::open(&settings(&root)).unwrap();

        let video = root.join("stale.mp4");
        fs::write(&video, b"not really video").unwrap();
        store.store_video(&video, "registry/stale.mp4").unwrap();

        let result = ingest_registry_export(&store, &export).unwrap();
        assert_eq!(result.removed_files, 1);
        assert!(store
            .sync_plan()
            .unwrap()
            .ordered()
            .all(|object| object.logical_path != "registry/stale.mp4"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_snapshot_is_rejected_before_existing_registry_state_changes() {
        let root = temp_root("incomplete");
        let complete = root.join("complete");
        write_export_file(&complete, REQUIRED_INDEX_SNAPSHOT, b"{}");
        write_export_file(&complete, "packages/keep.json", b"{\"name\":\"keep\"}");

        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        ingest_registry_export(&store, &complete).unwrap();
        let before = store.sync_plan().unwrap().root_sha256;

        let incomplete = root.join("incomplete");
        write_export_file(&incomplete, "packages/new.json", b"{\"name\":\"new\"}");
        assert!(ingest_registry_export(&store, &incomplete).is_err());
        assert_eq!(store.sync_plan().unwrap().root_sha256, before);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_ingest_rejects_project_manifest_and_unknown_collections() {
        assert!(validate_registry_relative_path(Path::new("packages/package.rbe.json")).is_err());
        assert!(validate_registry_relative_path(Path::new("other/demo.json")).is_err());
        assert!(validate_registry_relative_path(Path::new(
            "artifacts/demo/1.0.0/package.rbe.yaml"
        ))
        .is_err());
    }

    #[test]
    fn registry_ingest_rejects_non_json_metadata_and_non_rbe_artifacts() {
        assert!(validate_registry_relative_path(Path::new("packages/demo.yaml")).is_err());
        assert!(
            validate_registry_relative_path(Path::new("artifacts/demo/1.0.0/demo.zip")).is_err()
        );
        validate_registry_relative_path(Path::new("artifacts/demo/1.0.0/demo.rbe.zip")).unwrap();
    }
}
