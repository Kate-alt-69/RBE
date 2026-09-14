from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one Cloud Node anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


# ---- workspace -------------------------------------------------------------
replace_once(
    "engine/Cargo.toml",
    '    "crates/video-manager",\n',
    '    "crates/video-manager",\n    "crates/cloud-node",\n',
)

# ---- Cloud Node crate ------------------------------------------------------
crate = Path("engine/crates/cloud-node")
(crate / "src").mkdir(parents=True, exist_ok=True)

(crate / "Cargo.toml").write_text(r'''[package]
name = "cloud-node"
version.workspace = true
edition.workspace = true
publish.workspace = true

[[bin]]
name = "cloud_node"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
sha2 = "0.10"
hex = "0.4"
ed25519-dalek = "2"
''')

(crate / "src/lib.rs").write_text(r'''//! Low-level RBE Cloud Node primitives.
//!
//! Cloud Node sits below REL/RELC. Language programs never receive node keys,
//! tunnel identities, topology, storage roots, or direct access to this crate.

mod config;
mod crypto;
mod format;
mod protocol;
mod store;

pub use config::{CloudNodeSettings, NodeMode, ReplicationTarget, UpstreamSettings};
pub use crypto::{
    load_signing_key_from_env, public_key_hex, sign_challenge, verify_challenge,
    CLOUD_NODE_PRIVATE_KEY_ENV,
};
pub use format::{
    BlobKind, BlobManifest, ByteRangeChange, ChunkRef, FolderEntry, BLOB_FORMAT_VERSION,
};
pub use protocol::{Frame, FrameKind, CN_PROTOCOL, MAX_FRAME_BYTES};
pub use store::{CloudNodeStore, StoreSummary, StoredObject};
''')

(crate / "src/config.rs").write_text(r'''use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE_NAME: &str = "setting.node.cn.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NodeMode {
    #[default]
    Primary,
    Replica,
    Relay,
    Archive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudNodeSettings {
    #[serde(default = "default_format_version")]
    pub format_version: u16,
    pub node: NodeSettings,
    #[serde(default)]
    pub upstream: Option<UpstreamSettings>,
    #[serde(default)]
    pub replication: ReplicationSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSettings {
    pub id: String,
    #[serde(default)]
    pub mode: NodeMode,
    pub storage_root: PathBuf,
    #[serde(default = "default_backup_versions")]
    pub backup_versions: usize,
    #[serde(default = "default_true")]
    pub preserve_original: bool,
    #[serde(default = "default_video_chunk_bytes")]
    pub video_chunk_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamSettings {
    pub url: String,
    pub public_key: String,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_true")]
    pub sync_on_connect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSettings {
    #[serde(default)]
    pub targets: Vec<ReplicationTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationTarget {
    pub node_id: String,
    pub url: String,
    pub public_key: String,
    #[serde(default)]
    pub durable: bool,
}

impl CloudNodeSettings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))?;
        let settings: Self = serde_json::from_str(&source)
            .map_err(|error| anyhow::anyhow!("invalid {}: {error}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.format_version != 1 {
            anyhow::bail!("unsupported Cloud Node settings format {}", self.format_version);
        }
        validate_node_id(&self.node.id)?;
        if self.node.storage_root.as_os_str().is_empty() {
            anyhow::bail!("Cloud Node storageRoot cannot be empty");
        }
        if self.node.backup_versions == 0 || self.node.backup_versions > 1024 {
            anyhow::bail!("Cloud Node backupVersions must be between 1 and 1024");
        }
        if !(1024 * 1024..=64 * 1024 * 1024).contains(&self.node.video_chunk_bytes) {
            anyhow::bail!("Cloud Node videoChunkBytes must be between 1 MiB and 64 MiB");
        }
        if let Some(upstream) = &self.upstream {
            validate_peer_url(&upstream.url)?;
            validate_public_key(&upstream.public_key)?;
        }
        for target in &self.replication.targets {
            validate_node_id(&target.node_id)?;
            validate_peer_url(&target.url)?;
            validate_public_key(&target.public_key)?;
        }
        Ok(())
    }
}

fn validate_node_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("Cloud Node id must use 1..=128 ASCII [A-Za-z0-9_.-] characters");
    }
    Ok(())
}

fn validate_peer_url(value: &str) -> anyhow::Result<()> {
    let secure = value.starts_with("https://") || value.starts_with("wss://");
    let local_dev = value.starts_with("http://127.0.0.1")
        || value.starts_with("http://localhost")
        || value.starts_with("ws://127.0.0.1")
        || value.starts_with("ws://localhost");
    if !secure && !local_dev {
        anyhow::bail!("Cloud Node peer URL must use TLS outside localhost development");
    }
    Ok(())
}

fn validate_public_key(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("Cloud Node public keys must be 32-byte hexadecimal Ed25519 keys");
    }
    Ok(())
}

const fn default_format_version() -> u16 {
    1
}
const fn default_backup_versions() -> usize {
    5
}
const fn default_true() -> bool {
    true
}
const fn default_video_chunk_bytes() -> usize {
    4 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_backup_history_to_five() {
        let settings: CloudNodeSettings = serde_json::from_str(
            r#"{"node":{"id":"nas-main","storageRoot":"/srv/nas"}}"#,
        )
        .unwrap();
        assert_eq!(settings.node.backup_versions, 5);
        assert!(settings.node.preserve_original);
        settings.validate().unwrap();
    }
}
''')

