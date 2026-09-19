use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::config::CloudNodeSettings;
use crate::durable;
use crate::format::{BlobBody, BlobKind, BlobManifest, ByteRangeChange, ChunkRef, FolderEntry};

const HISTORY_MAGIC: &[u8; 8] = b"RBECNHI1";
const HISTORY_VERSION: u16 = 1;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct StoreSummary {
    pub root: PathBuf,
    pub storage: PathBuf,
    pub backup: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StoredObject {
    pub object_key: String,
    pub content_sha256: String,
    pub manifest: PathBuf,
}

#[derive(Clone)]
pub struct CloudNodeStore {
    root: PathBuf,
    storage: PathBuf,
    backup: PathBuf,
    backup_versions: usize,
    preserve_original: bool,
    video_chunk_bytes: usize,
}

impl CloudNodeStore {
    pub fn open(settings: &CloudNodeSettings) -> anyhow::Result<Self> {
        settings.validate()?;
        let root = settings.node.storage_root.join("rbe");
        durable::create_dir_all(&root)?;
        let storage = root.join("storage");
        let backup = root.join("backup");
        let previous = root.join("recovery-previous-storage");

        let restored_previous = !storage.exists() && previous.exists();
        if restored_previous {
            durable::rename(&previous, &storage)?;
        }
        durable::create_dir_all(&storage)?;
        durable::create_dir_all(&backup)?;

        let store = Self {
            root,
            storage,
            backup,
            backup_versions: settings.node.backup_versions,
            preserve_original: settings.node.preserve_original,
            video_chunk_bytes: settings.node.video_chunk_bytes,
        };

        if restored_previous {
            store.verify().map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node restored interrupted previous storage but verification failed: {error}"
                )
            })?;
        } else if previous.exists() {
            store.reconcile_interrupted_swap(&previous)?;
        }
        Ok(store)
    }

    fn reconcile_interrupted_swap(&self, previous: &Path) -> anyhow::Result<()> {
        let live_error = match self.verify() {
            Ok(_) => {
                durable::remove_dir_all(previous)?;
                return Ok(());
            }
            Err(error) => error,
        };

        let failed = self.root.join("recovery-failed-storage");
        if failed.exists() {
            anyhow::bail!(
                "Cloud Node has an unresolved failed recovery tree at {}",
                failed.display()
            );
        }
        durable::rename(&self.storage, &failed)?;
        if let Err(error) = durable::rename(previous, &self.storage) {
            let _ = durable::rename(&failed, &self.storage);
            return Err(error.into());
        }

        match self.verify() {
            Ok(_) => {
                durable::remove_dir_all(&failed)?;
                Ok(())
            }
            Err(previous_error) => anyhow::bail!(
                "Cloud Node interrupted recovery contains two invalid trees: active={live_error}; previous={previous_error}; failed tree preserved at {}",
                failed.display()
            ),
        }
    }

    pub fn summary(&self) -> StoreSummary {
        StoreSummary {
            root: self.root.clone(),
            storage: self.storage.clone(),
            backup: self.backup.clone(),
        }
    }

    pub fn store_file(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {
        self.store_regular(source, logical_path, BlobKind::File)
    }

    pub fn store_file_with_priority(
        &self,
        source: &Path,
        logical_path: &str,
        level: u8,
    ) -> anyhow::Result<StoredObject> {
        validate_replication_priority(level)?;
        let stored = self.store_regular(source, logical_path, BlobKind::File)?;
        self.set_replication_priority(&stored.object_key, level)?;
        Ok(stored)
    }

    pub(crate) fn replication_priority(&self, object_key: &[u8; 32]) -> anyhow::Result<u8> {
        let path = self
            .root
            .join("priority")
            .join(format!("{}.level", hex::encode(object_key)));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(2),
            Err(error) => return Err(error.into()),
        };
        if bytes.len() != 1 {
            anyhow::bail!(
                "Cloud Node replication priority sidecar is malformed: {}",
                path.display()
            );
        }
        validate_replication_priority(bytes[0])?;
        Ok(bytes[0])
    }

    fn set_replication_priority(&self, object_key: &str, level: u8) -> anyhow::Result<()> {
        validate_replication_priority(level)?;
        if object_key.len() != 64 || !object_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("Cloud Node object key is invalid for replication priority metadata");
        }
        let path = self
            .root
            .join("priority")
            .join(format!("{}.level", object_key.to_ascii_lowercase()));
        atomic_write(&path, &[level])
    }

    pub fn store_video(&self, source: &Path, logical_path: &str) -> anyhow::Result<StoredObject> {
        self.store_regular(source, logical_path, BlobKind::Video)
    }

    fn store_regular(
        &self,
        source: &Path,
        logical_path: &str,
        kind: BlobKind,
    ) -> anyhow::Result<StoredObject> {
        if !source.is_file() {
            anyhow::bail!("Cloud Node source is not a file: {}", source.display());
        }
        let logical_path = normalize_logical_path(logical_path)?;
        let object_key = object_key(kind, &logical_path);
        let object_hex = hex::encode(object_key);
        let object_dir = self.storage.join(&object_hex);
        durable::create_dir_all(&object_dir)?;
        let manifest_path = object_dir.join(kind.manifest_name());
        let previous = read_manifest_if_present(&manifest_path)?;
        let parent = previous.as_ref().map(|manifest| manifest.content_sha256);
        let content_sha256 = sha256_file(source)?;
        let content_hex = hex::encode(content_sha256);
        let version_dir = object_dir.join("versions").join(&content_hex);
        durable::create_dir_all(&version_dir)?;
        let payload_path = version_dir.join("payload");
        let logical_size = fs::metadata(source)?.len();
        if !file_matches(&payload_path, content_sha256, logical_size)? {
            copy_exact(source, &payload_path)?;
            if !file_matches(&payload_path, content_sha256, logical_size)? {
                anyhow::bail!(
                    "Cloud Node content-addressed payload verification failed after repair: {}",
                    payload_path.display()
                );
            }
        }
        let body = match kind {
            BlobKind::File => {
                let changes = if let Some(previous) = &previous {
                    let old = object_dir
                        .join("versions")
                        .join(hex::encode(previous.content_sha256))
                        .join("payload");
                    if file_matches(&old, previous.content_sha256, previous.logical_size)? {
                        exact_byte_changes(&old, source)?
                    } else {
                        vec![ByteRangeChange {
                            offset: 0,
                            old_len: previous.logical_size,
                            new_bytes: fs::read(source)?,
                        }]
                    }
                } else {
                    vec![ByteRangeChange {
                        offset: 0,
                        old_len: 0,
                        new_bytes: fs::read(source)?,
                    }]
                };
                BlobBody::File { changes }
            }
            BlobKind::Video => BlobBody::Video {
                chunk_bytes: u32::try_from(self.video_chunk_bytes)
                    .map_err(|_| anyhow::anyhow!("video chunk size exceeds u32"))?,
                chunks: self.store_video_chunks(source, &object_dir)?,
            },
            BlobKind::Folder => unreachable!("regular storage cannot write folder bodies"),
        };
        let manifest = BlobManifest {
            kind,
            object_key,
            content_sha256,
            parent_content_sha256: parent,
            logical_path: logical_path.clone(),
            logical_size,
            created_unix_ms: now_ms()?,
            body,
        };
        atomic_write(&manifest_path, &manifest.encode()?)?;
        self.update_backup(source, &logical_path, &object_hex, &manifest)?;
        Ok(StoredObject {
            object_key: object_hex,
            content_sha256: content_hex,
            manifest: manifest_path,
        })
    }

    pub fn snapshot_folder(
        &self,
        source: &Path,
        logical_path: &str,
    ) -> anyhow::Result<StoredObject> {
        if !source.is_dir() {
            anyhow::bail!(
                "Cloud Node folder source is not a directory: {}",
                source.display()
            );
        }
        let logical_path = normalize_logical_path(logical_path)?;
        let object_key = object_key(BlobKind::Folder, &logical_path);
        let object_hex = hex::encode(object_key);
        let object_dir = self.storage.join(&object_hex);
        durable::create_dir_all(&object_dir)?;
        let manifest_path = object_dir.join(BlobKind::Folder.manifest_name());
        let previous = read_manifest_if_present(&manifest_path)?;
        let mut entries = Vec::new();
        collect_folder_entries(source, source, &mut entries)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let content_sha256 = folder_digest(&entries);
        let manifest = BlobManifest {
            kind: BlobKind::Folder,
            object_key,
            content_sha256,
            parent_content_sha256: previous.as_ref().map(|value| value.content_sha256),
            logical_path,
            logical_size: entries.iter().map(|entry| entry.size).sum(),
            created_unix_ms: now_ms()?,
            body: BlobBody::Folder { entries },
        };
        let encoded = manifest.encode()?;
        let version_dir = object_dir
            .join("versions")
            .join(hex::encode(content_sha256));
        durable::create_dir_all(&version_dir)?;
        atomic_write(&version_dir.join("folder.blob.cn"), &encoded)?;
        atomic_write(&manifest_path, &encoded)?;
        self.update_folder_backup(&object_hex, &encoded, content_sha256)?;
        Ok(StoredObject {
            object_key: object_hex,
            content_sha256: hex::encode(content_sha256),
            manifest: manifest_path,
        })
    }

    pub fn verify(&self) -> anyhow::Result<usize> {
        Self::verify_storage_path(&self.storage)
    }

    pub(crate) fn verify_storage_path(storage: &Path) -> anyhow::Result<usize> {
        let mut verified = 0usize;
        for entry in fs::read_dir(storage)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let object_hex = entry.file_name().to_string_lossy().to_string();
            if object_hex.len() != 64 || !object_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                anyhow::bail!("invalid Cloud Node storage object directory {object_hex:?}");
            }

            let mut current = None;
            for kind in [BlobKind::Folder, BlobKind::Video, BlobKind::File] {
                let path = entry.path().join(kind.manifest_name());
                if !path.is_file() {
                    continue;
                }
                if current.is_some() {
                    anyhow::bail!(
                        "Cloud Node object {} contains multiple current manifests",
                        entry.path().display()
                    );
                }
                current = Some((kind, path));
            }
            let Some((kind, path)) = current else {
                anyhow::bail!(
                    "Cloud Node object {} contains no current manifest",
                    entry.path().display()
                );
            };

            let manifest = BlobManifest::decode(&fs::read(&path)?)?;
            if manifest.kind != kind || hex::encode(manifest.object_key) != object_hex {
                anyhow::bail!(
                    "Cloud Node manifest identity mismatch at {}",
                    path.display()
                );
            }
            let normalized = normalize_logical_path(&manifest.logical_path)?;
            if normalized != manifest.logical_path
                || object_key(kind, &manifest.logical_path) != manifest.object_key
            {
                anyhow::bail!(
                    "Cloud Node manifest path identity is not canonical at {}",
                    path.display()
                );
            }

            match &manifest.body {
                BlobBody::File { changes } => {
                    for change in changes {
                        change.offset.checked_add(change.old_len).ok_or_else(|| {
                            anyhow::anyhow!("Cloud Node file change range overflow")
                        })?;
                    }
                    let payload = entry
                        .path()
                        .join("versions")
                        .join(hex::encode(manifest.content_sha256))
                        .join("payload");
                    let metadata = fs::metadata(&payload)?;
                    if metadata.len() != manifest.logical_size
                        || sha256_file(&payload)? != manifest.content_sha256
                    {
                        anyhow::bail!(
                            "Cloud Node file payload integrity mismatch at {}",
                            payload.display()
                        );
                    }
                }
                BlobBody::Video {
                    chunk_bytes,
                    chunks,
                } => {
                    if *chunk_bytes == 0 && !chunks.is_empty() {
                        anyhow::bail!("Cloud Node video manifest uses zero-sized chunks");
                    }
                    let mut expected_offset = 0u64;
                    for chunk in chunks {
                        if chunk.offset != expected_offset
                            || chunk.len == 0
                            || chunk.len > *chunk_bytes
                        {
                            anyhow::bail!("Cloud Node video manifest chunk layout is invalid");
                        }
                        let chunk_path = entry
                            .path()
                            .join("chunks")
                            .join(format!("{}.chunk", hex::encode(chunk.sha256)));
                        let metadata = fs::metadata(&chunk_path)?;
                        if metadata.len() != u64::from(chunk.len)
                            || sha256_file(&chunk_path)? != chunk.sha256
                        {
                            anyhow::bail!(
                                "Cloud Node video chunk integrity mismatch at {}",
                                chunk_path.display()
                            );
                        }
                        expected_offset = expected_offset
                            .checked_add(u64::from(chunk.len))
                            .ok_or_else(|| {
                                anyhow::anyhow!("Cloud Node video chunk range overflow")
                            })?;
                    }
                    if expected_offset != manifest.logical_size {
                        anyhow::bail!(
                            "Cloud Node video manifest size does not match its chunk layout"
                        );
                    }
                    let payload = entry
                        .path()
                        .join("versions")
                        .join(hex::encode(manifest.content_sha256))
                        .join("payload");
                    let metadata = fs::metadata(&payload)?;
                    if metadata.len() != manifest.logical_size
                        || sha256_file(&payload)? != manifest.content_sha256
                    {
                        anyhow::bail!(
                            "Cloud Node video payload integrity mismatch at {}",
                            payload.display()
                        );
                    }
                }
                BlobBody::Folder { entries } => {
                    let mut previous: Option<&str> = None;
                    let mut logical_size = 0u64;
                    for folder_entry in entries {
                        let normalized = normalize_logical_path(&folder_entry.path)?;
                        if normalized != folder_entry.path
                            || object_key(folder_entry.kind, &folder_entry.path)
                                != folder_entry.object_key
                        {
                            anyhow::bail!("Cloud Node folder entry identity is not canonical");
                        }
                        if folder_entry.kind == BlobKind::Folder
                            && (folder_entry.content_sha256 != [0u8; 32] || folder_entry.size != 0)
                        {
                            anyhow::bail!("Cloud Node folder entry directory metadata is invalid");
                        }
                        if previous.is_some_and(|value| value >= folder_entry.path.as_str()) {
                            anyhow::bail!(
                                "Cloud Node folder manifest entries are not strictly ordered"
                            );
                        }
                        previous = Some(&folder_entry.path);
                        logical_size =
                            logical_size.checked_add(folder_entry.size).ok_or_else(|| {
                                anyhow::anyhow!("Cloud Node folder logical size overflow")
                            })?;
                    }
                    if logical_size != manifest.logical_size {
                        anyhow::bail!("Cloud Node folder logical size does not match its entries");
                    }
                    if folder_digest(entries) != manifest.content_sha256 {
                        anyhow::bail!("Cloud Node folder manifest content hash mismatch");
                    }
                }
            }
            verified = verified.saturating_add(1);
        }
        Ok(verified)
    }

    fn store_video_chunks(
        &self,
        source: &Path,
        object_dir: &Path,
    ) -> anyhow::Result<Vec<ChunkRef>> {
        let chunks_dir = object_dir.join("chunks");
        durable::create_dir_all(&chunks_dir)?;
        let mut reader = BufReader::new(File::open(source)?);
        let mut buffer = vec![0u8; self.video_chunk_bytes];
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        loop {
            let mut used = 0usize;
            while used < buffer.len() {
                let read = reader.read(&mut buffer[used..])?;
                if read == 0 {
                    break;
                }
                used += read;
            }
            if used == 0 {
                break;
            }
            let hash: [u8; 32] = Sha256::digest(&buffer[..used]).into();
            let chunk_path = chunks_dir.join(format!("{}.chunk", hex::encode(hash)));
            if !file_matches(&chunk_path, hash, used as u64)? {
                atomic_write(&chunk_path, &buffer[..used])?;
                if !file_matches(&chunk_path, hash, used as u64)? {
                    anyhow::bail!(
                        "Cloud Node content-addressed video chunk verification failed after repair: {}",
                        chunk_path.display()
                    );
                }
            }
            chunks.push(ChunkRef {
                offset,
                len: u32::try_from(used).map_err(|_| anyhow::anyhow!("video chunk too large"))?,
                sha256: hash,
            });
            offset = offset.saturating_add(used as u64);
            if used < buffer.len() {
                break;
            }
        }
        Ok(chunks)
    }

    fn update_backup(
        &self,
        source: &Path,
        logical_path: &str,
        object_hex: &str,
        manifest: &BlobManifest,
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        let versions = root.join("versions");
        durable::create_dir_all(&original)?;
        durable::create_dir_all(&latest)?;
        durable::create_dir_all(&versions)?;
        let name = safe_leaf_name(logical_path);
        let original_path = original.join(&name);
        if self.preserve_original && !original_path.exists() {
            copy_exact(source, &original_path)?;
        }
        let content_hex = hex::encode(manifest.content_sha256);
        let revision_path = versions.join(&content_hex).join(&name);
        if !revision_path.exists() {
            copy_exact(source, &revision_path)?;
        }
        copy_exact(source, &latest.join(&name))?;
        let retained = update_history(
            &root.join("history.blob.cn"),
            manifest.content_sha256,
            manifest.created_unix_ms,
            self.backup_versions,
        )?;
        prune_backup_versions(&versions, &retained)
    }

    fn update_folder_backup(
        &self,
        object_hex: &str,
        encoded: &[u8],
        content_sha256: [u8; 32],
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        let versions = root.join("versions");
        durable::create_dir_all(&original)?;
        durable::create_dir_all(&latest)?;
        durable::create_dir_all(&versions)?;
        let original_path = original.join("folder.blob.cn");
        if self.preserve_original && !original_path.exists() {
            atomic_write(&original_path, encoded)?;
        }
        let content_hex = hex::encode(content_sha256);
        let revision_path = versions.join(&content_hex).join("folder.blob.cn");
        if !revision_path.exists() {
            atomic_write(&revision_path, encoded)?;
        }
        atomic_write(&latest.join("folder.blob.cn"), encoded)?;
        let retained = update_history(
            &root.join("history.blob.cn"),
            content_sha256,
            now_ms()?,
            self.backup_versions,
        )?;
        prune_backup_versions(&versions, &retained)
    }
}

