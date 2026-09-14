use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
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
