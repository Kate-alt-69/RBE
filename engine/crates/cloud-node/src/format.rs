use std::io::{Cursor, Read};

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
    File {
        changes: Vec<ByteRangeChange>,
    },
    Video {
        chunk_bytes: u32,
        chunks: Vec<ChunkRef>,
    },
    Folder {
        entries: Vec<FolderEntry>,
    },
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
                    let path =
                        String::from_utf8(read_bytes(&mut cursor, bytes.len())?).map_err(|_| {
                            anyhow::anyhow!("Cloud Node folder entry path is not UTF-8")
                        })?;
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
            parent_content_sha256: (has_parent == 1).then_some(parent_raw),
            logical_path,
            logical_size,
            created_unix_ms,
            body,
        })
    }
}

fn write_count(out: &mut Vec<u8>, value: usize) -> anyhow::Result<()> {
    let value =
        u32::try_from(value).map_err(|_| anyhow::anyhow!("Cloud Node entry count overflow"))?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_bytes(out: &mut Vec<u8>, value: &[u8]) -> anyhow::Result<()> {
    let len =
        u32::try_from(value.len()).map_err(|_| anyhow::anyhow!("Cloud Node field too large"))?;
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