(crate / "src/crypto.rs").write_text(r'''use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub const CLOUD_NODE_PRIVATE_KEY_ENV: &str = "RBE_CLOUD_NODE_PRIVATE_KEY";
const CHALLENGE_DOMAIN: &[u8] = b"RBE-CLOUD-NODE-CHALLENGE/1";

pub fn load_signing_key_from_env() -> anyhow::Result<SigningKey> {
    let value = std::env::var(CLOUD_NODE_PRIVATE_KEY_ENV).map_err(|_| {
        anyhow::anyhow!(
            "{CLOUD_NODE_PRIVATE_KEY_ENV} must contain this node's 32-byte hexadecimal private key"
        )
    })?;
    let bytes = hex::decode(&value)
        .map_err(|_| anyhow::anyhow!("{CLOUD_NODE_PRIVATE_KEY_ENV} must be hexadecimal"))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!("{CLOUD_NODE_PRIVATE_KEY_ENV} must contain exactly 32 bytes")
    })?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn public_key_hex(signing: &SigningKey) -> String {
    hex::encode(signing.verifying_key().to_bytes())
}

fn challenge_message(node_id: &str, session: &[u8; 16], nonce: &[u8; 32]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(CHALLENGE_DOMAIN);
    digest.update((node_id.len() as u32).to_be_bytes());
    digest.update(node_id.as_bytes());
    digest.update(session);
    digest.update(nonce);
    digest.finalize().into()
}

pub fn sign_challenge(
    signing: &SigningKey,
    node_id: &str,
    session: &[u8; 16],
    nonce: &[u8; 32],
) -> [u8; 64] {
    signing
        .sign(&challenge_message(node_id, session, nonce))
        .to_bytes()
}

pub fn verify_challenge(
    public_key_hex: &str,
    node_id: &str,
    session: &[u8; 16],
    nonce: &[u8; 32],
    signature: &[u8; 64],
) -> anyhow::Result<()> {
    let public = hex::decode(public_key_hex)
        .map_err(|_| anyhow::anyhow!("Cloud Node public key is not hexadecimal"))?;
    let public: [u8; 32] = public
        .try_into()
        .map_err(|_| anyhow::anyhow!("Cloud Node public key must contain exactly 32 bytes"))?;
    let verifying = VerifyingKey::from_bytes(&public)
        .map_err(|_| anyhow::anyhow!("Cloud Node public key is invalid"))?;
    let signature = Signature::from_bytes(signature);
    verifying
        .verify(&challenge_message(node_id, session, nonce), &signature)
        .map_err(|_| anyhow::anyhow!("Cloud Node challenge signature verification failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_never_requires_transmitting_private_key() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let session = [3u8; 16];
        let nonce = [9u8; 32];
        let signature = sign_challenge(&signing, "nas-main", &session, &nonce);
        verify_challenge(
            &public_key_hex(&signing),
            "nas-main",
            &session,
            &nonce,
            &signature,
        )
        .unwrap();
        assert!(verify_challenge(
            &public_key_hex(&signing),
            "wrong-node",
            &session,
            &nonce,
            &signature,
        )
        .is_err());
    }
}
''')

(crate / "src/protocol.rs").write_text(r'''use std::io::{Cursor, Read};

pub const CN_PROTOCOL: &str = "RBE-CN/1";
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const FRAME_MAGIC: &[u8; 8] = b"RBECNFR1";
const FRAME_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    Challenge = 2,
    ChallengeResponse = 3,
    SyncHello = 4,
    FolderManifest = 5,
    ObjectRequest = 6,
    ObjectChunk = 7,
    SyncComplete = 8,
    Ping = 9,
    Pong = 10,
}

impl TryFrom<u8> for FrameKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Hello,
            2 => Self::Challenge,
            3 => Self::ChallengeResponse,
            4 => Self::SyncHello,
            5 => Self::FolderManifest,
            6 => Self::ObjectRequest,
            7 => Self::ObjectChunk,
            8 => Self::SyncComplete,
            9 => Self::Ping,
            10 => Self::Pong,
            _ => anyhow::bail!("unknown Cloud Node frame kind {value}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub session: [u8; 16],
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        if self.payload.len() > MAX_FRAME_BYTES {
            anyhow::bail!("Cloud Node frame exceeds {MAX_FRAME_BYTES} bytes");
        }
        let mut out = Vec::with_capacity(32 + self.payload.len());
        out.extend_from_slice(FRAME_MAGIC);
        out.extend_from_slice(&FRAME_VERSION.to_be_bytes());
        out.push(self.kind as u8);
        out.push(0);
        out.extend_from_slice(&self.session);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != FRAME_MAGIC {
            anyhow::bail!("invalid Cloud Node frame magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != FRAME_VERSION {
            anyhow::bail!("unsupported Cloud Node frame version {version}");
        }
        let mut kind = [0u8; 1];
        cursor.read_exact(&mut kind)?;
        let kind = FrameKind::try_from(kind[0])?;
        let mut reserved = [0u8; 1];
        cursor.read_exact(&mut reserved)?;
        if reserved[0] != 0 {
            anyhow::bail!("Cloud Node frame reserved bits are non-zero");
        }
        let mut session = [0u8; 16];
        cursor.read_exact(&mut session)?;
        let payload_len = read_u32(&mut cursor)? as usize;
        if payload_len > MAX_FRAME_BYTES || bytes.len() != 32usize.saturating_add(payload_len) {
            anyhow::bail!("invalid Cloud Node frame length");
        }
        let mut payload = vec![0u8; payload_len];
        cursor.read_exact(&mut payload)?;
        Ok(Self {
            kind,
            session,
            payload,
        })
    }
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
    use super::*;

    #[test]
    fn frame_round_trip_is_binary_and_bounded() {
        let frame = Frame {
            kind: FrameKind::SyncHello,
            session: [4u8; 16],
            payload: vec![0, 1, 2, 255, 0, 9],
        };
        let encoded = frame.encode().unwrap();
        assert!(encoded.starts_with(FRAME_MAGIC));
        assert_eq!(Frame::decode(&encoded).unwrap(), frame);
    }
}
''')

