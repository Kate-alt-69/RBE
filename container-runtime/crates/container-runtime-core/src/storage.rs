use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_NAMESPACE_BYTES: usize = 64;
const MAX_RELATIVE_PATH_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BlobRef {
    hash: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NamespaceManifest {
    generation: u64,
    files: BTreeMap<String, BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSnapshot {
    pub namespace: String,
    pub generation: u64,
    pub files: usize,
    pub logical_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCommit {
    pub namespace: String,
    pub generation: u64,
    pub files: usize,
    pub logical_bytes: u64,
    pub changed_paths: usize,
}

#[derive(Debug)]
enum StagedMutation {
    Put { staged_path: PathBuf, bytes: u64 },
    Delete,
}

/// Transactional persistent state for one RBE Environment.
///
/// A namespace commit is all-or-nothing. File contents are written to
/// content-addressed blobs first, then a complete generation manifest is
/// persisted, and finally `CURRENT` is atomically replaced. `CURRENT` is the
/// commit point: a crash before it changes leaves the previous generation
/// authoritative, while a crash after it changes exposes the complete new
/// manifest. Staging and volatile files are intentionally disposable.
pub struct EnvironmentStorageManager {
    root: PathBuf,
    limit_bytes: u64,
    io: atomic_io::AtomicIo,
    commit_lock: Mutex<()>,
    next_transaction: AtomicU64,
}

impl EnvironmentStorageManager {
    pub fn open(root: PathBuf, limit_bytes: u64) -> Result<Arc<Self>> {
        if limit_bytes == 0 {
            bail!("environment storage limit must be non-zero");
        }
        fs::create_dir_all(root.join("volatile"))?;
        fs::create_dir_all(root.join("staging"))?;
        fs::create_dir_all(root.join("state").join("blobs"))?;
        fs::create_dir_all(root.join("state").join("namespaces"))?;

        // Staging belongs to the process generation. Anything left here came
        // from an interrupted transaction and was never committed through a
        // namespace CURRENT pointer, so it is safe to discard on activation.
        reset_directory(&root.join("staging"))?;

        Ok(Arc::new(Self {
            root,
            limit_bytes,
            io: atomic_io::AtomicIo::new(),
            commit_lock: Mutex::new(()),
            next_transaction: AtomicU64::new(1),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn volatile_root(&self) -> PathBuf {
        self.root.join("volatile")
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub fn reset_volatile(&self) -> Result<()> {
        reset_directory(&self.root.join("volatile"))?;
        reset_directory(&self.root.join("staging"))?;
        Ok(())
    }

    pub fn begin(self: &Arc<Self>, namespace: &str) -> Result<StorageTransaction> {
        validate_namespace(namespace)?;
        let sequence = self.next_transaction.fetch_add(1, Ordering::Relaxed);
        let tx_name = format!("{}-{sequence:016x}", std::process::id());
        let staging_root = self.root.join("staging").join(tx_name);
        fs::create_dir_all(staging_root.join("files"))?;
        Ok(StorageTransaction {
            manager: Arc::clone(self),
            namespace: namespace.to_string(),
            staging_root,
            mutations: BTreeMap::new(),
            staged_bytes: 0,
            finished: false,
        })
    }

    pub fn read(&self, namespace: &str, path: &str) -> Result<Option<Vec<u8>>> {
        validate_namespace(namespace)?;
        let path = normalize_relative_path(path)?;
        let _guard = self
            .commit_lock
            .lock()
            .expect("storage commit lock poisoned");
        let manifest = self.load_manifest(namespace)?;
        let Some(blob) = manifest.files.get(&path) else {
            return Ok(None);
        };
        validate_blob_hash(&blob.hash)?;
        let bytes = self
            .io
            .read(&self.blob_path(&blob.hash))
            .with_context(|| format!("read storage blob {}", blob.hash))?;
        if bytes.len() as u64 != blob.bytes {
            bail!("storage blob {} length does not match manifest", blob.hash);
        }
        let actual = sha256_hex(&bytes);
        if actual != blob.hash {
            bail!("storage blob {} failed SHA-256 verification", blob.hash);
        }
        Ok(Some(bytes))
    }

    pub fn list(&self, namespace: &str) -> Result<Vec<String>> {
        validate_namespace(namespace)?;
        let _guard = self
            .commit_lock
            .lock()
            .expect("storage commit lock poisoned");
        Ok(self.load_manifest(namespace)?.files.into_keys().collect())
    }

    pub fn snapshot(&self, namespace: &str) -> Result<StorageSnapshot> {
        validate_namespace(namespace)?;
        let _guard = self
            .commit_lock
            .lock()
            .expect("storage commit lock poisoned");
        let manifest = self.load_manifest(namespace)?;
        Ok(StorageSnapshot {
            namespace: namespace.to_string(),
            generation: manifest.generation,
            files: manifest.files.len(),
            logical_bytes: logical_bytes(&manifest.files)?,
            limit_bytes: self.limit_bytes,
        })
    }

    fn commit_transaction(
        &self,
        namespace: &str,
        mutations: &BTreeMap<String, StagedMutation>,
    ) -> Result<StorageCommit> {
        validate_namespace(namespace)?;
        let _guard = self
            .commit_lock
            .lock()
            .expect("storage commit lock poisoned");
        let current = self.load_manifest(namespace)?;
        let mut next_files = current.files.clone();
        let mut predicted_bytes = logical_bytes(&next_files)?;

        // Reserve logical quota before materializing any new blob. This avoids
        // turning a rejected transaction into disk growth through orphaned blobs.
        for (path, mutation) in mutations {
            let previous = next_files.get(path).map_or(0, |entry| entry.bytes);
            predicted_bytes = predicted_bytes
                .checked_sub(previous)
                .ok_or_else(|| anyhow!("storage quota accounting underflow"))?;
            match mutation {
                StagedMutation::Put { bytes, .. } => {
                    predicted_bytes = predicted_bytes
                        .checked_add(*bytes)
                        .ok_or_else(|| anyhow!("storage quota accounting overflow"))?;
                    next_files.insert(
                        path.clone(),
                        BlobRef {
                            hash: String::new(),
                            bytes: *bytes,
                        },
                    );
                }
                StagedMutation::Delete => {
                    next_files.remove(path);
                }
            }
        }
        if predicted_bytes > self.limit_bytes {
            bail!(
                "environment storage quota exceeded: requested {predicted_bytes} bytes, limit {} bytes",
                self.limit_bytes
            );
        }

        // Replace temporary BlobRef placeholders with verified content-addressed
        // references. Blob writes are immutable/deduplicated and occur before the
        // generation pointer moves.
        for (path, mutation) in mutations {
            match mutation {
                StagedMutation::Put { staged_path, bytes } => {
                    let data = self
                        .io
                        .read(staged_path)
                        .with_context(|| format!("read staged storage file {path}"))?;
                    if data.len() as u64 != *bytes {
                        bail!("staged storage file {path} changed before commit");
                    }
                    let hash = sha256_hex(&data);
                    let blob_path = self.blob_path(&hash);
                    if blob_path.exists() {
                        let existing = self
                            .io
                            .read(&blob_path)
                            .with_context(|| format!("verify existing storage blob {hash}"))?;
                        if sha256_hex(&existing) != hash {
                            bail!("existing storage blob {hash} failed SHA-256 verification");
                        }
                    } else {
                        self.io
                            .write_atomic(&blob_path, &data)
                            .with_context(|| format!("write storage blob {hash}"))?;
                    }
                    next_files.insert(
                        path.clone(),
                        BlobRef {
                            hash,
                            bytes: *bytes,
                        },
                    );
                }
                StagedMutation::Delete => {
                    next_files.remove(path);
                }
            }
        }

        let generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("storage generation overflow"))?;
        let manifest = NamespaceManifest {
            generation,
            files: next_files,
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let generation_path = self.generation_path(namespace, generation);
        self.io
            .write_atomic(&generation_path, &manifest_bytes)
            .with_context(|| format!("write namespace {namespace} generation {generation}"))?;

        // Atomic commit point. Readers either observe the old complete manifest
        // or the new complete manifest; they never see a partially applied set.
        self.io
            .write_atomic(
                &self.current_path(namespace),
                format!("{generation}\n").as_bytes(),
            )
            .with_context(|| format!("commit namespace {namespace} generation {generation}"))?;

        Ok(StorageCommit {
            namespace: namespace.to_string(),
            generation,
            files: manifest.files.len(),
            logical_bytes: predicted_bytes,
            changed_paths: mutations.len(),
        })
    }

    fn namespace_root(&self, namespace: &str) -> PathBuf {
        self.root.join("state").join("namespaces").join(namespace)
    }

    fn current_path(&self, namespace: &str) -> PathBuf {
        self.namespace_root(namespace).join("CURRENT")
    }

    fn generation_path(&self, namespace: &str, generation: u64) -> PathBuf {
        self.namespace_root(namespace)
            .join("generations")
            .join(format!("{generation:016x}.json"))
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root
            .join("state")
            .join("blobs")
            .join(format!("{hash}.bin"))
    }

    fn load_manifest(&self, namespace: &str) -> Result<NamespaceManifest> {
        let current_path = self.current_path(namespace);
        let generation = match self.io.read(&current_path) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .with_context(|| format!("namespace {namespace} CURRENT is not UTF-8"))?;
                text.trim()
                    .parse::<u64>()
                    .with_context(|| format!("namespace {namespace} CURRENT is invalid"))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(NamespaceManifest::default())
            }
            Err(error) => return Err(error).context("read storage CURRENT pointer"),
        };
        if generation == 0 {
            return Ok(NamespaceManifest::default());
        }
        let path = self.generation_path(namespace, generation);
        let bytes = self
            .io
            .read(&path)
            .with_context(|| format!("read namespace {namespace} generation {generation}"))?;
        let manifest: NamespaceManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode namespace {namespace} generation {generation}"))?;
        if manifest.generation != generation {
            bail!("namespace {namespace} generation manifest does not match CURRENT");
        }
        for (path, blob) in &manifest.files {
            normalize_relative_path(path)?;
            validate_blob_hash(&blob.hash)?;
        }
        Ok(manifest)
    }
}

pub struct StorageTransaction {
    manager: Arc<EnvironmentStorageManager>,
    namespace: String,
    staging_root: PathBuf,
    mutations: BTreeMap<String, StagedMutation>,
    staged_bytes: u64,
    finished: bool,
}

impl StorageTransaction {
    pub fn put(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        let path = normalize_relative_path(path)?;
        let bytes_len =
            u64::try_from(bytes.len()).map_err(|_| anyhow!("storage write is too large"))?;
        if bytes_len > self.manager.limit_bytes {
            bail!("single storage write exceeds Environment quota");
        }

        if let Some(StagedMutation::Put { bytes, .. }) = self.mutations.get(&path) {
            self.staged_bytes = self.staged_bytes.saturating_sub(*bytes);
        }
        self.staged_bytes = self
            .staged_bytes
            .checked_add(bytes_len)
            .ok_or_else(|| anyhow!("staged storage byte accounting overflow"))?;
        if self.staged_bytes > self.manager.limit_bytes {
            bail!("transaction staging exceeds Environment quota");
        }

        let staged_path = self.staging_root.join("files").join(&path);
        self.manager
            .io
            .write_atomic(&staged_path, bytes)
            .with_context(|| format!("stage storage write {path}"))?;
        self.mutations.insert(
            path,
            StagedMutation::Put {
                staged_path,
                bytes: bytes_len,
            },
        );
        Ok(())
    }

    pub fn delete(&mut self, path: &str) -> Result<()> {
        let path = normalize_relative_path(path)?;
        if let Some(StagedMutation::Put { staged_path, bytes }) = self.mutations.remove(&path) {
            self.staged_bytes = self.staged_bytes.saturating_sub(bytes);
            let _ = fs::remove_file(staged_path);
        }
        self.mutations.insert(path, StagedMutation::Delete);
        Ok(())
    }

    pub fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }

    pub fn commit(mut self) -> Result<StorageCommit> {
        let result = self
            .manager
            .commit_transaction(&self.namespace, &self.mutations);
        self.finished = true;
        let _ = fs::remove_dir_all(&self.staging_root);
        result
    }

    pub fn abort(mut self) {
        self.finished = true;
        let _ = fs::remove_dir_all(&self.staging_root);
    }
}

impl Drop for StorageTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::remove_dir_all(&self.staging_root);
        }
    }
}

fn logical_bytes(files: &BTreeMap<String, BlobRef>) -> Result<u64> {
    files.values().try_fold(0u64, |total, entry| {
        total
            .checked_add(entry.bytes)
            .ok_or_else(|| anyhow!("storage manifest byte accounting overflow"))
    })
}

fn validate_namespace(namespace: &str) -> Result<()> {
    if namespace.is_empty() || namespace.len() > MAX_NAMESPACE_BYTES {
        bail!("storage namespace must be 1..={MAX_NAMESPACE_BYTES} bytes");
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("storage namespace contains unsupported characters");
    }
    Ok(())
}

fn normalize_relative_path(path: &str) -> Result<String> {
    if path.is_empty() || path.len() > MAX_RELATIVE_PATH_BYTES || path.contains('\0') {
        bail!("storage path is empty, too long, or contains NUL");
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        bail!("storage path must be relative to the Environment namespace");
    }
    let mut clean = Vec::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("storage path contains an invalid segment");
        }
        clean.push(segment);
    }
    Ok(clean.join("/"))
}

