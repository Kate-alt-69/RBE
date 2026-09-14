use std::io::{Cursor, Read};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};

const AUTH_MAGIC: &[u8; 8] = b"RBECNAU1";
const AUTH_VERSION: u16 = 1;
const AUTH_DOMAIN: &[u8] = b"RBE-CLOUD-NODE-AUTH/1\0";
const MAX_NODE_ID_BYTES: usize = 128;
pub const DEFAULT_AUTH_SKEW_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeProofKind {
    Knock = 1,
    Accept = 2,
}

impl TryFrom<u8> for NodeProofKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Knock,
            2 => Self::Accept,
            _ => anyhow::bail!("unknown Cloud Node proof kind {value}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeProof {
    pub kind: NodeProofKind,
    pub timestamp_ms: u64,
    pub session: [u8; 16],
    pub nonce: [u8; 32],
    pub peer_nonce: [u8; 32],
    pub node_id: String,
    pub signature: [u8; 64],
}

impl NodeProof {
    pub fn knock(
        signing: &SigningKey,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::signed(
            signing,
            NodeProofKind::Knock,
            node_id,
            timestamp_ms,
            session,
            nonce,
            [0u8; 32],
        )
    }

    pub fn accept(
        signing: &SigningKey,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
        peer_nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::signed(
            signing,
            NodeProofKind::Accept,
            node_id,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
        )
    }

    fn signed(
        signing: &SigningKey,
        kind: NodeProofKind,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
        peer_nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        validate_node_id(node_id)?;
        let mut proof = Self {
            kind,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
            node_id: node_id.to_string(),
            signature: [0u8; 64],
        };
        proof.signature = signing.sign(&proof.signing_digest()?).to_bytes();
        Ok(proof)
    }

    pub fn verify_identity(
        &self,
        expected_node_id: &str,
        public_key_hex: &str,
    ) -> anyhow::Result<()> {
        if self.node_id != expected_node_id {
            anyhow::bail!("Cloud Node proof identity does not match configured peer");
        }
        validate_node_id(&self.node_id)?;
        let public = hex::decode(public_key_hex)
            .map_err(|_| anyhow::anyhow!("Cloud Node public key is not hexadecimal"))?;
        let public: [u8; 32] = public
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cloud Node public key must contain exactly 32 bytes"))?;
        let verifying = VerifyingKey::from_bytes(&public)
            .map_err(|_| anyhow::anyhow!("Cloud Node public key is invalid"))?;
        let signature = Signature::from_bytes(&self.signature);
        verifying
            .verify(&self.signing_digest()?, &signature)
            .map_err(|_| anyhow::anyhow!("Cloud Node proof signature verification failed"))
    }

    pub fn verify_freshness(&self, now_ms: u64, max_skew_ms: u64) -> anyhow::Result<()> {
        if self.timestamp_ms.abs_diff(now_ms) > max_skew_ms {
            anyhow::bail!("Cloud Node proof is outside the accepted clock window");
        }
        Ok(())
    }