(crate / "src/format.rs").write_text(r'''use std::io::{Cursor, Read};

pub const BLOB_FORMAT_VERSION: u16 = 1;
const BLOB_MAGIC: &[u8; 8] = b"RBECNBL1";
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlobKind {
    File = 1,
    Video = 2,
    Folder = 3,
}

impl BlobKind {
    pub fn manifest_name(self) -> &'static str {
        match self {
            Self::File => "file.blob.cn",
            Self::Video => "video.blob.cn",
            Self::Folder => "folder.blob.cn",
        }
    }
}

impl TryFrom<u8> for BlobKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::File,
            2 => Self::Video,
            3 => Self::Folder,
            _ => anyhow::bail!("unknown Cloud Node blob kind {value}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteRangeChange {
    pub offset: u64,
    pub old_len: u64,
    pub new_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    pub offset: u64,
    pub len: u32,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    pub path: String,
    pub kind: BlobKind,
    pub object_key: [u8; 32],
    pub content_sha256: [u8; 32],
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobBody {
    File { changes: Vec<ByteRangeChange> },
    Video { chunk_bytes: u32, chunks: Vec<ChunkRef> },
    Folder { entries: Vec<FolderEntry> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobManifest {
    pub kind: BlobKind,
    pub object_key: [u8; 32],
    pub content_sha256: [u8; 32],
    pub parent_content_sha256: Option<[u8; 32]>,
    pub logical_path: String,
    pub logical_size: u64,
    pub created_unix_ms: u64,
    pub body: BlobBody,
}

impl BlobManifest {
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        if self.logical_path.len() > u32::MAX as usize {
            anyhow::bail!("Cloud Node logical path is too large");
        }
        let mut out = Vec::new();
        out.extend_from_slice(BLOB_MAGIC);
        out.extend_from_slice(&BLOB_FORMAT_VERSION.to_be_bytes());
        out.push(self.kind as u8);
        out.push(u8::from(self.parent_content_sha256.is_some()));
        out.extend_from_slice(&self.object_key);
        out.extend_from_slice(&self.content_sha256);
        out.extend_from_slice(&self.parent_content_sha256.unwrap_or([0u8; 32]));
        out.extend_from_slice(&self.logical_size.to_be_bytes());
        out.extend_from_slice(&self.created_unix_ms.to_be_bytes());
        write_bytes(&mut out, self.logical_path.as_bytes())?;
        match &self.body {
            BlobBody::File { changes } => {
                write_count(&mut out, changes.len())?;
                for change in changes {
                    out.extend_from_slice(&change.offset.to_be_bytes());
                    out.extend_from_slice(&change.old_len.to_be_bytes());
                    write_bytes(&mut out, &change.new_bytes)?;
                }
            }
            BlobBody::Video {
                chunk_bytes,
                chunks,
            } => {
                out.extend_from_slice(&chunk_bytes.to_be_bytes());
                write_count(&mut out, chunks.len())?;
                for chunk in chunks {
                    out.extend_from_slice(&chunk.offset.to_be_bytes());
                    out.extend_from_slice(&chunk.len.to_be_bytes());
                    out.extend_from_slice(&chunk.sha256);
                }
            }
            BlobBody::Folder { entries } => {
                write_count(&mut out, entries.len())?;
                for entry in entries {
                    out.push(entry.kind as u8);
                    out.extend_from_slice(&entry.object_key);
                    out.extend_from_slice(&entry.content_sha256);
                    out.extend_from_slice(&entry.size.to_be_bytes());
                    write_bytes(&mut out, entry.path.as_bytes())?;
                }
            }
        }
        if out.len() > MAX_MANIFEST_BYTES {
            anyhow::bail!("Cloud Node blob manifest exceeds {MAX_MANIFEST_BYTES} bytes");
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            anyhow::bail!("Cloud Node blob manifest exceeds {MAX_MANIFEST_BYTES} bytes");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != BLOB_MAGIC {
            anyhow::bail!("invalid Cloud Node blob magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != BLOB_FORMAT_VERSION {
            anyhow::bail!("unsupported Cloud Node blob version {version}");
        }
        let kind = BlobKind::try_from(read_u8(&mut cursor)?)?;
        let has_parent = read_u8(&mut cursor)?;
        if has_parent > 1 {
            anyhow::bail!("invalid Cloud Node parent flag");
        }
        let object_key = read_hash(&mut cursor)?;
        let content_sha256 = read_hash(&mut cursor)?;
        let parent_raw = read_hash(&mut cursor)?;
        let logical_size = read_u64(&mut cursor)?;
        let created_unix_ms = read_u64(&mut cursor)?;
        let logical_path = String::from_utf8(read_bytes(&mut cursor, bytes.len())?)
            .map_err(|_| anyhow::anyhow!("Cloud Node logical path is not valid UTF-8"))?;
        let body = match kind {
            BlobKind::File => {
                let count = read_u32(&mut cursor)? as usize;
                let mut changes = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    changes.push(ByteRangeChange {
                        offset: read_u64(&mut cursor)?,
                        old_len: read_u64(&mut cursor)?,
                        new_bytes: read_bytes(&mut cursor, bytes.len())?,
                    });
                }
                BlobBody::File { changes }
            }
            BlobKind::Video => {
                let chunk_bytes = read_u32(&mut cursor)?;
                let count = read_u32(&mut cursor)? as usize;
                let mut chunks = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    chunks.push(ChunkRef {
                        offset: read_u64(&mut cursor)?,
                        len: read_u32(&mut cursor)?,
                        sha256: read_hash(&mut cursor)?,
                    });
                }
                BlobBody::Video {
                    chunk_bytes,
                    chunks,
                }
            }
            BlobKind::Folder => {
                let count = read_u32(&mut cursor)? as usize;
                let mut entries = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    let kind = BlobKind::try_from(read_u8(&mut cursor)?)?;
                    let object_key = read_hash(&mut cursor)?;
                    let content_sha256 = read_hash(&mut cursor)?;
                    let size = read_u64(&mut cursor)?;
                    let path = String::from_utf8(read_bytes(&mut cursor, bytes.len())?)
                        .map_err(|_| anyhow::anyhow!("Cloud Node folder entry path is not UTF-8"))?;
                    entries.push(FolderEntry {
                        path,
                        kind,
                        object_key,
                        content_sha256,
                        size,
                    });
                }
                BlobBody::Folder { entries }
            }
        };
        if cursor.position() as usize != bytes.len() {
            anyhow::bail!("Cloud Node blob has trailing bytes");
        }
        Ok(Self {
            kind,
            object_key,
            content_sha256,
            parent_content_sha256: has_parent.then_some(parent_raw),
            logical_path,
            logical_size,
            created_unix_ms,
            body,
        })
    }
}

fn write_count(out: &mut Vec<u8>, value: usize) -> anyhow::Result<()> {
    let value = u32::try_from(value).map_err(|_| anyhow::anyhow!("Cloud Node entry count overflow"))?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_bytes(out: &mut Vec<u8>, value: &[u8]) -> anyhow::Result<()> {
    let len = u32::try_from(value.len()).map_err(|_| anyhow::anyhow!("Cloud Node field too large"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn read_bytes(cursor: &mut Cursor<&[u8]>, total: usize) -> anyhow::Result<Vec<u8>> {
    let len = read_u32(cursor)? as usize;
    let remaining = total.saturating_sub(cursor.position() as usize);
    if len > remaining {
        anyhow::bail!("Cloud Node blob field length exceeds remaining bytes");
    }
    let mut value = vec![0u8; len];
    cursor.read_exact(&mut value)?;
    Ok(value)
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u8> {
    let mut value = [0u8; 1];
    cursor.read_exact(&mut value)?;
    Ok(value[0])
}
fn read_u16(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u16> {
    let mut value = [0u8; 2];
    cursor.read_exact(&mut value)?;
    Ok(u16::from_be_bytes(value))
}
fn read_u32(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u32> {
    let mut value = [0u8; 4];
    cursor.read_exact(&mut value)?;
    Ok(u32::from_be_bytes(value))
}
fn read_u64(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u64> {
    let mut value = [0u8; 8];
    cursor.read_exact(&mut value)?;
    Ok(u64::from_be_bytes(value))
}
fn read_hash(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<[u8; 32]> {
    let mut value = [0u8; 32];
    cursor.read_exact(&mut value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_blob_is_binary_and_round_trips_exact_bytes() {
        let manifest = BlobManifest {
            kind: BlobKind::File,
            object_key: [1u8; 32],
            content_sha256: [2u8; 32],
            parent_content_sha256: Some([3u8; 32]),
            logical_path: "db/users.db".into(),
            logical_size: 4,
            created_unix_ms: 7,
            body: BlobBody::File {
                changes: vec![ByteRangeChange {
                    offset: 1,
                    old_len: 2,
                    new_bytes: vec![0, 255, 1],
                }],
            },
        };
        let encoded = manifest.encode().unwrap();
        assert_eq!(&encoded[..8], BLOB_MAGIC);
        assert_eq!(BlobManifest::decode(&encoded).unwrap(), manifest);
    }
}
''')