fn validate_replication_priority(level: u8) -> anyhow::Result<()> {
    if !(1..=3).contains(&level) {
        anyhow::bail!("Cloud Node replication priority must be 1, 2, or 3");
    }
    Ok(())
}

fn normalize_logical_path(value: &str) -> anyhow::Result<String> {
    let normalized = value.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(':')
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || normalized.chars().any(char::is_control)
    {
        anyhow::bail!("invalid Cloud Node logical path {value:?}");
    }
    Ok(normalized)
}

fn object_key(kind: BlobKind, logical_path: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-OBJECT/1\0");
    digest.update([kind as u8]);
    digest.update(logical_path.as_bytes());
    digest.finalize().into()
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

fn exact_byte_changes(old: &Path, new: &Path) -> anyhow::Result<Vec<ByteRangeChange>> {
    let old_bytes = fs::read(old)?;
    let new_bytes = fs::read(new)?;
    if old_bytes == new_bytes {
        return Ok(Vec::new());
    }
    if old_bytes.len() != new_bytes.len() {
        let prefix = old_bytes
            .iter()
            .zip(&new_bytes)
            .take_while(|(left, right)| left == right)
            .count();
        let mut suffix = 0usize;
        while suffix < old_bytes.len().saturating_sub(prefix)
            && suffix < new_bytes.len().saturating_sub(prefix)
            && old_bytes[old_bytes.len() - 1 - suffix] == new_bytes[new_bytes.len() - 1 - suffix]
        {
            suffix += 1;
        }
        return Ok(vec![ByteRangeChange {
            offset: prefix as u64,
            old_len: old_bytes.len().saturating_sub(prefix + suffix) as u64,
            new_bytes: new_bytes[prefix..new_bytes.len().saturating_sub(suffix)].to_vec(),
        }]);
    }
    let mut changes = Vec::new();
    let mut index = 0usize;
    while index < old_bytes.len() {
        if old_bytes[index] == new_bytes[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < old_bytes.len() && old_bytes[index] != new_bytes[index] {
            index += 1;
        }
        changes.push(ByteRangeChange {
            offset: start as u64,
            old_len: (index - start) as u64,
            new_bytes: new_bytes[start..index].to_vec(),
        });
    }
    Ok(changes)
}

fn collect_folder_entries(
    root: &Path,
    current: &Path,
    out: &mut Vec<FolderEntry>,
) -> anyhow::Result<()> {
    let mut children = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let logical = normalize_logical_path(&relative)?;
            out.push(FolderEntry {
                path: logical.clone(),
                kind: BlobKind::Folder,
                object_key: object_key(BlobKind::Folder, &logical),
                content_sha256: [0u8; 32],
                size: 0,
            });
            collect_folder_entries(root, &path, out)?;
        } else if file_type.is_file() {
            let logical = normalize_logical_path(&relative)?;
            let kind = if is_video_path(&path) {
                BlobKind::Video
            } else {
                BlobKind::File
            };
            out.push(FolderEntry {
                path: logical.clone(),
                kind,
                object_key: object_key(kind, &logical),
                content_sha256: sha256_file(&path)?,
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
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

fn folder_digest(entries: &[FolderEntry]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-FOLDER/1\0");
    for entry in entries {
        digest.update([entry.kind as u8]);
        digest.update((entry.path.len() as u64).to_be_bytes());
        digest.update(entry.path.as_bytes());
        digest.update(entry.object_key);
        digest.update(entry.content_sha256);
        digest.update(entry.size.to_be_bytes());
    }
    digest.finalize().into()
}

fn safe_leaf_name(logical_path: &str) -> String {
    logical_path
        .rsplit('/')
        .next()
        .unwrap_or("object")
        .to_string()
}

fn read_manifest_if_present(path: &Path) -> anyhow::Result<Option<BlobManifest>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(BlobManifest::decode(&fs::read(path)?)?))
}

fn prune_backup_versions(
    versions: &Path,
    retained: &VecDeque<([u8; 32], u64)>,
) -> anyhow::Result<()> {
    let retained = retained
        .iter()
        .map(|(hash, _)| hex::encode(hash))
        .collect::<std::collections::HashSet<_>>();
    for entry in fs::read_dir(versions)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if !retained.contains(&name.to_ascii_lowercase()) {
            durable::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn file_matches(path: &Path, expected_hash: [u8; 32], expected_size: u64) -> anyhow::Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != expected_size {
        return Ok(false);
    }
    Ok(sha256_file(path)? == expected_hash)
}

fn part_path(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node target has no file name: {}", path.display()))?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{name}.part.{}", std::process::id())))
}

fn commit_part(part: &Path, target: &Path) -> anyhow::Result<()> {
    if target.exists() {
        #[cfg(unix)]
        {
            durable::rename(part, target)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            durable::remove_file(target)?;
        }
    }
    durable::rename(part, target)?;
    Ok(())
}

fn copy_exact(source: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        durable::create_dir_all(parent)?;
    }
    let part = part_path(target)?;
    if part.exists() {
        durable::remove_file(&part)?;
    }
    fs::copy(source, &part)?;
    File::open(&part)?.sync_all()?;
    commit_part(&part, target)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        durable::create_dir_all(parent)?;
    }
    let part = part_path(path)?;
    if part.exists() {
        durable::remove_file(&part)?;
    }
    {
        let mut writer = BufWriter::new(File::create(&part)?);
        writer.write_all(bytes)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    commit_part(&part, path)
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}

