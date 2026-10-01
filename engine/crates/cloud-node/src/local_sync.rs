use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::durable;
use crate::{BlobKind, CloudNodeStore, SyncObject};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_LOGICAL_PREFIX: &str = "local";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LocalSyncSettings {
    pub directory: PathBuf,
    #[serde(default = "default_logical_prefix")]
    pub logical_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDirectoryStatus {
    pub dirty: bool,
    pub scanned_files: usize,
    pub managed_objects: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalDirectorySyncResult {
    pub scanned_files: usize,
    pub stored: usize,
    pub removed: usize,
    pub unchanged: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalDirectoryRestoreResult {
    pub restored: usize,
    pub removed: usize,
    pub unchanged: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsEnvelope {
    #[serde(default)]
    local_sync: Option<LocalSyncSettings>,
}

#[derive(Debug, Clone)]
struct ScannedEntry {
    source: PathBuf,
    kind: BlobKind,
    content_sha256: [u8; 32],
}

pub fn load_local_sync_settings(path: &Path) -> anyhow::Result<Option<LocalSyncSettings>> {
    let source = fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))?;
    let envelope: SettingsEnvelope = serde_json::from_str(&source)
        .map_err(|error| anyhow::anyhow!("invalid {} localSync settings: {error}", path.display()))?;
    if let Some(settings) = &envelope.local_sync {
        settings.validate()?;
    }
    Ok(envelope.local_sync)
}

impl LocalSyncSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.directory.as_os_str().is_empty() {
            anyhow::bail!("Cloud Node localSync.directory cannot be empty");
        }
        normalize_logical_prefix(&self.logical_prefix)?;
        Ok(())
    }

    pub fn resolve_directory(&self, config_path: &Path) -> anyhow::Result<PathBuf> {
        self.validate()?;
        let candidate = if self.directory.is_absolute() {
            self.directory.clone()
        } else {
            config_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(&self.directory)
        };
        durable::create_dir_all(&candidate).map_err(|error| {
            anyhow::anyhow!(
                "failed to create Cloud Node localSync.directory {}: {error}",
                candidate.display()
            )
        })?;
        candidate.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize Cloud Node localSync.directory {}: {error}",
                candidate.display()
            )
        })
    }
}

impl CloudNodeStore {
    pub fn local_directory_status(
        &self,
        directory: &Path,
        logical_prefix: &str,
    ) -> anyhow::Result<LocalDirectoryStatus> {
        let prefix = normalize_logical_prefix(logical_prefix)?;
        let scanned = scan_local_directory(self, directory, &prefix)?;
        let managed = managed_objects(self, &prefix)?;
        let dirty = local_directory_dirty(&scanned, &managed);
        Ok(LocalDirectoryStatus {
            dirty,
            scanned_files: scanned.len(),
            managed_objects: managed.len(),
        })
    }

    pub fn sync_local_directory(
        &self,
        directory: &Path,
        logical_prefix: &str,
    ) -> anyhow::Result<LocalDirectorySyncResult> {
        let prefix = normalize_logical_prefix(logical_prefix)?;
        let scanned = scan_local_directory(self, directory, &prefix)?;
        let managed = managed_objects(self, &prefix)?;
        let mut result = LocalDirectorySyncResult {
            scanned_files: scanned.len(),
            ..Default::default()
        };

        for (logical_path, entry) in &scanned {
            let unchanged = managed.iter().any(|object| {
                object.logical_path == *logical_path
                    && object.kind == entry.kind
                    && object.content_sha256 == entry.content_sha256
            });
            if unchanged {
                result.unchanged = result.unchanged.saturating_add(1);
                continue;
            }
            match entry.kind {
                BlobKind::File => {
                    self.store_file(&entry.source, logical_path)?;
                }
                BlobKind::Video => {
                    self.store_video(&entry.source, logical_path)?;
                }
                BlobKind::Folder => unreachable!("local directory scans do not emit folder objects"),
            }
            result.stored = result.stored.saturating_add(1);
        }

        for object in &managed {
            let still_present = scanned
                .get(&object.logical_path)
                .is_some_and(|entry| entry.kind == object.kind);
            if still_present {
                continue;
            }
            remove_active_object(self, object)?;
            result.removed = result.removed.saturating_add(1);
        }

        Ok(result)
    }

