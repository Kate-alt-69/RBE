use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::format::{BlobBody, BlobKind, BlobManifest};
use crate::store::CloudNodeStore;

const SYNC_HEADER_MAGIC: &[u8; 8] = b"RBECNSY1";
const SYNC_HEADER_VERSION: u16 = 1;
const SYNC_ROOT_DOMAIN: &[u8] = b"RBE-CN-SYNC-ROOT/1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncObject {
    pub kind: BlobKind,
    pub object_key: [u8; 32],
    pub content_sha256: [u8; 32],
    pub logical_path: String,
    pub logical_size: u64,
    pub manifest_path: PathBuf,
    pub payload_path: Option<PathBuf>,
    pub chunk_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub root_sha256: [u8; 32],
    pub folders: Vec<SyncObject>,
    pub videos: Vec<SyncObject>,
    pub files: Vec<SyncObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPlanHeader {
    pub root_sha256: [u8; 32],
    pub folder_count: u32,
    pub video_count: u32,
    pub file_count: u32,
}

impl SyncPlan {
    pub fn scan(store: &CloudNodeStore) -> anyhow::Result<Self> {
        let storage = store.summary().storage;
        let mut folders = Vec::new();
        let mut videos = Vec::new();
        let mut files = Vec::new();

        for entry in fs::read_dir(&storage)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let object_dir = entry.path();
            let object_name = entry.file_name().to_string_lossy().to_string();
            if object_name.len() != 64 || !object_name.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                anyhow::bail!("invalid Cloud Node object directory {object_name:?}");
            }

            let mut found = Vec::new();
            for kind in [BlobKind::Folder, BlobKind::Video, BlobKind::File] {
                let path = object_dir.join(kind.manifest_name());
                if path.is_file() {
                    found.push((kind, path));
                }
            }
            if found.len() > 1 {
                anyhow::bail!(
                    "Cloud Node object {} contains multiple current manifests",
                    object_dir.display()
                );
            }
            let Some((kind, manifest_path)) = found.pop() else {
                continue;
            };
            let manifest = BlobManifest::decode(&fs::read(&manifest_path)?)?;
            if manifest.kind != kind || hex::encode(manifest.object_key) != object_name {
                anyhow::bail!(
                    "Cloud Node current manifest identity mismatch at {}",
                    manifest_path.display()
                );
            }
            let object = sync_object(&object_dir, manifest_path, manifest)?;
            match kind {
                BlobKind::Folder => folders.push(object),
                BlobKind::Video => videos.push(object),
                BlobKind::File => files.push(object),
            }
        }

        let sort = |left: &SyncObject, right: &SyncObject| {
            left.logical_path
                .cmp(&right.logical_path)
                .then(left.object_key.cmp(&right.object_key))
        };
        folders.sort_by(sort);
        videos.sort_by(sort);
        files.sort_by(sort);

        let root_sha256 = sync_root(&folders, &videos, &files);
        Ok(Self {
            root_sha256,
            folders,
            videos,
            files,
        })
    }

    pub fn root_hex(&self) -> String {
        hex::encode(self.root_sha256)
    }

    pub fn object_count(&self) -> usize {
        self.folders
            .len()
            .saturating_add(self.videos.len())
            .saturating_add(self.files.len())
    }

    /// Canonical LOCAL -> REMOTE recovery order. Folder topology always goes
    /// first, then large video objects/chunks, then regular file generations.
    pub fn ordered(&self) -> impl Iterator<Item = &SyncObject> {
        self.folders
            .iter()
            .chain(self.videos.iter())
            .chain(self.files.iter())
    }

    pub fn header(&self) -> anyhow::Result<SyncPlanHeader> {
        Ok(SyncPlanHeader {
            root_sha256: self.root_sha256,
            folder_count: u32::try_from(self.folders.len())
                .map_err(|_| anyhow::anyhow!("Cloud Node folder count exceeds protocol range"))?,
            video_count: u32::try_from(self.videos.len())
                .map_err(|_| anyhow::anyhow!("Cloud Node video count exceeds protocol range"))?,
            file_count: u32::try_from(self.files.len())
                .map_err(|_| anyhow::anyhow!("Cloud Node file count exceeds protocol range"))?,
        })
    }
}

impl CloudNodeStore {
    pub fn sync_plan(&self) -> anyhow::Result<SyncPlan> {
        SyncPlan::scan(self)
    }
}

