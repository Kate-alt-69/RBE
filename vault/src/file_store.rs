//! Fallback credential store for when the OS credential store isn't
//! available (headless Linux with no Secret Service daemon is the
//! realistic case — see `lib.rs`'s startup probe). AES-256-GCM,
//! per-entry nonce, matching the encryption scheme named in the
//! design discussion.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
struct StoredEntry {
    nonce: String,
    ciphertext: String,
}

pub struct FileStore {
    io: AtomicIo,
    store_path: PathBuf,
    key: [u8; 32],
}

impl FileStore {
    pub fn open(io: AtomicIo, dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)?;
        let store_path = dir.join("vault-store.json");
        let key_path = dir.join("vault-master.key");
        let key = read_or_create_key(&io, &key_path, &store_path)?;
        Ok(Self { io, store_path, key })
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
        let nonce_len = nonce_bytes.len();
        let nonce_bytes: [u8; 12] = nonce_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("vault: stored nonce is {nonce_len} bytes, expected exactly 12"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = hex::decode(&entry.ciphertext)?;

        let plaintext = self
            .cipher()
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|e| {
                anyhow::anyhow!(
                    "vault: decryption failed — wrong key, or the stored entry was tampered with (GCM auth tag mismatch): {e}"
                )
            })?;

        Ok(String::from_utf8(plaintext)?)
    }
}

fn read_or_create_key(io: &AtomicIo, key_path: &Path, store_path: &Path) -> anyhow::Result<[u8; 32]> {
    match fs::read_to_string(key_path) {
        Ok(existing) => {
            let trimmed = existing.trim();
            let bytes = hex::decode(trimmed)
                .map_err(|error| anyhow::anyhow!("vault master key {} is malformed: {error}", key_path.display()))?;
            if bytes.len() != 32 {
                anyhow::bail!(
                    "vault master key {} is {} bytes, expected exactly 32; refusing to replace a key that may protect existing credentials",
                    key_path.display(),
                    bytes.len()
                );
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!("failed to read vault master key {}: {error}", key_path.display()));
        }
    }

    if store_path.exists() {
        anyhow::bail!(
            "vault credential store {} exists but master key {} is missing; refusing to generate a replacement key that would make existing credentials unrecoverable",
            store_path.display(),
            key_path.display()
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
    fn malformed_nonce_is_an_error_not_a_panic() {
        let dir = temp_dir("bad-nonce");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        store.set("secret", "value").unwrap();
        let mut map = store.load_map().unwrap();
        map.get_mut("secret").unwrap().nonce = "00".repeat(8);
        store.save_map(&map).unwrap();
        assert!(store.get("secret").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_store_without_key_fails_closed() {
        let dir = temp_dir("missing-master-key");
        let store = FileStore::open(AtomicIo::new(), &dir).unwrap();
        store.set("secret", "value").unwrap();
        drop(store);
        fs::remove_file(dir.join("vault-master.key")).unwrap();
        assert!(FileStore::open(AtomicIo::new(), &dir).is_err());
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