(crate / "src/store.rs").write_text(r'''use std::collections::VecDeque;
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
            anyhow::bail!("Cloud Node folder source is not a directory: {}", source.display());
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
                    anyhow::bail!("Cloud Node manifest identity mismatch at {}", path.display());
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

    fn store_video_chunks(&self, source: &Path, object_dir: &Path) -> anyhow::Result<Vec<ChunkRef>> {
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

fn collect_folder_entries(root: &Path, current: &Path, out: &mut Vec<FolderEntry>) -> anyhow::Result<()> {
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
    logical_path.rsplit('/').next().unwrap_or("object").to_string()
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
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))?)
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
    let count = u32::try_from(entries.len()).map_err(|_| anyhow::anyhow!("history count overflow"))?;
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
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
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
''')

(crate / "src/main.rs").write_text(r'''use std::path::{Path, PathBuf};

use cloud_node::{
    load_signing_key_from_env, public_key_hex, CloudNodeSettings, CloudNodeStore,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("cloud_node: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let config_path = take_config_arg(&mut args)?.unwrap_or_else(default_config_path);
    let command = args.first().map(String::as_str).unwrap_or("evaluate");

    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;
    let store = CloudNodeStore::open(&settings)?;
    match command {
        "evaluate" => {
            let summary = store.summary();
            println!("Cloud Node {} ready", settings.node.id);
            println!("storage={}", summary.storage.display());
            println!("backup={}", summary.backup.display());
            if let Some(upstream) = &settings.upstream {
                println!("upstream={}", upstream.url);
            }
        }
        "store-file" | "store-video" | "snapshot-folder" => {
            if args.len() != 3 {
                anyhow::bail!("{command} requires <source> <logical-path>");
            }
            let source = Path::new(&args[1]);
            let logical = &args[2];
            let stored = match command {
                "store-file" => store.store_file(source, logical)?,
                "store-video" => store.store_video(source, logical)?,
                "snapshot-folder" => store.snapshot_folder(source, logical)?,
                _ => unreachable!(),
            };
            println!("object={}", stored.object_key);
            println!("content={}", stored.content_sha256);
            println!("manifest={}", stored.manifest.display());
        }
        "verify" => {
            println!("verified={}", store.verify()?);
        }
        "--help" | "-h" => print_help(),
        other => anyhow::bail!("unknown cloud_node command {other:?}; use --help"),
    }
    Ok(())
}

fn take_config_arg(args: &mut Vec<String>) -> anyhow::Result<Option<PathBuf>> {
    let mut found = None;
    let mut index = 0usize;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--config=") {
            if value.is_empty() || found.is_some() {
                anyhow::bail!("--config must be supplied at most once with a non-empty path");
            }
            found = Some(PathBuf::from(value));
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(found)
}

fn default_config_path() -> PathBuf {
    std::env::var_os("RBE_CN_SETTINGS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe().ok().and_then(|path| {
                path.parent()
                    .map(|parent| parent.join("setting.node.cn.json"))
            })
        })
        .unwrap_or_else(|| PathBuf::from("setting.node.cn.json"))
}

fn print_help() {
    println!(
        "cloud_node [--config=<setting.node.cn.json>] [evaluate|verify|public-key|store-file <source> <logical>|store-video <source> <logical>|snapshot-folder <source> <logical>]"
    );
}
''')