impl SyncPlanHeader {
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(56);
        out.extend_from_slice(SYNC_HEADER_MAGIC);
        out.extend_from_slice(&SYNC_HEADER_VERSION.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&self.root_sha256);
        out.extend_from_slice(&self.folder_count.to_be_bytes());
        out.extend_from_slice(&self.video_count.to_be_bytes());
        out.extend_from_slice(&self.file_count.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() != 56 {
            anyhow::bail!("invalid Cloud Node sync header length");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != SYNC_HEADER_MAGIC {
            anyhow::bail!("invalid Cloud Node sync header magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != SYNC_HEADER_VERSION {
            anyhow::bail!("unsupported Cloud Node sync header version {version}");
        }
        if read_u16(&mut cursor)? != 0 {
            anyhow::bail!("Cloud Node sync header reserved bits are non-zero");
        }
        let mut root_sha256 = [0u8; 32];
        cursor.read_exact(&mut root_sha256)?;
        Ok(Self {
            root_sha256,
            folder_count: read_u32(&mut cursor)?,
            video_count: read_u32(&mut cursor)?,
            file_count: read_u32(&mut cursor)?,
        })
    }
}

fn sync_object(
    object_dir: &Path,
    manifest_path: PathBuf,
    manifest: BlobManifest,
) -> anyhow::Result<SyncObject> {
    let content_hex = hex::encode(manifest.content_sha256);
    let (payload_path, chunk_paths) = match &manifest.body {
        BlobBody::Folder { .. } => (None, Vec::new()),
        BlobBody::File { .. } => {
            let payload = object_dir
                .join("versions")
                .join(&content_hex)
                .join("payload");
            if !payload.is_file() {
                anyhow::bail!(
                    "Cloud Node file generation payload is missing: {}",
                    payload.display()
                );
            }
            (Some(payload), Vec::new())
        }
        BlobBody::Video { chunks, .. } => {
            let payload = object_dir
                .join("versions")
                .join(&content_hex)
                .join("payload");
            if !payload.is_file() {
                anyhow::bail!(
                    "Cloud Node video generation payload is missing: {}",
                    payload.display()
                );
            }
            let mut paths = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                let path = object_dir
                    .join("chunks")
                    .join(format!("{}.chunk", hex::encode(chunk.sha256)));
                if !path.is_file() {
                    anyhow::bail!("Cloud Node video chunk is missing: {}", path.display());
                }
                paths.push(path);
            }
            (Some(payload), paths)
        }
    };

    Ok(SyncObject {
        kind: manifest.kind,
        object_key: manifest.object_key,
        content_sha256: manifest.content_sha256,
        logical_path: manifest.logical_path,
        logical_size: manifest.logical_size,
        manifest_path,
        payload_path,
        chunk_paths,
    })
}

fn sync_root(folders: &[SyncObject], videos: &[SyncObject], files: &[SyncObject]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(SYNC_ROOT_DOMAIN);
    digest.update((folders.len() as u64).to_be_bytes());
    digest.update((videos.len() as u64).to_be_bytes());
    digest.update((files.len() as u64).to_be_bytes());
    for object in folders.iter().chain(videos).chain(files) {
        digest.update([object.kind as u8]);
        digest.update(object.object_key);
        digest.update(object.content_sha256);
        digest.update(object.logical_size.to_be_bytes());
        digest.update((object.logical_path.len() as u64).to_be_bytes());
        digest.update(object.logical_path.as_bytes());
    }
    digest.finalize().into()
}

fn read_u16(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u16> {
    let mut bytes = [0u8; 2];
    cursor.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::CloudNodeSettings;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-sync-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn recovery_plan_orders_folder_video_then_file() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node": {
                "id": "sync-test",
                "storageRoot": root,
                "videoChunkBytes": 1048576
            }
        }))
        .unwrap();
        let store = CloudNodeStore::open(&settings).unwrap();

        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.txt"), b"folder topology").unwrap();
        store.snapshot_folder(&tree, "root").unwrap();

        let video = root.join("clip.mp4");
        fs::write(&video, vec![5u8; 1024 * 1024 + 7]).unwrap();
        store.store_video(&video, "video/clip.mp4").unwrap();

        let file = root.join("users.db");
        fs::write(&file, b"database bytes").unwrap();
        store.store_file(&file, "db/users.db").unwrap();

        let plan = store.sync_plan().unwrap();
        assert_eq!(plan.object_count(), 3);
        let kinds = plan.ordered().map(|object| object.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![BlobKind::Folder, BlobKind::Video, BlobKind::File]
        );
        assert_eq!(
            SyncPlanHeader::decode(&plan.header().unwrap().encode()).unwrap(),
            plan.header().unwrap()
        );
        assert_ne!(plan.root_sha256, [0u8; 32]);
        fs::remove_dir_all(root).unwrap();
    }
}
