//! The payment environment: the one environment allowed to touch
//! payment data, and the one required to keep it encrypted the whole
//! time it's in this process's hands.
//!
//! **Same caution as `vault::file_store`: this file contains hand-
//! written AES-256-GCM code, verified only by the round-trip/tamper
//! tests below, with no compiler available to check it against.
//! Cryptographic code is where subtle mistakes are most dangerous —
//! treat this as needing real review before trusting it with real
//! payment data, not as production-ready because it compiles.**
//!
//! Duplicates the encrypt/decrypt shape from `vault::file_store`
//! rather than sharing it — different purpose (in-flight request/
//! response payloads here, at-rest credentials there), different
//! workspace, and small enough that duplicating a ~30-line pattern
//! beats introducing a shared crypto-helper crate for one caller on
//! each side. Worth revisiting if a third caller shows up.
//!
//! **`send_details` is an explicit, clearly-marked stub.** There is no
//! real payment gateway integration in this codebase — no gateway
//! credentials, no API client, nothing. Calling it does not send
//! anything anywhere. It exists to prove the encryption boundary and
//! audit-logging wiring end to end, so the real integration has a
//! tested shape to slot into rather than being built from nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use atomic_io::AtomicIo;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// A caller-identity string for the vault's ACL — the payment
/// environment reads its encryption key as this identity, so
/// `data/admin/vault-acl.json` must explicitly grant it, same
/// fail-closed rule as any other vault access.
pub const VAULT_CALLER_IDENTITY: &str = "payment_environment";
const VAULT_KEY_NAME: &str = "payment.encryption_key";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedPayload {
    /// Hex-encoded 12-byte nonce — fresh per encryption call, never
    /// reused (see `vault::file_store`'s identical rule and why it
    /// matters for GCM).
    nonce: String,
    /// Hex-encoded ciphertext, GCM auth tag included.
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
    /// `data_dir` should be the same `./data/admin`-style directory
    /// used elsewhere — the audit log lands at
    /// `<data_dir>/payment-environment-audit.log`.
    pub fn new(io: AtomicIo, vault: Arc<vault::Vault>, data_dir: &Path) -> Self {
        Self {
            io,
            vault,
            audit_log_path: data_dir.join("payment-environment-audit.log"),
        }
    }

    fn encryption_key(&self) -> anyhow::Result<[u8; 32]> {
        let secret = self
            .vault
            .credential(VAULT_KEY_NAME, VAULT_CALLER_IDENTITY)
            .map_err(|e| anyhow::anyhow!("payment environment could not read its encryption key from vault: {e}"))?;

        let hex_key = secret.expose_secret();
        let bytes = hex::decode(hex_key)
            .map_err(|e| anyhow::anyhow!("{VAULT_KEY_NAME} in vault is not valid hex: {e}"))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "{VAULT_KEY_NAME} in vault is {} bytes, expected exactly 32 (AES-256 key size)",
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    /// Decrypts `payload`, runs `work` on the plaintext, encrypts
    /// whatever `work` returns. The plaintext exists only as a local
    /// buffer for the duration of this call — zeroized before it's
    /// dropped, never written to disk — which is what "fully
    /// encrypted to do its work" means concretely: the boundary of
    /// this environment is encrypted in, encrypted out, and the
    /// in-between is deliberately as short-lived as possible.
    pub fn process_encrypted<F>(
        &self,
        payload: &EncryptedPayload,
        work: F,
    ) -> anyhow::Result<EncryptedPayload>
    where
        F: FnOnce(&[u8]) -> anyhow::Result<Vec<u8>>,
    {
        let key = self.encryption_key()?;

        let mut plaintext = decrypt(&key, payload)?;
        let in_len = plaintext.len();

        let work_result = work(&plaintext);
        plaintext.zeroize();
        let mut result_plaintext = work_result?;

        let encrypted_result = encrypt(&key, &result_plaintext);
        result_plaintext.zeroize();
        let encrypted_result = encrypted_result?;

        // `/2` because `ciphertext` is hex-encoded (2 chars per byte)
        // — audit log should record actual bytes, not encoded-string
        // length.
        self.audit("process", in_len, encrypted_result.ciphertext.len() / 2)?;

        Ok(encrypted_result)
    }

    /// **Stub — see this module's doc comment.** Does not send
    /// anything anywhere. Audit-logs the intent and returns.
    pub fn send_details(&self, payload: &EncryptedPayload, destination_label: &str) -> anyhow::Result<()> {
        tracing::warn!(
            destination = destination_label,
            "PLACEHOLDER: send_details performs no real network call — no payment gateway \
             integration exists yet. This call only proves the audit-logging wiring."
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
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = hex::decode(&payload.ciphertext)?;

    cipher(key).decrypt(nonce, ciphertext.as_ref()).map_err(|e| {
        anyhow::anyhow!(
            "payment environment: decryption failed — wrong key, or the payload was tampered \
             with in transit (GCM auth tag mismatch): {e}"
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
    fn two_encryptions_never_share_a_nonce() {
        let key = test_key();
        let a = encrypt(&key, b"x").unwrap();
        let b = encrypt(&key, b"x").unwrap();
        assert_ne!(a.nonce, b.nonce);
    }
}
