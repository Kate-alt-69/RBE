use std::io::{Cursor, Read};

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
