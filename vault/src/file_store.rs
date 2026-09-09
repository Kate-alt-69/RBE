//! Fallback credential store for when the OS credential store isn't
//! available (headless Linux with no Secret Service daemon is the
//! realistic case — see `lib.rs`'s startup probe). AES-256-GCM,
//! per-entry nonce, with the master key supplied from outside the
//! data directory rather than persisted beside the ciphertext.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const LEGACY_MASTER_KEY_FILE: &str = "vault-master.key";

#[derive(Serialize, Deserialize, Clone)]
struct StoredEntry {
    nonce: String,
    ciphertext: String,
}

pub struct FileStore {
    io: AtomicIo,
    store_path: PathBuf,
    key: Zeroizing<[u8; 32]>,
}

impl FileStore {
    pub fn open(io: AtomicIo, dir: &Path, master_key_hex: &str) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)?;
        let store_path = dir.join("vault-store.json");
        let legacy_key_path = dir.join(LEGACY_MASTER_KEY_FILE);
        let key = parse_master_key(master_key_hex, "externally supplied vault fallback key")?;
        migrate_legacy_key(&legacy_key_path, &store_path, &key)?;

        let store = Self {
            io,
            store_path,
            key,
        };
        store.validate_existing_entries()?;
        Ok(store)
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

    fn validate_existing_entries(&self) -> anyhow::Result<()> {
        for (name, entry) in self.load_map()? {
            self.decrypt(&entry).map_err(|error| {
                anyhow::anyhow!(
                    "vault fallback credential {name:?} could not be decrypted with the supplied master key: {error}"
                )
            })?;
        }
        Ok(())
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*self.key))
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
        let nonce_bytes: [u8; 12] = nonce_bytes.try_into().map_err(|_| {
            anyhow::anyhow!("vault: stored nonce is {nonce_len} bytes, expected exactly 12")
        })?;
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

fn parse_master_key(value: &str, label: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let trimmed = value.trim();
    let bytes = Zeroizing::new(
        hex::decode(trimmed)
            .map_err(|error| anyhow::anyhow!("{label} is not valid hexadecimal: {error}"))?,
    );
    if bytes.len() != 32 {
        anyhow::bail!(
            "{label} is {} bytes, expected exactly 32 (64 hexadecimal characters)",
            bytes.len()
        );
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(Zeroizing::new(key))
}

fn migrate_legacy_key(
    key_path: &Path,
    store_path: &Path,
    supplied_key: &[u8; 32],
) -> anyhow::Result<()> {
    let existing = match fs::read_to_string(key_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to inspect legacy vault master key {}: {error}",
                key_path.display()
            ));
        }
    };

    if store_path.exists() {
        let legacy = parse_master_key(&existing, "legacy vault master key")?;
        if &*legacy != supplied_key {
            anyhow::bail!(
                "legacy vault credential store {} is protected by a different master key; refusing to delete {} or open the store with the supplied key",
                store_path.display(),
                key_path.display()
            );
        }
    }

    fs::remove_file(key_path).map_err(|error| {
        anyhow::anyhow!(
            "failed to remove legacy plaintext vault master key {}: {error}",
            key_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vault-filestore-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn test_key(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    #[test]
    fn round_trips_a_value_without_writing_master_key() {
        let dir = temp_dir("roundtrip");
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
        store
            .set("db.password", "correct horse battery staple")
            .unwrap();
        let value = store.get("db.password").unwrap();
        assert_eq!(value, "correct horse battery staple");
        assert!(!dir.join(LEGACY_MASTER_KEY_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_credential_is_a_clear_error_not_a_panic() {
        let dir = temp_dir("missing");
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
        assert!(store.get("does.not.exist").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let dir = temp_dir("tamper");
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
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
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
        store.set("secret", "value").unwrap();
        let mut map = store.load_map().unwrap();
        map.get_mut("secret").unwrap().nonce = "00".repeat(8);
        store.save_map(&map).unwrap();
        assert!(store.get("secret").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_external_key_fails_during_open() {
        let dir = temp_dir("wrong-key");
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
        store.set("secret", "value").unwrap();
        drop(store);
        assert!(FileStore::open(AtomicIo::new(), &dir, &test_key(9)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_legacy_key_is_removed_after_migration() {
        let dir = temp_dir("legacy-match");
        fs::create_dir_all(&dir).unwrap();
        let key = test_key(7);
        fs::write(dir.join(LEGACY_MASTER_KEY_FILE), &key).unwrap();
        let store = FileStore::open(AtomicIo::new(), &dir, &key).unwrap();
        assert!(!dir.join(LEGACY_MASTER_KEY_FILE).exists());
        store.set("secret", "value").unwrap();
        assert_eq!(store.get("secret").unwrap(), "value");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_legacy_key_preserves_existing_store_and_key() {
        let dir = temp_dir("legacy-mismatch");
        let old_key = test_key(7);
        let store = FileStore::open(AtomicIo::new(), &dir, &old_key).unwrap();
        store.set("secret", "value").unwrap();
        drop(store);
        fs::write(dir.join(LEGACY_MASTER_KEY_FILE), &old_key).unwrap();

        assert!(FileStore::open(AtomicIo::new(), &dir, &test_key(9)).is_err());
        assert!(dir.join(LEGACY_MASTER_KEY_FILE).exists());
        assert!(dir.join("vault-store.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_entries_never_share_a_nonce() {
        let dir = temp_dir("nonce-uniqueness");
        let store = FileStore::open(AtomicIo::new(), &dir, &test_key(7)).unwrap();
        store.set("a", "value-a").unwrap();
        store.set("b", "value-b").unwrap();
        let map = store.load_map().unwrap();
        assert_ne!(map.get("a").unwrap().nonce, map.get("b").unwrap().nonce);
        let _ = fs::remove_dir_all(&dir);
    }
}