# ---- Build integration -----------------------------------------------------
# build.ps1: Cloud Node-only builds must not require backend admin/signing
# credentials and must not erase an existing target package.
replace_once(
    "build.ps1",
    '''try {
    if (-not ($args -contains '--help' -or $args -contains '-h' -or $args -contains '-?')) {
        Initialize-AdminPasswordVerifier
    }
    Initialize-ContainerSigningKey

    $BuildWin = $false; $BuildLinux = $false; $BuildMacos = $false; $BuildAll = $false
''',
    '''try {
    $CloudNodeOnlyRequested = $args -contains '--cloud-node-only' -or $args -contains '--build-cloud-node' -or $args -contains '-CloudNodeOnly'
    $HelpRequested = $args -contains '--help' -or $args -contains '-h' -or $args -contains '-?'
    if (-not $HelpRequested -and -not $CloudNodeOnlyRequested) {
        Initialize-AdminPasswordVerifier
        Initialize-ContainerSigningKey
    }

    $BuildWin = $false; $BuildLinux = $false; $BuildMacos = $false; $BuildAll = $false
    $CloudNodeOnly = $false
''',
)
replace_once(
    "build.ps1",
    '''            '^--build-all$' { $BuildAll = $true; continue }
            '^--musl$' { $Musl = $true; continue }
''',
    '''            '^--build-all$' { $BuildAll = $true; continue }
            '^(--cloud-node-only|--build-cloud-node|-CloudNodeOnly)$' { $CloudNodeOnly = $true; continue }
            '^--musl$' { $Musl = $true; continue }
''',
)
replace_once(
    "build.ps1",
    '''        $outDir = Join-Path $DistRoot $target; $depDir = Join-Path $outDir 'dep'
        if (Test-Path -LiteralPath $outDir) { Remove-Item -LiteralPath $outDir -Recurse -Force }
        New-Item -ItemType Directory -Force -Path $depDir | Out-Null

        Write-Host "-- container-bin ($target) --" -ForegroundColor Cyan
''',
    '''        $outDir = Join-Path $DistRoot $target; $depDir = Join-Path $outDir 'dep'
        if ($CloudNodeOnly) {
            New-Item -ItemType Directory -Force -Path $outDir | Out-Null
            Write-Host "-- cloud_node ($target) --" -ForegroundColor Cyan
            Push-Location $EngineDir
            try { Invoke-Build 'cloud-node' $target $Release 'cloud_node'; $cloudNodePath = Get-BinaryPath $EngineDir 'cloud_node' $target $Release }
            finally { Pop-Location }
            if (-not (Test-Path $cloudNodePath)) { throw "Cloud Node binary was not produced: $cloudNodePath" }
            $cloudNodeName = if ((Get-TargetOs $target) -eq 'windows') { 'cloud_node.exe' } else { 'cloud_node' }
            Copy-Item $cloudNodePath (Join-Path $outDir $cloudNodeName) -Force
            Write-Host "  -> $(Join-Path $outDir $cloudNodeName)" -ForegroundColor Green
            continue
        }
        if (Test-Path -LiteralPath $outDir) { Remove-Item -LiteralPath $outDir -Recurse -Force }
        New-Item -ItemType Directory -Force -Path $depDir | Out-Null

        Write-Host "-- container-bin ($target) --" -ForegroundColor Cyan
''',
)
replace_once(
    "build.ps1",
    '''        if (-not (Test-Path $servicePath)) { throw "service runtime was not produced: $servicePath" }

        Write-Host "-- backend ($target) --" -ForegroundColor Cyan
''',
    '''        if (-not (Test-Path $servicePath)) { throw "service runtime was not produced: $servicePath" }

        Write-Host "-- cloud_node ($target) --" -ForegroundColor Cyan
        Push-Location $EngineDir
        try { Invoke-Build 'cloud-node' $target $Release 'cloud_node'; $cloudNodePath = Get-BinaryPath $EngineDir 'cloud_node' $target $Release }
        finally { Pop-Location }
        if (-not (Test-Path $cloudNodePath)) { throw "Cloud Node binary was not produced: $cloudNodePath" }

        Write-Host "-- backend ($target) --" -ForegroundColor Cyan
''',
)
replace_once(
    "build.ps1",
    '''        Copy-Item $servicePath (Join-Path $depDir $serviceName) -Force

        $settings = Join-Path $EngineDir 'settings.json'; if (Test-Path $settings) { Copy-Item $settings $outDir -Force }
''',
    '''        Copy-Item $servicePath (Join-Path $depDir $serviceName) -Force
        $cloudNodeName = if ((Get-TargetOs $target) -eq 'windows') { 'cloud_node.exe' } else { 'cloud_node' }
        Copy-Item $cloudNodePath (Join-Path $outDir $cloudNodeName) -Force

        $settings = Join-Path $EngineDir 'settings.json'; if (Test-Path $settings) { Copy-Item $settings $outDir -Force }
''',
)

