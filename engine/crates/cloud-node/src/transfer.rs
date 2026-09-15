use std::io::{Cursor, Read};

use sha2::{Digest, Sha256};

use crate::format::BlobKind;
use crate::protocol::{Frame, FrameKind};

const TRANSFER_MAGIC: &[u8; 8] = b"RBECNXF1";
const TRANSFER_VERSION: u16 = 1;
const TRANSFER_HEADER_BYTES: usize = 162;
pub const MAX_TRANSFER_DATA_BYTES: usize = 8 * 1024 * 1024;
pub const RESUME_ACK_HEADER: &str = "x-rbe-cn-resume-ack";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TransferResource {
    Manifest = 1,
    FilePayload = 2,
    VideoChunk = 3,
}

impl TryFrom<u8> for TransferResource {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Manifest,
            2 => Self::FilePayload,
            3 => Self::VideoChunk,
            _ => anyhow::bail!("unknown Cloud Node transfer resource {value}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferChunk {
    pub kind: BlobKind,
    pub resource: TransferResource,
    pub final_chunk: bool,
    pub object_key: [u8; 32],
    pub content_sha256: [u8; 32],
    /// Hash of the complete resource being transferred. For a manifest this
    /// hashes the encoded `*.blob.cn`; for a file payload it equals the content
    /// SHA; for a video chunk it is the chunk's content-addressed SHA.
    pub resource_sha256: [u8; 32],
    pub offset: u64,
    pub total_size: u64,
    pub data_sha256: [u8; 32],
    pub data: Vec<u8>,
}

impl TransferChunk {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: BlobKind,
        resource: TransferResource,
        object_key: [u8; 32],
        content_sha256: [u8; 32],
        resource_sha256: [u8; 32],
        offset: u64,
        total_size: u64,
        data: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let data_sha256 = Sha256::digest(&data).into();
        let end = offset
            .checked_add(
                u64::try_from(data.len())
                    .map_err(|_| anyhow::anyhow!("transfer chunk size exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("Cloud Node transfer range overflow"))?;
        let chunk = Self {
            kind,
            resource,
            final_chunk: end == total_size,
            object_key,
            content_sha256,
            resource_sha256,
            offset,
            total_size,
            data_sha256,
            data,
        };
        chunk.validate()?;
        Ok(chunk)
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        let data_len = u32::try_from(self.data.len())
            .map_err(|_| anyhow::anyhow!("Cloud Node transfer chunk length exceeds u32"))?;
        let mut out = Vec::with_capacity(TRANSFER_HEADER_BYTES + self.data.len());
        out.extend_from_slice(TRANSFER_MAGIC);
        out.extend_from_slice(&TRANSFER_VERSION.to_be_bytes());
        out.push(self.kind as u8);
        out.push(self.resource as u8);
        out.push(u8::from(self.final_chunk));
        out.push(0);
        out.extend_from_slice(&self.object_key);
        out.extend_from_slice(&self.content_sha256);
        out.extend_from_slice(&self.resource_sha256);
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.total_size.to_be_bytes());
        out.extend_from_slice(&data_len.to_be_bytes());
        out.extend_from_slice(&self.data_sha256);
        out.extend_from_slice(&self.data);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() < TRANSFER_HEADER_BYTES
            || bytes.len() > TRANSFER_HEADER_BYTES.saturating_add(MAX_TRANSFER_DATA_BYTES)
        {
            anyhow::bail!("invalid Cloud Node transfer chunk length");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != TRANSFER_MAGIC {
            anyhow::bail!("invalid Cloud Node transfer chunk magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != TRANSFER_VERSION {
            anyhow::bail!("unsupported Cloud Node transfer version {version}");
        }
        let kind = BlobKind::try_from(read_u8(&mut cursor)?)?;
        let resource = TransferResource::try_from(read_u8(&mut cursor)?)?;
        let final_chunk = match read_u8(&mut cursor)? {
            0 => false,
            1 => true,
            _ => anyhow::bail!("invalid Cloud Node transfer final-chunk flag"),
        };
        if read_u8(&mut cursor)? != 0 {
            anyhow::bail!("Cloud Node transfer reserved bits are non-zero");
        }
        let object_key = read_hash(&mut cursor)?;
        let content_sha256 = read_hash(&mut cursor)?;
        let resource_sha256 = read_hash(&mut cursor)?;
        let offset = read_u64(&mut cursor)?;
        let total_size = read_u64(&mut cursor)?;
        let data_len = read_u32(&mut cursor)? as usize;
        let data_sha256 = read_hash(&mut cursor)?;
        if data_len > MAX_TRANSFER_DATA_BYTES
            || bytes.len() != TRANSFER_HEADER_BYTES.saturating_add(data_len)
        {
            anyhow::bail!("Cloud Node transfer data length is invalid");
        }
        let mut data = vec![0u8; data_len];
        cursor.read_exact(&mut data)?;
        let chunk = Self {
            kind,
            resource,
            final_chunk,
            object_key,
            content_sha256,
            resource_sha256,
            offset,
            total_size,
            data_sha256,
            data,
        };
        chunk.validate()?;
        Ok(chunk)
    }

    pub fn into_frame(&self, session: [u8; 16]) -> anyhow::Result<Frame> {
        Ok(Frame {
            kind: FrameKind::ObjectChunk,
            session,
            payload: self.encode()?,
        })
    }

    pub fn from_frame(frame: &Frame) -> anyhow::Result<Self> {
        if frame.kind != FrameKind::ObjectChunk {
            anyhow::bail!("Cloud Node frame is not an object-transfer chunk");
        }
        Self::decode(&frame.payload)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_resource_kind(self.kind, self.resource)?;
        if self.data.len() > MAX_TRANSFER_DATA_BYTES {
            anyhow::bail!("Cloud Node transfer chunk exceeds {MAX_TRANSFER_DATA_BYTES} bytes");
        }
        let data_len = u64::try_from(self.data.len())
            .map_err(|_| anyhow::anyhow!("Cloud Node transfer chunk size exceeds u64"))?;
        let end = self
            .offset
            .checked_add(data_len)
            .ok_or_else(|| anyhow::anyhow!("Cloud Node transfer range overflow"))?;
        if self.offset > self.total_size || end > self.total_size {
            anyhow::bail!("Cloud Node transfer chunk exceeds declared resource size");
        }
        if self.total_size == 0 && (self.offset != 0 || !self.data.is_empty()) {
            anyhow::bail!("Cloud Node empty transfer resource has non-empty range");
        }
        if self.final_chunk != (end == self.total_size) {
            anyhow::bail!("Cloud Node transfer final-chunk flag does not match range");
        }
        let actual: [u8; 32] = Sha256::digest(&self.data).into();
        if actual != self.data_sha256 {
            anyhow::bail!("Cloud Node transfer chunk data hash mismatch");
        }
        if self.resource == TransferResource::FilePayload
            && self.resource_sha256 != self.content_sha256
        {
            anyhow::bail!("Cloud Node file payload hash must equal manifest content hash");
        }
        Ok(())
    }
}

fn validate_resource_kind(kind: BlobKind, resource: TransferResource) -> anyhow::Result<()> {
    match (kind, resource) {
        (_, TransferResource::Manifest)
        | (BlobKind::File, TransferResource::FilePayload)
        | (BlobKind::Video, TransferResource::VideoChunk) => Ok(()),
        _ => anyhow::bail!("Cloud Node transfer resource is invalid for blob kind"),
    }
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u8> {
    let mut bytes = [0u8; 1];
    cursor.read_exact(&mut bytes)?;
    Ok(bytes[0])
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

fn read_u64(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u64> {
    let mut bytes = [0u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_hash(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    cursor.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_chunk_round_trips_inside_protocol_frame() {
        let chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            [1u8; 32],
            [2u8; 32],
            [2u8; 32],
            0,
            4,
            vec![0, 1, 2, 3],
        )
        .unwrap();
        let frame = chunk.into_frame([9u8; 16]).unwrap();
        let decoded_frame = Frame::decode(&frame.encode().unwrap()).unwrap();
        assert_eq!(TransferChunk::from_frame(&decoded_frame).unwrap(), chunk);
    }

    #[test]
    fn tampered_chunk_bytes_are_rejected() {
        let chunk = TransferChunk::new(
            BlobKind::Video,
            TransferResource::VideoChunk,
            [1u8; 32],
            [2u8; 32],
            [3u8; 32],
            0,
            3,
            vec![7, 8, 9],
        )
        .unwrap();
        let mut encoded = chunk.encode().unwrap();
        *encoded.last_mut().unwrap() ^= 0xff;
        assert!(TransferChunk::decode(&encoded).is_err());
    }

    #[test]
    fn resource_kind_and_final_range_are_enforced() {
        assert!(TransferChunk::new(
            BlobKind::Folder,
            TransferResource::FilePayload,
            [1u8; 32],
            [2u8; 32],
            [2u8; 32],
            0,
            1,
            vec![1],
        )
        .is_err());

        let mut chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::Manifest,
            [1u8; 32],
            [2u8; 32],
            [3u8; 32],
            0,
            4,
            vec![1, 2],
        )
        .unwrap();
        chunk.final_chunk = true;
        assert!(chunk.validate().is_err());
    }
}
