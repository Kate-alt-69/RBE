use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::config::CloudNodeSettings;
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
        let storage = root.join("storage");
        let backup = root.join("backup");
        fs::create_dir_all(&storage)?;
        fs::create_dir_all(&backup)?;
        Ok(Self {
            root,
            storage,
            backup,
            backup_versions: settings.node.backup_versions,
            preserve_original: settings.node.preserve_original,
            video_chunk_bytes: settings.node.video_chunk_bytes,
        })
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
        fs::create_dir_all(&object_dir)?;
        let manifest_path = object_dir.join(kind.manifest_name());
        let previous = read_manifest_if_present(&manifest_path)?;
        let parent = previous.as_ref().map(|manifest| manifest.content_sha256);
        let content_sha256 = sha256_file(source)?;
        let content_hex = hex::encode(content_sha256);
        let version_dir = object_dir.join("versions").join(&content_hex);
        fs::create_dir_all(&version_dir)?;
        let payload_path = version_dir.join("payload");
        if !payload_path.exists() {
            copy_exact(source, &payload_path)?;
        }
        let logical_size = fs::metadata(source)?.len();
        let body = match kind {
            BlobKind::File => {
                let changes = if let Some(previous) = &previous {
                    let old = object_dir
                        .join("versions")
                        .join(hex::encode(previous.content_sha256))
                        .join("payload");
                    if old.is_file() {
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
        fs::create_dir_all(&object_dir)?;
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
        fs::create_dir_all(&version_dir)?;
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
        let mut verified = 0usize;
        for entry in fs::read_dir(&self.storage)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let object_hex = entry.file_name().to_string_lossy().to_string();
            if object_hex.len() != 64 || !object_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                anyhow::bail!("invalid Cloud Node storage object directory {object_hex:?}");
            }
            for kind in [BlobKind::Folder, BlobKind::Video, BlobKind::File] {
                let path = entry.path().join(kind.manifest_name());
                if !path.is_file() {
                    continue;
                }
                let manifest = BlobManifest::decode(&fs::read(&path)?)?;
                if manifest.kind != kind || hex::encode(manifest.object_key) != object_hex {
                    anyhow::bail!(
                        "Cloud Node manifest identity mismatch at {}",
                        path.display()
                    );
                }
                if kind != BlobKind::Folder {
                    let payload = entry
                        .path()
                        .join("versions")
                        .join(hex::encode(manifest.content_sha256))
                        .join("payload");
                    if sha256_file(&payload)? != manifest.content_sha256 {
                        anyhow::bail!("Cloud Node payload hash mismatch at {}", payload.display());
                    }
                }
                verified = verified.saturating_add(1);
            }
        }
        Ok(verified)
    }

    fn store_video_chunks(
        &self,
        source: &Path,
        object_dir: &Path,
    ) -> anyhow::Result<Vec<ChunkRef>> {
        let chunks_dir = object_dir.join("chunks");
        fs::create_dir_all(&chunks_dir)?;
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
            if !chunk_path.exists() {
                atomic_write(&chunk_path, &buffer[..used])?;
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
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        let name = safe_leaf_name(logical_path);
        let original_path = original.join(&name);
        if self.preserve_original && !original_path.exists() {
            copy_exact(source, &original_path)?;
        }
        copy_exact(source, &latest.join(&name))?;
        update_history(
            &root.join("history.blob.cn"),
            manifest.content_sha256,
            manifest.created_unix_ms,
            self.backup_versions,
        )
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
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        let original_path = original.join("folder.blob.cn");
        if self.preserve_original && !original_path.exists() {
            atomic_write(&original_path, encoded)?;
        }
        atomic_write(&latest.join("folder.blob.cn"), encoded)?;
        update_history(
            &root.join("history.blob.cn"),
            content_sha256,
            now_ms()?,
            self.backup_versions,
        )
    }
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

fn copy_exact(source: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = target.with_extension(format!("cn.tmp.{}", std::process::id()));
    fs::copy(source, &temp)?;
    if target.exists() {
        fs::remove_file(target)?;
    }
    fs::rename(temp, target)?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("cn.tmp.{}", std::process::id()));
    {
        let mut writer = BufWriter::new(File::create(&temp)?);
        writer.write_all(bytes)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?;
    Ok(())
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
) -> anyhow::Result<()> {
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
    atomic_write(path, &encode_history(&entries)?)
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
}