    /// Materialize the provider/peer-backed Cloud Node snapshot into the configured
    /// watched directory. The Cloud Node store is verified before the working tree
    /// is touched. Empty directories are intentionally not tracked, matching Git.
    pub fn restore_local_directory(
        &self,
        directory: &Path,
        logical_prefix: &str,
    ) -> anyhow::Result<LocalDirectoryRestoreResult> {
        self.verify()?;
        let prefix = normalize_logical_prefix(logical_prefix)?;
        let root = prepare_local_root(self, directory)?;
        let scanned = scan_local_directory_from_root(self, &root, &prefix)?;
        let managed = managed_objects(self, &prefix)?;
        let mut desired = BTreeMap::<String, SyncObject>::new();
        for object in managed {
            if object.kind == BlobKind::Folder {
                continue;
            }
            if desired.insert(object.logical_path.clone(), object).is_some() {
                anyhow::bail!("Cloud Node localSync snapshot contains duplicate logical paths");
            }
        }

        let mut result = LocalDirectoryRestoreResult::default();
        for (logical_path, object) in &desired {
            let relative = logical_relative_path(logical_path, &prefix)?;
            let target = join_relative_logical_path(&root, relative)?;
            let current_matches = scanned.get(logical_path).is_some_and(|entry| {
                entry.kind == object.kind && entry.content_sha256 == object.content_sha256
            });
            if current_matches {
                result.unchanged = result.unchanged.saturating_add(1);
                continue;
            }
            let payload = object.payload_path.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Cloud Node managed object {:?} has no restorable payload",
                    object.logical_path
                )
            })?;
            restore_payload_atomic(payload, &target, object.content_sha256)?;
            result.restored = result.restored.saturating_add(1);
        }

        for (logical_path, entry) in &scanned {
            if desired.contains_key(logical_path) {
                continue;
            }
            if entry.source.is_file() {
                durable::remove_file(&entry.source)?;
                result.removed = result.removed.saturating_add(1);
            }
        }

        Ok(result)
    }
}

fn managed_objects(store: &CloudNodeStore, prefix: &str) -> anyhow::Result<Vec<SyncObject>> {
    Ok(store
        .sync_plan()?
        .ordered()
        .filter(|object| logical_is_managed(&object.logical_path, prefix))
        .cloned()
        .collect())
}

fn local_directory_dirty(
    scanned: &BTreeMap<String, ScannedEntry>,
    managed: &[SyncObject],
) -> bool {
    if scanned.len() != managed.len() {
        return true;
    }
    scanned.iter().any(|(logical_path, entry)| {
        !managed.iter().any(|object| {
            object.logical_path == *logical_path
                && object.kind == entry.kind
                && object.content_sha256 == entry.content_sha256
        })
    })
}

fn scan_local_directory(
    store: &CloudNodeStore,
    directory: &Path,
    prefix: &str,
) -> anyhow::Result<BTreeMap<String, ScannedEntry>> {
    let root = prepare_local_root(store, directory)?;
    scan_local_directory_from_root(store, &root, prefix)
}

fn scan_local_directory_from_root(
    store: &CloudNodeStore,
    root: &Path,
    prefix: &str,
) -> anyhow::Result<BTreeMap<String, ScannedEntry>> {
    let store_root = store.summary().root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "failed to canonicalize Cloud Node internal store {}: {error}",
            store.summary().root.display()
        )
    })?;
    if root.starts_with(&store_root) {
        anyhow::bail!(
            "Cloud Node localSync.directory {} cannot live inside the Cloud Node internal store {}",
            root.display(),
            store_root.display()
        );
    }
    let mut scanned = BTreeMap::new();
    walk_directory(root, root, &store_root, prefix, &mut scanned)?;
    Ok(scanned)
}

