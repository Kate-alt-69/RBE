//! Fallback credential store for when the OS credential store isn't
//! available (headless Linux with no Secret Service daemon is the
//! realistic case — see `lib.rs`'s startup probe). AES-256-GCM,
//! per-entry nonce, matching the encryption scheme named in the
//! design discussion.
//!
//! **This is the highest-risk file in this crate, and arguably in the
//! whole codebase so far.** Cryptographic code is exactly where subtle
//! mistakes are most dangerous (nonce reuse, wrong key handling), and
//! this was written with no compiler available to verify it against —
//! see the round-trip and tamper-detection tests below, which are the
//! best verification available right now, but **do not treat this as
//! production-ready until it's actually been built, and ideally
//! reviewed by someone who does cryptographic code review, not just
//! compiled successfully.**

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
struct StoredEntry {
    /// Hex-encoded 12-byte nonce. Generated fresh per encryption call
    /// (see `encrypt`) — never reused across entries or across
    /// updates to the same entry, which is the one hard rule GCM mode
    /// requires to stay secure.
    nonce: String,
    /// Hex-encoded ciphertext (includes the GCM auth tag).
    ciphertext: String,
}

pub struct FileStore {
    io: AtomicIo,
    store_path: PathBuf,
    key: [u8; 32],
}

impl FileStore {
    /// `io` is meant to be the same shared `AtomicIo` instance used
    /// elsewhere in the process (constructed once in `main.rs`), not a
    /// fresh one per call — sharing it means the lock registry and
    /// stats actually reflect *all* disk I/O, not just this store's.
    pub fn open(io: AtomicIo, dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)?;
        let key = read_or_create_key(&io, &dir.join("vault-master.key"))?;
        Ok(Self {
            io,
            store_path: dir.join("vault-store.json"),
            key,
        })
    }

    pub fn get(&self, name: &str) -> anyhow::Result<String> {
        let map = self.load_map()?;
        let entry = map
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("credential {name:?} not found in fallback store"))?;
        self.decrypt(entry)
    }

    pub fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let mut map = self.load_map()?;
        let entry = self.encrypt(value)?;
        map.insert(name.to_string(), entry);
        self.save_map(&map)
    }

    fn load_map(&self) -> anyhow::Result<HashMap<String, StoredEntry>> {
        if !self.store_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = self.io.read(&self.store_path)?;
        Ok(serde_json::from_slice(&raw)?)
    }

    fn save_map(&self, map: &HashMap<String, StoredEntry>) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(map)?;
        self.io.write_atomic(&self.store_path, json.as_bytes())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&self.store_path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&self.store_path, perms)?;
        }

        Ok(())
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key))
    }

    fn encrypt(&self, plaintext: &str) -> anyhow::Result<StoredEntry> {
        use rand::RngCore;
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher()
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("vault: encryption failed: {e}"))?;

        Ok(StoredEntry {
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(ciphertext),
        })
    }

    fn decrypt(&self, entry: &StoredEntry) -> anyhow::Result<String> {
        let nonce_bytes = hex::decode(&entry.nonce)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = hex::decode(&entry.ciphertext)?;

        let plaintext = self
            .cipher()
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|e| {
                anyhow::anyhow!(
                    "vault: decryption failed — wrong key, or the stored entry was tampered \
                     with (GCM auth tag mismatch): {e}"
                )
            })?;

        Ok(String::from_utf8(plaintext)?)
    }
}

fn read_or_create_key(io: &AtomicIo, key_path: &Path) -> anyhow::Result<[u8; 32]> {
    if let Ok(existing) = fs::read_to_string(key_path) {
        let trimmed = existing.trim();
        if let Ok(bytes) = hex::decode(trimmed) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
        }
        tracing::warn!(
            path = %key_path.display(),
            "vault master key file exists but is malformed — regenerating (previously \
             encrypted entries will become unreadable)"
        );
    }

    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    io.write_atomic(key_path, hex::encode(key).as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(key_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(key_path, perms)?;
    }

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vault-filestore-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_a_value() {
        let dir = temp_dir("roundtrip");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        store.set("db.password", "correct horse battery staple").unwrap();
        let value = store.get("db.password").unwrap();
        assert_eq!(value, "correct horse battery staple");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_key_is_a_clear_error_not_a_panic() {
        let dir = temp_dir("missing");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        assert!(store.get("does.not.exist").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let dir = temp_dir("tamper");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        store.set("secret", "sensitive-value").unwrap();

        // Directly corrupt the stored ciphertext on disk and confirm
        // decryption fails loudly (GCM auth tag check) rather than
        // silently returning garbage.
        let mut map = store.load_map().unwrap();
        let entry = map.get_mut("secret").unwrap();
        let mut bytes = hex::decode(&entry.ciphertext).unwrap();
        bytes[0] ^= 0xFF;
        entry.ciphertext = hex::encode(bytes);
        store.save_map(&map).unwrap();

        assert!(store.get("secret").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_entries_never_share_a_nonce() {
        let dir = temp_dir("nonce-uniqueness");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        store.set("a", "value-a").unwrap();
        store.set("b", "value-b").unwrap();
        let map = store.load_map().unwrap();
        assert_ne!(map.get("a").unwrap().nonce, map.get("b").unwrap().nonce);
        let _ = fs::remove_dir_all(&dir);
    }
}
