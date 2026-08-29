//! The payment environment: the one environment allowed to touch
//! payment data, and the one required to keep it encrypted the whole
//! time it's in this process's hands.
//!
//! **Same caution as `vault::file_store`: this file contains hand-
//! written AES-256-GCM code. Treat it as requiring review before use
//! with real payment data.**

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use atomic_io::AtomicIo;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const VAULT_CALLER_IDENTITY: &str = "payment_environment";
const VAULT_KEY_NAME: &str = "payment.encryption_key";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedPayload {
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize)]
struct AuditRecord {
    ts_ms: u64,
    action: String,
    in_bytes: usize,
    out_bytes: usize,
}

pub struct PaymentEnvironment {
    io: AtomicIo,
    vault: Arc<vault::Vault>,
    audit_log_path: PathBuf,
}

impl PaymentEnvironment {
    pub fn new(io: AtomicIo, vault: Arc<vault::Vault>, data_dir: &Path) -> Self {
        Self {
            io,
            vault,
            audit_log_path: data_dir.join("payment-environment-audit.log"),
        }
    }

    fn encryption_key(&self) -> anyhow::Result<Zeroizing<[u8; 32]>> {
        let secret = self
            .vault
            .credential(VAULT_KEY_NAME, VAULT_CALLER_IDENTITY)
            .map_err(|e| anyhow::anyhow!("payment environment could not read its encryption key from vault: {e}"))?;

        let hex_key = secret.expose_secret();
        let bytes = Zeroizing::new(
            hex::decode(hex_key)
                .map_err(|e| anyhow::anyhow!("{VAULT_KEY_NAME} in vault is not valid hex: {e}"))?,
        );
        if bytes.len() != 32 {
            anyhow::bail!(
                "{VAULT_KEY_NAME} in vault is {} bytes, expected exactly 32 (AES-256 key size)",
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(Zeroizing::new(key))
    }

    pub fn process_encrypted<F>(
        &self,
        payload: &EncryptedPayload,
        work: F,
    ) -> anyhow::Result<EncryptedPayload>
    where
        F: FnOnce(&[u8]) -> anyhow::Result<Vec<u8>>,
    {
        let key = self.encryption_key()?;
        let plaintext = Zeroizing::new(decrypt(&key, payload)?);
        let in_len = plaintext.len();

        let result_plaintext = Zeroizing::new(work(&plaintext)?);
        let encrypted_result = encrypt(&key, &result_plaintext)?;

        self.audit("process", in_len, encrypted_result.ciphertext.len() / 2)?;
        Ok(encrypted_result)
    }

    pub fn send_details(&self, payload: &EncryptedPayload, destination_label: &str) -> anyhow::Result<()> {
        tracing::warn!(
            destination = destination_label,
            "PLACEHOLDER: send_details performs no real network call — no payment gateway integration exists yet. This call only proves the audit-logging wiring."
        );
        self.audit("send_details_stub", payload.ciphertext.len() / 2, 0)?;
        Ok(())
    }

    fn audit(&self, action: &str, in_bytes: usize, out_bytes: usize) -> anyhow::Result<()> {
        let record = AuditRecord {
            ts_ms: now_unix_ms(),
            action: action.to_string(),
            in_bytes,
            out_bytes,
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        self.io.append_locked(&self.audit_log_path, line.as_bytes())?;
        Ok(())
    }
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))
}

fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<EncryptedPayload> {
    use rand::RngCore;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher(key)
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("payment environment: encryption failed: {e}"))?;

    Ok(EncryptedPayload {
        nonce: hex::encode(nonce_bytes),
        ciphertext: hex::encode(ciphertext),
    })
}

fn decrypt(key: &[u8; 32], payload: &EncryptedPayload) -> anyhow::Result<Vec<u8>> {
    let nonce_bytes = hex::decode(&payload.nonce)?;
    let nonce_len = nonce_bytes.len();
    let nonce_bytes: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("payment environment: nonce is {nonce_len} bytes, expected exactly 12"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = hex::decode(&payload.ciphertext)?;

    cipher(key).decrypt(nonce, ciphertext.as_ref()).map_err(|e| {
        anyhow::anyhow!(
            "payment environment: decryption failed — wrong key, or the payload was tampered with in transit (GCM auth tag mismatch): {e}"
        )
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn round_trips_a_payload() {
        let key = test_key();
        let encrypted = encrypt(&key, b"card-details-placeholder").unwrap();
        let decrypted = decrypt(&key, &encrypted).unwrap();
        assert_eq!(decrypted, b"card-details-placeholder");
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let key = test_key();
        let mut encrypted = encrypt(&key, b"sensitive").unwrap();
        let mut bytes = hex::decode(&encrypted.ciphertext).unwrap();
        bytes[0] ^= 0xFF;
        encrypted.ciphertext = hex::encode(bytes);
        assert!(decrypt(&key, &encrypted).is_err());
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let encrypted = encrypt(&test_key(), b"sensitive").unwrap();
        let wrong_key = [9u8; 32];
        assert!(decrypt(&wrong_key, &encrypted).is_err());
    }

    #[test]
    fn malformed_nonce_is_an_error() {
        let key = test_key();
        let mut encrypted = encrypt(&key, b"sensitive").unwrap();
        encrypted.nonce = "00".repeat(8);
        assert!(decrypt(&key, &encrypted).is_err());
    }

    #[test]
    fn two_encryptions_never_share_a_nonce() {
        let key = test_key();
        let a = encrypt(&key, b"x").unwrap();
        let b = encrypt(&key, b"x").unwrap();
        assert_ne!(a.nonce, b.nonce);
    }
}