fn prepare_local_root(store: &CloudNodeStore, directory: &Path) -> anyhow::Result<PathBuf> {
    durable::create_dir_all(directory)?;
    let root = directory.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "failed to canonicalize Cloud Node localSync.directory {}: {error}",
            directory.display()
        )
    })?;
    let store_root = store.summary().root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "failed to canonicalize Cloud Node internal store {}: {error}",
            store.summary().root.display()
        )
    })?;
    if root == store_root || root.starts_with(&store_root) {
        anyhow::bail!(
            "Cloud Node localSync.directory {} cannot be the Cloud Node store or one of its children",
            root.display()
        );
    }
    Ok(root)
}

fn walk_directory(
    root: &Path,
    current: &Path,
    excluded_store_root: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, ScannedEntry>,
) -> anyhow::Result<()> {
    if current == excluded_store_root {
        return Ok(());
    }
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let canonical = path.canonicalize()?;
            if canonical == excluded_store_root {
                continue;
            }
            walk_directory(root, &path, excluded_store_root, prefix, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        let relative = relative_to_logical(relative)?;
        let logical_path = format!("{prefix}/{relative}");
        let kind = if is_video_path(&path) {
            BlobKind::Video
        } else {
            BlobKind::File
        };
        let entry = ScannedEntry {
            source: path.clone(),
            kind,
            content_sha256: sha256_file(&path)?,
        };
        if out.insert(logical_path.clone(), entry).is_some() {
            anyhow::bail!("Cloud Node localSync produced duplicate path {logical_path:?}");
        }
    }
    Ok(())
}

fn remove_active_object(store: &CloudNodeStore, object: &SyncObject) -> anyhow::Result<()> {
    let summary = store.summary();
    let object_hex = hex::encode(object.object_key);
    let object_dir = summary.storage.join(&object_hex);
    if object_dir.exists() {
        durable::remove_dir_all(&object_dir)?;
    }
    let priority = summary
        .root
        .join("priority")
        .join(format!("{object_hex}.level"));
    if priority.is_file() {
        durable::remove_file(priority)?;
    }
    Ok(())
}

fn restore_payload_atomic(
    source: &Path,
    target: &Path,
    expected_sha256: [u8; 32],
) -> anyhow::Result<()> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    durable::create_dir_all(parent)?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let leaf = target
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "restore".to_owned());
    let temporary = parent.join(format!(
        ".{leaf}.rbe-cn-restore-{}-{unique}.part",
        std::process::id()
    ));

    let copied = (|| -> anyhow::Result<()> {
        let mut input = File::open(source)?;
        let mut output = File::create(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        if sha256_file(&temporary)? != expected_sha256 {
            anyhow::bail!(
                "Cloud Node restored payload hash mismatch before activation: {}",
                target.display()
            );
        }

        #[cfg(windows)]
        if target.exists() {
            // Windows std::fs::rename cannot replace an existing target. The
            // durable Cloud Node store remains authoritative if the process dies
            // in this tiny remove/rename window, so the next cycle repairs it.
            durable::remove_file(target)?;
        }
        #[cfg(all(not(unix), not(windows)))]
        if target.exists() {
            durable::remove_file(target)?;
        }
        durable::rename(&temporary, target)?;
        Ok(())
    })();
    if copied.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    copied
}

fn normalize_logical_prefix(value: &str) -> anyhow::Result<String> {
    let normalized = value.replace('\\', "/");
    if normalized.is_empty()
        || normalized.len() > 512
        || normalized.starts_with('/')
        || normalized.ends_with('/')
        || normalized.contains(':')
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || normalized.chars().any(char::is_control)
    {
        anyhow::bail!(
            "Cloud Node localSync.logicalPrefix must be a relative canonical path without '.', '..', drive prefixes, or trailing slash"
        );
    }
    Ok(normalized)
}

fn logical_is_managed(logical_path: &str, prefix: &str) -> bool {
    logical_path == prefix
        || logical_path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn logical_relative_path<'a>(logical_path: &'a str, prefix: &str) -> anyhow::Result<&'a str> {
    let suffix = logical_path.strip_prefix(prefix).ok_or_else(|| {
        anyhow::anyhow!("Cloud Node localSync object escaped configured logical prefix")
    })?;
    let relative = suffix.strip_prefix('/').ok_or_else(|| {
        anyhow::anyhow!("Cloud Node localSync object cannot map the prefix root to a file")
    })?;
    if relative.is_empty() {
        anyhow::bail!("Cloud Node localSync object has an empty relative path");
    }
    Ok(relative)
}