fn validate_blob_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("storage manifest contains an invalid blob hash");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn reset_directory(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
    }
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-env-storage-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn multi_file_commit_switches_one_generation() {
        let root = temp_root("multi-file");
        let storage = EnvironmentStorageManager::open(root.clone(), 1024).unwrap();

        let mut first = storage.begin("uac").unwrap();
        first.put("users/index.json", b"users-v1").unwrap();
        first.put("sessions/index.json", b"sessions-v1").unwrap();
        let commit = first.commit().unwrap();
        assert_eq!(commit.generation, 1);
        assert_eq!(
            storage.read("uac", "users/index.json").unwrap().unwrap(),
            b"users-v1"
        );

        let mut second = storage.begin("uac").unwrap();
        second.put("users/index.json", b"users-v2").unwrap();
        second.delete("sessions/index.json").unwrap();
        second.put("metadata.json", b"meta-v2").unwrap();
        let commit = second.commit().unwrap();
        assert_eq!(commit.generation, 2);
        assert_eq!(
            storage.read("uac", "users/index.json").unwrap().unwrap(),
            b"users-v2"
        );
        assert!(storage
            .read("uac", "sessions/index.json")
            .unwrap()
            .is_none());
        assert_eq!(
            storage.read("uac", "metadata.json").unwrap().unwrap(),
            b"meta-v2"
        );

        assert!(storage.generation_path("uac", 1).is_file());
        assert!(storage.generation_path("uac", 2).is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_quota_commit_does_not_move_current() {
        let root = temp_root("quota");
        let storage = EnvironmentStorageManager::open(root.clone(), 10).unwrap();
        let mut first = storage.begin("uac").unwrap();
        first.put("a", b"12345").unwrap();
        first.commit().unwrap();

        let mut second = storage.begin("uac").unwrap();
        second.put("b", b"123456").unwrap();
        assert!(second.commit().is_err());
        assert_eq!(storage.snapshot("uac").unwrap().generation, 1);
        assert_eq!(storage.read("uac", "a").unwrap().unwrap(), b"12345");
        assert!(storage.read("uac", "b").unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn volatile_reset_keeps_committed_state() {
        let root = temp_root("volatile");
        let storage = EnvironmentStorageManager::open(root.clone(), 1024).unwrap();
        fs::write(storage.volatile_root().join("scratch.txt"), b"temporary").unwrap();
        let mut tx = storage.begin("service.uac").unwrap();
        tx.put("state.json", b"durable").unwrap();
        tx.commit().unwrap();

        storage.reset_volatile().unwrap();
        assert!(!storage.volatile_root().join("scratch.txt").exists());
        assert_eq!(
            storage.read("service.uac", "state.json").unwrap().unwrap(),
            b"durable"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn traversal_and_host_paths_are_rejected() {
        let root = temp_root("paths");
        let storage = EnvironmentStorageManager::open(root.clone(), 1024).unwrap();
        let mut tx = storage.begin("uac").unwrap();
        assert!(tx.put("../escape", b"no").is_err());
        assert!(tx.put("C:\\Windows\\nope", b"no").is_err());
        assert!(tx.put("/etc/passwd", b"no").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacing_a_staged_path_updates_staging_accounting() {
        let root = temp_root("replace");
        let storage = EnvironmentStorageManager::open(root.clone(), 16).unwrap();
        let mut tx = storage.begin("cache").unwrap();
        tx.put("entry", b"12345678").unwrap();
        tx.put("entry", b"12").unwrap();
        assert_eq!(tx.staged_bytes(), 2);
        tx.commit().unwrap();
        assert_eq!(storage.snapshot("cache").unwrap().logical_bytes, 2);
        let _ = fs::remove_dir_all(root);
    }
}