# build-core.sh mirrors the same behavior for Linux/macOS/CI entrypoints.
replace_once(
    "build-core.sh",
    '''ARCH_ARM64=false; ARCH_ARMV7=false; CUSTOM_TARGET=""; SHOW_HELP=false
CACHE_REMOVE=false; CACHE_REMOVE_WIN=false; CACHE_REMOVE_LINUX=false; CACHE_REMOVE_ALL=false; DISTRO=""
''',
    '''ARCH_ARM64=false; ARCH_ARMV7=false; CUSTOM_TARGET=""; SHOW_HELP=false
CLOUD_NODE_ONLY=false
CACHE_REMOVE=false; CACHE_REMOVE_WIN=false; CACHE_REMOVE_LINUX=false; CACHE_REMOVE_ALL=false; DISTRO=""
''',
)
replace_once(
    "build-core.sh",
    '''        --build-all) BUILD_ALL=true ;;
        --musl) MUSL=true ;;
''',
    '''        --build-all) BUILD_ALL=true ;;
        --cloud-node-only|--build-cloud-node) CLOUD_NODE_ONLY=true ;;
        --musl) MUSL=true ;;
''',
)
replace_once(
    "build-core.sh",
    '''if [ -z "${RBE_CONTAINER_SIGNING_PRIVATE_KEY:-}" ]; then
    echo "ERROR: RBE_CONTAINER_SIGNING_PRIVATE_KEY is required for packaged builds." >&2
    echo 'For a temporary local key: export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$(openssl rand -hex 32)"' >&2
    exit 1
fi
if [ -z "${RBE_ADMIN_AUTH_ROUNDS:-}" ] || [ -z "${RBE_ADMIN_AUTH_SALT_HEX:-}" ] || [ -z "${RBE_ADMIN_AUTH_VERIFIER_HEX:-}" ]; then
    echo "ERROR: a complete RBE_ADMIN_AUTH_* verifier is required for packaged builds. Use build.sh for interactive password entry." >&2
    exit 1
fi
[[ "$RBE_ADMIN_AUTH_ROUNDS" =~ ^[0-9]+$ ]] && [ "$RBE_ADMIN_AUTH_ROUNDS" -ge 10000 ] || { echo "ERROR: invalid RBE_ADMIN_AUTH_ROUNDS." >&2; exit 1; }
[[ "$RBE_ADMIN_AUTH_SALT_HEX" =~ ^[0-9a-fA-F]{16}$ ]] || { echo "ERROR: invalid RBE_ADMIN_AUTH_SALT_HEX." >&2; exit 1; }
[[ "$RBE_ADMIN_AUTH_VERIFIER_HEX" =~ ^[0-9a-fA-F]{64}$ ]] || { echo "ERROR: invalid RBE_ADMIN_AUTH_VERIFIER_HEX." >&2; exit 1; }
''',
    '''if [ "$CLOUD_NODE_ONLY" = false ]; then
    if [ -z "${RBE_CONTAINER_SIGNING_PRIVATE_KEY:-}" ]; then
        echo "ERROR: RBE_CONTAINER_SIGNING_PRIVATE_KEY is required for packaged builds." >&2
        echo 'For a temporary local key: export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$(openssl rand -hex 32)"' >&2
        exit 1
    fi
    if [ -z "${RBE_ADMIN_AUTH_ROUNDS:-}" ] || [ -z "${RBE_ADMIN_AUTH_SALT_HEX:-}" ] || [ -z "${RBE_ADMIN_AUTH_VERIFIER_HEX:-}" ]; then
        echo "ERROR: a complete RBE_ADMIN_AUTH_* verifier is required for packaged builds. Use build.sh for interactive password entry." >&2
        exit 1
    fi
    [[ "$RBE_ADMIN_AUTH_ROUNDS" =~ ^[0-9]+$ ]] && [ "$RBE_ADMIN_AUTH_ROUNDS" -ge 10000 ] || { echo "ERROR: invalid RBE_ADMIN_AUTH_ROUNDS." >&2; exit 1; }
    [[ "$RBE_ADMIN_AUTH_SALT_HEX" =~ ^[0-9a-fA-F]{16}$ ]] || { echo "ERROR: invalid RBE_ADMIN_AUTH_SALT_HEX." >&2; exit 1; }
    [[ "$RBE_ADMIN_AUTH_VERIFIER_HEX" =~ ^[0-9a-fA-F]{64}$ ]] || { echo "ERROR: invalid RBE_ADMIN_AUTH_VERIFIER_HEX." >&2; exit 1; }
fi
''',
)
replace_once(
    "build-core.sh",
    '''    out_dir="$DIST_ROOT/$target"; dep_dir="$out_dir/dep"; rm -rf -- "$out_dir"; mkdir -p "$dep_dir"

    echo "-- container-bin ($target) --" >&2
''',
    '''    out_dir="$DIST_ROOT/$target"; dep_dir="$out_dir/dep"
    if [ "$CLOUD_NODE_ONLY" = true ]; then
        mkdir -p "$out_dir"
        echo "-- cloud_node ($target) --" >&2
        (cd "$ENGINE_DIR" && invoke_cargo_build cloud-node "$target" "$RELEASE" cloud_node)
        cloud_node_path=$(get_built_binary_path "$ENGINE_DIR" cloud_node "$target" "$RELEASE")
        [ -f "$cloud_node_path" ] || { echo "ERROR: Cloud Node binary missing: $cloud_node_path" >&2; exit 1; }
        cloud_node_dest="$out_dir/cloud_node"; [ "$(get_target_os "$target")" = windows ] && cloud_node_dest="$cloud_node_dest.exe"
        cp "$cloud_node_path" "$cloud_node_dest"
        echo "  -> $cloud_node_dest" >&2
        continue
    fi
    rm -rf -- "$out_dir"; mkdir -p "$dep_dir"

    echo "-- container-bin ($target) --" >&2
''',
)
replace_once(
    "build-core.sh",
    '''    [ -f "$service_path" ] || { echo "ERROR: service artifact missing: $service_path" >&2; exit 1; }

    echo "-- backend ($target) --" >&2
''',
    '''    [ -f "$service_path" ] || { echo "ERROR: service artifact missing: $service_path" >&2; exit 1; }

    echo "-- cloud_node ($target) --" >&2
    (cd "$ENGINE_DIR" && invoke_cargo_build cloud-node "$target" "$RELEASE" cloud_node)
    cloud_node_path=$(get_built_binary_path "$ENGINE_DIR" cloud_node "$target" "$RELEASE")
    [ -f "$cloud_node_path" ] || { echo "ERROR: Cloud Node binary missing: $cloud_node_path" >&2; exit 1; }

    echo "-- backend ($target) --" >&2
''',
)
replace_once(
    "build-core.sh",
    '''    service_dest="$dep_dir/service"; [ "$(get_target_os "$target")" = windows ] && service_dest="$service_dest.exe"
    cp "$service_path" "$service_dest"

    [ -f "$ENGINE_DIR/settings.json" ] && cp "$ENGINE_DIR/settings.json" "$out_dir/" 2>/dev/null || true
''',
    '''    service_dest="$dep_dir/service"; [ "$(get_target_os "$target")" = windows ] && service_dest="$service_dest.exe"
    cp "$service_path" "$service_dest"
    cloud_node_dest="$out_dir/cloud_node"; [ "$(get_target_os "$target")" = windows ] && cloud_node_dest="$cloud_node_dest.exe"
    cp "$cloud_node_path" "$cloud_node_dest"

    [ -f "$ENGINE_DIR/settings.json" ] && cp "$ENGINE_DIR/settings.json" "$out_dir/" 2>/dev/null || true
''',
)