fn relative_to_logical(path: &Path) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node localSync paths must be valid UTF-8: {}",
                        path.display()
                    )
                })?;
                if value.is_empty() || value == "." || value == ".." || value.chars().any(char::is_control) {
                    anyhow::bail!("Cloud Node localSync path is not canonical: {}", path.display());
                }
                parts.push(value);
            }
            _ => anyhow::bail!(
                "Cloud Node localSync path is not relative/canonical: {}",
                path.display()
            ),
        }
    }
    if parts.is_empty() {
        anyhow::bail!("Cloud Node localSync file path is empty");
    }
    Ok(parts.join("/"))
}

fn join_relative_logical_path(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let mut target = root.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            anyhow::bail!("Cloud Node localSync restore path is not canonical");
        }
        target.push(part);
    }
    Ok(target)
}

fn is_video_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("mp4" | "mkv" | "mov" | "webm" | "avi" | "m4v" | "ts" | "m2ts")
    )
}

fn sha256_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut reader = File::open(path)?;
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

fn default_logical_prefix() -> String {
    DEFAULT_LOGICAL_PREFIX.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-cn-local-sync-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn store(root: &Path) -> CloudNodeStore {
        let settings: crate::CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "node": {
                "id": "local-sync-test",
                "storageRoot": root.join("cn")
            }
        }))
        .unwrap();
        CloudNodeStore::open(&settings).unwrap()
    }

    #[test]
    fn watched_directory_tracks_changes_and_deletions() {
        let root = root("changes");
        let watched = root.join("watched");
        fs::create_dir_all(&watched).unwrap();
        fs::write(watched.join("a.txt"), b"one").unwrap();
        let store = store(&root);

        assert!(store
            .local_directory_status(&watched, "workspace")
            .unwrap()
            .dirty);
        let first = store.sync_local_directory(&watched, "workspace").unwrap();
        assert_eq!(first.stored, 1);
        assert!(!store
            .local_directory_status(&watched, "workspace")
            .unwrap()
            .dirty);

        fs::write(watched.join("a.txt"), b"two").unwrap();
        let second = store.sync_local_directory(&watched, "workspace").unwrap();
        assert_eq!(second.stored, 1);
        fs::remove_file(watched.join("a.txt")).unwrap();
        let third = store.sync_local_directory(&watched, "workspace").unwrap();
        assert_eq!(third.removed, 1);
        assert_eq!(
            store
                .sync_plan()
                .unwrap()
                .ordered()
                .filter(|object| logical_is_managed(&object.logical_path, "workspace"))
                .count(),
            0
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pulled_store_materializes_into_an_empty_working_directory() {
        let root = root("restore");
        let source = root.join("source.txt");
        fs::write(&source, b"remote state").unwrap();
        let watched = root.join("watched");
        fs::create_dir_all(&watched).unwrap();
        let store = store(&root);
        store.store_file(&source, "workspace/nested/state.txt").unwrap();

        let restored = store
            .restore_local_directory(&watched, "workspace")
            .unwrap();
        assert_eq!(restored.restored, 1);
        assert_eq!(
            fs::read(watched.join("nested/state.txt")).unwrap(),
            b"remote state"
        );
        assert!(!store
            .local_directory_status(&watched, "workspace")
            .unwrap()
            .dirty);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn watcher_skips_cloud_node_internal_store_when_it_is_nested() {
        let root = root("nested-store");
        let watched = root.join("watched");
        fs::create_dir_all(&watched).unwrap();
        let settings: crate::CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "node": {
                "id": "nested-store-test",
                "storageRoot": watched.join(".cloud-node")
            }
        }))
        .unwrap();
        let store = CloudNodeStore::open(&settings).unwrap();
        fs::write(watched.join("user.txt"), b"user data").unwrap();
        let sync = store.sync_local_directory(&watched, "workspace").unwrap();
        assert_eq!(sync.scanned_files, 1);
        assert_eq!(sync.stored, 1);
        let _ = fs::remove_dir_all(root);
    }
}