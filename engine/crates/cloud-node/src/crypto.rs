use std::io::Read;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub const CLOUD_NODE_PRIVATE_KEY_ENV: &str = "RBE_CLOUD_NODE_PRIVATE_KEY";
const CHALLENGE_DOMAIN: &[u8] = b"RBE-CLOUD-NODE-CHALLENGE/1";
const MAX_PRIVATE_KEY_FILE_BYTES: u64 = 4096;

pub fn load_signing_key_from_env() -> anyhow::Result<SigningKey> {
    let value = std::env::var(CLOUD_NODE_PRIVATE_KEY_ENV).map_err(|_| {
        anyhow::anyhow!(
            "{CLOUD_NODE_PRIVATE_KEY_ENV} must contain this node's 32-byte hexadecimal private key or file:/absolute/path"
        )
    })?;
    load_signing_key_value(&value)
}

fn load_signing_key_value(value: &str) -> anyhow::Result<SigningKey> {
    let resolved = resolve_signing_key_value(value)?;
    let bytes = hex::decode(&resolved)
        .map_err(|_| anyhow::anyhow!("{CLOUD_NODE_PRIVATE_KEY_ENV} must resolve to hexadecimal"))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!("{CLOUD_NODE_PRIVATE_KEY_ENV} must resolve to exactly 32 bytes")
    })?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn resolve_signing_key_value(value: &str) -> anyhow::Result<String> {
    let Some(file_name) = value.strip_prefix("file:") else {
        return Ok(value.to_owned());
    };
    if file_name.is_empty() {
        anyhow::bail!("{CLOUD_NODE_PRIVATE_KEY_ENV} has an empty file: path");
    }
    let path = Path::new(file_name);
    if !path.is_absolute() {
        anyhow::bail!("{CLOUD_NODE_PRIVATE_KEY_ENV} file: path must be absolute");
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect Cloud Node private-key file {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "Cloud Node private-key source {} must resolve to a regular file",
            path.display()
        );
    }
    let file = std::fs::File::open(path).map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node private-key file {}: {error}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node private-key file {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_PRIVATE_KEY_FILE_BYTES {
        anyhow::bail!(
            "Cloud Node private-key file {} exceeds {} bytes",
            path.display(),
            MAX_PRIVATE_KEY_FILE_BYTES
        );
    }
    let value = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "Cloud Node private-key file {} is not UTF-8",
            path.display()
        )
    })?;
    let value = value.trim_end_matches(&['\r', '\n'][..]);
    if value.is_empty() {
        anyhow::bail!("Cloud Node private-key file {} is empty", path.display());
    }
    Ok(value.to_owned())
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
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn signing_key_can_be_loaded_from_bounded_absolute_secret_file() {
        let expected = SigningKey::from_bytes(&[13u8; 32]);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-cn-private-key-{}-{unique}.secret",
            std::process::id()
        ));
        fs::write(&path, format!("{}\n", hex::encode(expected.to_bytes()))).unwrap();

        let loaded = load_signing_key_value(&format!("file:{}", path.display())).unwrap();
        assert_eq!(loaded.to_bytes(), expected.to_bytes());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn private_key_file_indirection_rejects_relative_oversized_and_non_file_sources() {
        assert!(load_signing_key_value("file:relative.secret").is_err());

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-cn-private-key-too-large-{}-{unique}.secret",
            std::process::id()
        ));
        fs::write(&path, vec![b'a'; (MAX_PRIVATE_KEY_FILE_BYTES + 1) as usize]).unwrap();
        assert!(load_signing_key_value(&format!("file:{}", path.display())).is_err());
        let _ = fs::remove_file(path);

        let directory = std::env::temp_dir().join(format!(
            "rbe-cn-private-key-directory-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        assert!(load_signing_key_value(&format!("file:{}", directory.display())).is_err());
        let _ = fs::remove_dir_all(directory);
    }
}