# build.sh must bypass backend-only secret preparation for a Cloud Node-only build.
replace_once(
    "build.sh",
    '''case " $* " in
    *" --help "*|*" -h "*|*" -? "*) ;;
    *) ensure_admin_verifier ;;
esac
ensure_container_signing_key
''',
    '''CLOUD_NODE_ONLY=false
case " $* " in
    *" --cloud-node-only "*|*" --build-cloud-node "*) CLOUD_NODE_ONLY=true ;;
esac
case " $* " in
    *" --help "*|*" -h "*|*" -? "*) ;;
    *)
        if [ "$CLOUD_NODE_ONLY" = false ]; then
            ensure_admin_verifier
            ensure_container_signing_key
        fi
        ;;
esac
''',
)

# ---- Documentation ---------------------------------------------------------
Path("doc/cloud-node.md").write_text(r'''# RBE Cloud Node

Cloud Node is low-level RBE persistence infrastructure. It is deliberately below REL and RELC: language programs cannot read node private keys, choose peers, open node tunnels, or mutate topology.

## Build contract

Every normal packaged RBE build includes `cloud_node` / `cloud_node.exe` beside the backend binary. A node-only refresh is also supported without requiring the backend Control Room password or Container signing key:

```powershell
.\build.ps1 --cloud-node-only --build-win --arch-x64
.\build.ps1 --cloud-node-only --build-linux --arch-x64
```

```bash
./build.sh --cloud-node-only --build-linux --arch-x64
```

A Cloud Node-only build overwrites only the Cloud Node artifact inside `dist/<target>`; it does not delete an existing backend package.

## Node settings

Cloud Node reads `setting.node.cn.json`. Private keys never belong in this file. The node's Ed25519 private key is supplied through `RBE_CLOUD_NODE_PRIVATE_KEY`; peer public keys are configuration.

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "mode": "primary",
    "storageRoot": "Z:/",
    "backupVersions": 5,
    "preserveOriginal": true,
    "videoChunkBytes": 4194304
  },
  "upstream": {
    "url": "https://example-backend.invalid",
    "publicKey": "<64 hex Ed25519 public key>",
    "autoReconnect": true,
    "syncOnConnect": true
  },
  "replication": {
    "targets": []
  }
}
```

## NAS layout

For a configured storage root `<ROOT>` Cloud Node owns:

```text
<ROOT>/rbe/
├── storage/
│   └── <object-sha256>/
│       ├── file.blob.cn | video.blob.cn | folder.blob.cn
│       ├── versions/<content-sha256>/...
│       └── chunks/<chunk-sha256>.chunk       # video objects
└── backup/
    └── <same-object-sha256>/
        ├── original/                         # first exact object, preserved
        ├── latest/                           # latest exact object
        └── history.blob.cn                   # binary rolling history, default 5
```

The directory SHA is a stable object identity derived from blob kind + normalized logical path. Every exact content revision has its own SHA-256 inside that object. This gives backup and active storage the same stable lookup key while still keeping immutable content generations.

## Binary CN blob formats

`file.blob.cn`, `video.blob.cn`, `folder.blob.cn`, and `history.blob.cn` are binary formats. They are not JSON, UTF-8 documents, or UTF-16 documents. String fields inside a binary record are length-prefixed bytes for cross-platform path identity.

`file.blob.cn` records exact byte-range replacements from the previous content generation. The immutable full payload remains under `versions/<content-sha256>/payload`, so history can reconstruct or verify any retained generation without pretending a database needs database-specific semantics.

`video.blob.cn` uses large SHA-256-addressed chunks (4 MiB by default) so large media revisions can reuse unchanged chunks rather than duplicating a whole video in the active object store.

`folder.blob.cn` is the filesystem topology manifest. During LOCAL -> REMOTE recovery the sync planner must send/validate data in this order:

```text
folder.blob.cn / folder structure
        ↓
video.blob.cn + requested video chunks
        ↓
file.blob.cn + requested file generations
```

That ordering is part of the Cloud Node recovery contract and is intended to run while the RBE main node is in its `Evaluating..` phase before normal runtime admission.

## Authentication foundation

Cloud Node uses domain-separated Ed25519 challenge signing. The private key is never sent in a ping, challenge, response, sync frame, or configuration file. The binary `RBE-CN/1` frame envelope already reserves distinct message types for Hello, challenge/response, sync negotiation, folder manifests, object requests/chunks, completion, and ping/pong.

The storage/format/identity foundation in the Cloud Node crate intentionally does not expose any REL capability. The authenticated remote tunnel and backend Evaluating-phase sync admission are the next transport layer built on this contract.
''')