fn update_history(
    path: &Path,
    content_sha256: [u8; 32],
    created_unix_ms: u64,
    keep: usize,
) -> anyhow::Result<VecDeque<([u8; 32], u64)>> {
    let mut entries = if path.is_file() {
        decode_history(&fs::read(path)?)?
    } else {
        VecDeque::new()
    };
    if entries.back().is_none_or(|entry| entry.0 != content_sha256) {
        entries.push_back((content_sha256, created_unix_ms));
    }
    while entries.len() > keep {
        entries.pop_front();
    }
    atomic_write(path, &encode_history(&entries)?)?;
    Ok(entries)
}

fn encode_history(entries: &VecDeque<([u8; 32], u64)>) -> anyhow::Result<Vec<u8>> {
    let count =
        u32::try_from(entries.len()).map_err(|_| anyhow::anyhow!("history count overflow"))?;
    let mut out = Vec::with_capacity(14 + entries.len() * 40);
    out.extend_from_slice(HISTORY_MAGIC);
    out.extend_from_slice(&HISTORY_VERSION.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    for (hash, timestamp) in entries {
        out.extend_from_slice(hash);
        out.extend_from_slice(&timestamp.to_be_bytes());
    }
    Ok(out)
}

fn decode_history(bytes: &[u8]) -> anyhow::Result<VecDeque<([u8; 32], u64)>> {
    if bytes.len() < 14 || &bytes[..8] != HISTORY_MAGIC {
        anyhow::bail!("invalid Cloud Node history blob");
    }
    let version = u16::from_be_bytes([bytes[8], bytes[9]]);
    if version != HISTORY_VERSION {
        anyhow::bail!("unsupported Cloud Node history version {version}");
    }
    let count = u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    if bytes.len() != 14usize.saturating_add(count.saturating_mul(40)) {
        anyhow::bail!("invalid Cloud Node history length");
    }
    let mut out = VecDeque::with_capacity(count);
    let mut offset = 14usize;
    for _ in 0..count {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[offset..offset + 32]);
        offset += 32;
        let timestamp = u64::from_be_bytes(bytes[offset..offset + 8].try_into()?);
        offset += 8;
        out.push_back((hash, timestamp));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CloudNodeSettings;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": "test-node",
                "storageRoot": root,
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            }
        }))
        .unwrap()
    }

    #[test]
    fn storage_and_backup_share_stable_object_key() {
        let root = test_root("layout");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        fs::write(&source, b"abc").unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let first = store.store_file(&source, "db/users.db").unwrap();
        fs::write(&source, b"aXc").unwrap();
        let second = store.store_file(&source, "db/users.db").unwrap();
        assert_eq!(first.object_key, second.object_key);
        assert_ne!(first.content_sha256, second.content_sha256);
        assert!(store
            .summary()
            .backup
            .join(&first.object_key)
            .join("original/users.db")
            .is_file());
        assert_eq!(
            fs::read(
                store
                    .summary()
                    .backup
                    .join(&first.object_key)
                    .join("latest/users.db")
            )
            .unwrap(),
            b"aXc"
        );
        assert!(store.verify().unwrap() >= 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_keeps_original_plus_five_actual_revisions() {
        let root = test_root("backup-history");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let mut object_key = String::new();
        for revision in 0..7u8 {
            fs::write(&source, [revision, b'd', b'b']).unwrap();
            let stored = store.store_file(&source, "db/users.db").unwrap();
            object_key = stored.object_key;
        }
        let backup = store.summary().backup.join(object_key);
        assert_eq!(
            fs::read(backup.join("original/users.db")).unwrap(),
            [0, b'd', b'b']
        );
        assert_eq!(
            fs::read(backup.join("latest/users.db")).unwrap(),
            [6, b'd', b'b']
        );
        let versions = fs::read_dir(backup.join("versions"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(versions.len(), 5);
        for revision in 2..7u8 {
            let hash: [u8; 32] = Sha256::digest([revision, b'd', b'b']).into();
            let path = backup
                .join("versions")
                .join(hex::encode(hash))
                .join("users.db");
            assert_eq!(fs::read(path).unwrap(), [revision, b'd', b'b']);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_storage_uses_hashed_chunks() {
        let root = test_root("video");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("clip.mp4");
        fs::write(&source, vec![9u8; 2 * 1024 * 1024 + 17]).unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let stored = store.store_video(&source, "video/clip.mp4").unwrap();
        let manifest = BlobManifest::decode(&fs::read(stored.manifest).unwrap()).unwrap();
        let BlobBody::Video { chunks, .. } = manifest.body else {
            panic!("expected video blob");
        };
        assert_eq!(chunks.len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn storing_same_file_repairs_corrupted_content_addressed_payload() {
        let root = test_root("repair-payload");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        fs::write(&source, b"authoritative bytes").unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let stored = store.store_file(&source, "db/users.db").unwrap();
        let payload = store
            .summary()
            .storage
            .join(&stored.object_key)
            .join("versions")
            .join(&stored.content_sha256)
            .join("payload");
        fs::write(&payload, b"corrupt").unwrap();
        assert!(store.verify().is_err());
        store.store_file(&source, "db/users.db").unwrap();
        assert_eq!(fs::read(&payload).unwrap(), b"authoritative bytes");
        store.verify().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn storing_same_video_repairs_corrupted_content_addressed_chunk() {
        let root = test_root("repair-video-chunk");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("clip.mp4");
        fs::write(&source, vec![5u8; 1024 * 1024 + 17]).unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let stored = store.store_video(&source, "video/clip.mp4").unwrap();
        let manifest = BlobManifest::decode(&fs::read(&stored.manifest).unwrap()).unwrap();
        let BlobBody::Video { chunks, .. } = manifest.body else {
            panic!("expected video blob");
        };
        let first = chunks.first().unwrap();
        let chunk_path = store
            .summary()
            .storage
            .join(&stored.object_key)
            .join("chunks")
            .join(format!("{}.chunk", hex::encode(first.sha256)));
        fs::write(&chunk_path, b"corrupt").unwrap();
        assert!(store.verify().is_err());
        store.store_video(&source, "video/clip.mp4").unwrap();
        store.verify().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_rejects_corrupted_video_chunk_even_with_intact_payload() {
        let root = test_root("video-corruption");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("clip.mp4");
        fs::write(&source, vec![7u8; 1024 * 1024 + 31]).unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let stored = store.store_video(&source, "video/clip.mp4").unwrap();
        let manifest = BlobManifest::decode(&fs::read(&stored.manifest).unwrap()).unwrap();
        let BlobBody::Video { chunks, .. } = manifest.body else {
            panic!("expected video blob");
        };
        let first = chunks.first().expect("video should have chunks");
        let chunk_path = store
            .summary()
            .storage
            .join(&stored.object_key)
            .join("chunks")
            .join(format!("{}.chunk", hex::encode(first.sha256)));
        let mut bytes = fs::read(&chunk_path).unwrap();
        bytes[0] ^= 0xff;
        fs::write(&chunk_path, bytes).unwrap();
        assert!(store.verify().is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_rejects_inconsistent_folder_manifest_metadata() {
        let root = test_root("folder-corruption");
        fs::create_dir_all(&root).unwrap();
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.txt"), b"folder bytes").unwrap();
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let stored = store.snapshot_folder(&tree, "root").unwrap();
        let mut manifest = BlobManifest::decode(&fs::read(&stored.manifest).unwrap()).unwrap();
        let BlobBody::Folder { entries } = &mut manifest.body else {
            panic!("expected folder blob");
        };
        entries[0].size = entries[0].size.saturating_add(1);
        manifest.logical_size = manifest.logical_size.saturating_add(1);
        fs::write(&stored.manifest, manifest.encode().unwrap()).unwrap();
        assert!(store.verify().is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_restores_previous_storage_when_live_tree_is_missing() {
        let root = test_root("interrupted-missing-live");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        fs::write(&source, b"old bytes").unwrap();
        let settings = settings(&root);
        let store = CloudNodeStore::open(&settings).unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let expected = store.sync_plan().unwrap().header().unwrap();
        let summary = store.summary();
        let previous = summary.root.join("recovery-previous-storage");
        fs::rename(&summary.storage, &previous).unwrap();
        drop(store);

        let reopened = CloudNodeStore::open(&settings).unwrap();
        assert_eq!(reopened.sync_plan().unwrap().header().unwrap(), expected);
        assert!(!previous.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_keeps_verified_new_tree_after_interrupted_swap() {
        let root = test_root("interrupted-good-live");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        fs::write(&source, b"old bytes").unwrap();
        let settings = settings(&root);
        let store = CloudNodeStore::open(&settings).unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let summary = store.summary();
        let previous = summary.root.join("recovery-previous-storage");
        fs::rename(&summary.storage, &previous).unwrap();

        fs::write(&source, b"new bytes").unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let expected_new = store.sync_plan().unwrap().header().unwrap();
        drop(store);

        let reopened = CloudNodeStore::open(&settings).unwrap();
        assert_eq!(
            reopened.sync_plan().unwrap().header().unwrap(),
            expected_new
        );
        assert!(!previous.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_rolls_back_corrupt_new_tree_to_verified_previous_tree() {
        let root = test_root("interrupted-bad-live");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        fs::write(&source, b"old bytes").unwrap();
        let settings = settings(&root);
        let store = CloudNodeStore::open(&settings).unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let expected_old = store.sync_plan().unwrap().header().unwrap();
        let summary = store.summary();
        let previous = summary.root.join("recovery-previous-storage");
        fs::rename(&summary.storage, &previous).unwrap();

        fs::write(&source, b"new bytes").unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let plan = store.sync_plan().unwrap();
        let payload = plan.files[0].payload_path.as_ref().unwrap();
        fs::write(payload, b"corrupt").unwrap();
        drop(store);

        let reopened = CloudNodeStore::open(&settings).unwrap();
        assert_eq!(
            reopened.sync_plan().unwrap().header().unwrap(),
            expected_old
        );
        assert!(!previous.exists());
        assert!(!reopened
            .summary()
            .root
            .join("recovery-failed-storage")
            .exists());
        fs::remove_dir_all(root).unwrap();
    }
}