    pub fn verify_accepts(&self, knock: &Self) -> anyhow::Result<()> {
        if self.kind != NodeProofKind::Accept || knock.kind != NodeProofKind::Knock {
            anyhow::bail!("Cloud Node proof kind is invalid for challenge response");
        }
        if self.session != knock.session || self.peer_nonce != knock.nonce {
            anyhow::bail!("Cloud Node accept proof is not bound to the initiating knock");
        }
        Ok(())
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let node = self.node_id.as_bytes();
        validate_node_id(&self.node_id)?;
        let node_len = u16::try_from(node.len())
            .map_err(|_| anyhow::anyhow!("Cloud Node id exceeds proof format"))?;
        let mut out = Vec::with_capacity(166 + node.len());
        out.extend_from_slice(AUTH_MAGIC);
        out.extend_from_slice(&AUTH_VERSION.to_be_bytes());
        out.push(self.kind as u8);
        out.push(0);
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        out.extend_from_slice(&self.session);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.peer_nonce);
        out.extend_from_slice(&node_len.to_be_bytes());
        out.extend_from_slice(node);
        out.extend_from_slice(&self.signature);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() < 166 || bytes.len() > 166 + MAX_NODE_ID_BYTES {
            anyhow::bail!("invalid Cloud Node proof length");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != AUTH_MAGIC {
            anyhow::bail!("invalid Cloud Node proof magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != AUTH_VERSION {
            anyhow::bail!("unsupported Cloud Node proof version {version}");
        }
        let kind = NodeProofKind::try_from(read_u8(&mut cursor)?)?;
        if read_u8(&mut cursor)? != 0 {
            anyhow::bail!("Cloud Node proof reserved bits are non-zero");
        }
        let timestamp_ms = read_u64(&mut cursor)?;
        let session = read_array::<16>(&mut cursor)?;
        let nonce = read_array::<32>(&mut cursor)?;
        let peer_nonce = read_array::<32>(&mut cursor)?;
        let node_len = read_u16(&mut cursor)? as usize;
        if node_len == 0 || node_len > MAX_NODE_ID_BYTES {
            anyhow::bail!("invalid Cloud Node proof node id length");
        }
        let expected = 166usize.saturating_add(node_len);
        if bytes.len() != expected {
            anyhow::bail!("Cloud Node proof length does not match node id length");
        }
        let mut node = vec![0u8; node_len];
        cursor.read_exact(&mut node)?;
        let node_id = String::from_utf8(node)
            .map_err(|_| anyhow::anyhow!("Cloud Node proof node id is not UTF-8"))?;
        validate_node_id(&node_id)?;
        let signature = read_array::<64>(&mut cursor)?;
        Ok(Self {
            kind,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
            node_id,
            signature,
        })
    }

    fn signing_digest(&self) -> anyhow::Result<[u8; 32]> {
        validate_node_id(&self.node_id)?;
        let mut digest = Sha256::new();
        digest.update(AUTH_DOMAIN);
        digest.update([self.kind as u8]);
        digest.update(self.timestamp_ms.to_be_bytes());
        digest.update(self.session);
        digest.update(self.nonce);
        digest.update(self.peer_nonce);
        digest.update((self.node_id.len() as u32).to_be_bytes());
        digest.update(self.node_id.as_bytes());
        Ok(digest.finalize().into())
    }
}

pub fn random_session_and_nonce() -> ([u8; 16], [u8; 32]) {
    let mut session = [0u8; 16];
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut session);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    (session, nonce)
}

fn validate_node_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > MAX_NODE_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("Cloud Node id must use 1..=128 ASCII [A-Za-z0-9_.-] characters");
    }
    Ok(())
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u8> {
    Ok(read_array::<1>(cursor)?[0])
}

fn read_u16(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u16> {
    Ok(u16::from_be_bytes(read_array::<2>(cursor)?))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u64> {
    Ok(u64::from_be_bytes(read_array::<8>(cursor)?))
}

fn read_array<const N: usize>(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<[u8; N]> {
    let mut value = [0u8; N];
    cursor.read_exact(&mut value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutual_proofs_bind_identity_session_nonce_and_time() {
        let client = SigningKey::from_bytes(&[7u8; 32]);
        let server = SigningKey::from_bytes(&[9u8; 32]);
        let (session, client_nonce) = random_session_and_nonce();
        let (_, server_nonce) = random_session_and_nonce();
        let knock = NodeProof::knock(&client, "nas-main", 50_000, session, client_nonce).unwrap();
        let encoded = knock.encode().unwrap();
        let knock = NodeProof::decode(&encoded).unwrap();
        knock
            .verify_identity("nas-main", &hex::encode(client.verifying_key().to_bytes()))
            .unwrap();
        knock
            .verify_freshness(50_500, DEFAULT_AUTH_SKEW_MS)
            .unwrap();

        let accept = NodeProof::accept(
            &server,
            "render-main",
            50_600,
            session,
            server_nonce,
            client_nonce,
        )
        .unwrap();
        let accept = NodeProof::decode(&accept.encode().unwrap()).unwrap();
        accept.verify_accepts(&knock).unwrap();
        accept
            .verify_identity(
                "render-main",
                &hex::encode(server.verifying_key().to_bytes()),
            )
            .unwrap();
        assert!(accept
            .verify_identity("attacker", &hex::encode(server.verifying_key().to_bytes()))
            .is_err());
    }
}
