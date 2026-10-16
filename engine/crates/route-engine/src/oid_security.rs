use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use atomic_io::AtomicIo;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::service_oid::OidError;

const INDEX_ENVELOPE_PROTOCOL: &str = "RBE-OID-PROTECTED-INDEX/1";
const HEAD_PROTOCOL: &str = "RBE-OID-VAULT-HEAD/1";
const INDEX_AAD: &[u8] = b"RBE-OID-INDEX/1";

pub trait OidVaultAuthority: Send + Sync {
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String>;
    fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String>;
    fn trusted_head(&self) -> Result<Option<String>, String>;
    fn set_trusted_head(&self, head: &str) -> Result<(), String>;
}

#[derive(Clone)]
pub(crate) struct OidSecurity {
    inner: Arc<OidSecurityInner>,
}

struct OidSecurityInner {
    authority: Arc<dyn OidVaultAuthority>,
    _lease: Arc<File>,
    record_sha256: Mutex<BTreeMap<u16, String>>,
    head_generation: Mutex<Option<u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProtectedIndexEnvelope {
    protocol: String,
    index_hex: String,
    record_sha256: BTreeMap<u16, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OidTrustedHead {
    protocol: String,
    generation: u64,
    sealed_index_sha256: String,
}

impl OidSecurity {
    pub(crate) fn acquire(
        project_root: &Path,
        authority: Arc<dyn OidVaultAuthority>,
    ) -> Result<Self, OidError> {
        let compiler_root = project_root.join(".cache/compiler");
        fs::create_dir_all(&compiler_root)?;
        let lock_path = compiler_root.join("oid.lock");
        let lease = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lease.try_lock_exclusive().map_err(|error| {
            OidError::Locked(format!(
                "OID compiler cache is locked by another RBE/RELC process ({}): {error}",
                lock_path.display()
            ))
        })?;
        Ok(Self {
            inner: Arc::new(OidSecurityInner {
                authority,
                _lease: Arc::new(lease),
                record_sha256: Mutex::new(BTreeMap::new()),
                head_generation: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn open_index(&self, root: &Path, io: &AtomicIo) -> Result<Vec<u8>, OidError> {
        let sealed_index = io.read(&root.join("index"))?;
        let raw_head = self
            .inner
            .authority
            .trusted_head()
            .map_err(vault_error)?
            .ok_or_else(|| {
                OidError::Security("Vault has no trusted OID head for an existing cache".into())
            })?;
        let head: OidTrustedHead = serde_json::from_str(&raw_head)
            .map_err(|error| OidError::Security(format!("Vault OID head is malformed: {error}")))?;
        if head.protocol != HEAD_PROTOCOL {
            return Err(OidError::Security(format!(
                "unsupported Vault OID head protocol {:?}",
                head.protocol
            )));
        }
        if sha256_hex(&sealed_index) != head.sealed_index_sha256 {
            return Err(OidError::Security(
                "OID index does not match the Vault trusted head (tamper or replay detected)"
                    .into(),
            ));
        }
        let plaintext = self
            .inner
            .authority
            .open(INDEX_AAD, &sealed_index)
            .map_err(|message| {
                OidError::Security(format!("Vault rejected protected OID index: {message}"))
            })?;
        let envelope: ProtectedIndexEnvelope =
            serde_json::from_slice(&plaintext).map_err(|error| {
                OidError::Security(format!(
                    "protected OID index envelope is malformed: {error}"
                ))
            })?;
        if envelope.protocol != INDEX_ENVELOPE_PROTOCOL {
            return Err(OidError::Security(format!(
                "unsupported protected OID index protocol {:?}",
                envelope.protocol
            )));
        }
        let index = hex::decode(&envelope.index_hex).map_err(|error| {
            OidError::Security(format!("protected OID index bytes are invalid: {error}"))
        })?;
        *self.record_hashes()? = envelope.record_sha256;
        *self.head_generation()? = Some(head.generation);
        Ok(index)
    }

    pub(crate) fn verify_generation(&self, generation: u64) -> Result<(), OidError> {
        let observed = *self.head_generation()?;
        if observed != Some(generation) {
            return Err(OidError::Security(format!(
                "Vault trusted OID generation {:?} does not match index generation {generation}",
                observed
            )));
        }
        Ok(())
    }

    pub(crate) fn reset(&self) -> Result<(), OidError> {
        self.record_hashes()?.clear();
        *self.head_generation()? = None;
        Ok(())
    }

    pub(crate) fn write_index(
        &self,
        root: &Path,
        io: &AtomicIo,
        index_plaintext: &[u8],
        generation: u64,
    ) -> Result<(), OidError> {
        fs::create_dir_all(root)?;
        let envelope = ProtectedIndexEnvelope {
            protocol: INDEX_ENVELOPE_PROTOCOL.to_string(),
            index_hex: hex::encode(index_plaintext),
            record_sha256: self.record_hashes()?.clone(),
        };
        let encoded = serde_json::to_vec(&envelope)
            .map_err(|error| OidError::Invariant(format!("encode protected OID index: {error}")))?;
        let sealed = self
            .inner
            .authority
            .seal(INDEX_AAD, &encoded)
            .map_err(vault_error)?;
        io.write_atomic(&root.join("index"), &sealed)?;
        let head = OidTrustedHead {
            protocol: HEAD_PROTOCOL.to_string(),
            generation,
            sealed_index_sha256: sha256_hex(&sealed),
        };
        let raw = serde_json::to_string(&head)
            .map_err(|error| OidError::Invariant(format!("encode Vault OID head: {error}")))?;
        self.inner
            .authority
            .set_trusted_head(&raw)
            .map_err(vault_error)?;
        *self.head_generation()? = Some(generation);
        Ok(())
    }

    pub(crate) fn record_matches(
        &self,
        root: &Path,
        io: &AtomicIo,
        oid: u16,
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<bool, OidError> {
        let digest = sha256_hex(plaintext);
        if self.record_hashes()?.get(&oid) != Some(&digest) {
            return Ok(false);
        }
        let path = root.join(oid.to_string());
        if !path.is_file() {
            return Ok(false);
        }
        let sealed = io.read(&path)?;
        let opened = self.inner.authority.open(aad, &sealed).map_err(|message| {
            OidError::Security(format!("Vault rejected protected OID {oid}: {message}"))
        })?;
        Ok(opened == plaintext)
    }

    pub(crate) fn stage_record_write(
        &self,
        root: &Path,
        io: &AtomicIo,
        oid: u16,
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<(), OidError> {
        let sealed = self
            .inner
            .authority
            .seal(aad, plaintext)
            .map_err(vault_error)?;
        io.write_atomic(&root.join(oid.to_string()), &sealed)?;
        self.record_hashes()?.insert(oid, sha256_hex(plaintext));
        Ok(())
    }

    pub(crate) fn open_record(
        &self,
        root: &Path,
        io: &AtomicIo,
        oid: u16,
        aad: &[u8],
    ) -> Result<Vec<u8>, OidError> {
        let expected = self.record_hashes()?.get(&oid).cloned().ok_or_else(|| {
            OidError::Security(format!(
                "OID {oid} is not present in the Vault-trusted index envelope"
            ))
        })?;
        let sealed = io.read(&root.join(oid.to_string()))?;
        let plaintext = self.inner.authority.open(aad, &sealed).map_err(|message| {
            OidError::Security(format!("Vault rejected protected OID {oid}: {message}"))
        })?;
        if sha256_hex(&plaintext) != expected {
            return Err(OidError::Security(format!(
                "OID {oid} plaintext digest does not match the Vault-trusted index envelope"
            )));
        }
        Ok(plaintext)
    }

    pub(crate) fn stage_record_remove(&self, root: &Path, oid: u16) -> Result<bool, OidError> {
        let path = root.join(oid.to_string());
        let removed = match fs::remove_file(path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let metadata_changed = self.record_hashes()?.remove(&oid).is_some();
        Ok(removed || metadata_changed)
    }

    fn record_hashes(&self) -> Result<MutexGuard<'_, BTreeMap<u16, String>>, OidError> {
        self.inner
            .record_sha256
            .lock()
            .map_err(|_| OidError::Security("OID record-digest lock poisoned".into()))
    }

    fn head_generation(&self) -> Result<MutexGuard<'_, Option<u64>>, OidError> {
        self.inner
            .head_generation
            .lock()
            .map_err(|_| OidError::Security("OID trusted-head lock poisoned".into()))
    }
}

pub(crate) fn record_aad(oid: u16, target: &str, compiler_abi: &str) -> Vec<u8> {
    format!("RBE-OID-RECORD/1|oid={oid}|target={target}|compiler={compiler_abi}").into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn vault_error(message: String) -> OidError {
    OidError::Vault(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockVault {
        head: Mutex<Option<String>>,
    }

    impl OidVaultAuthority for MockVault {
        fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = Vec::with_capacity(4 + aad.len() + plaintext.len());
            out.extend_from_slice(&(aad.len() as u32).to_le_bytes());
            out.extend_from_slice(aad);
            out.extend_from_slice(plaintext);
            Ok(out)
        }

        fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
            if sealed.len() < 4 {
                return Err("truncated".into());
            }
            let len = u32::from_le_bytes(sealed[..4].try_into().unwrap()) as usize;
            if sealed.len() < 4 + len || &sealed[4..4 + len] != aad {
                return Err("AAD mismatch".into());
            }
            Ok(sealed[4 + len..].to_vec())
        }

        fn trusted_head(&self) -> Result<Option<String>, String> {
            Ok(self.head.lock().unwrap().clone())
        }

        fn set_trusted_head(&self, head: &str) -> Result<(), String> {
            *self.head.lock().unwrap() = Some(head.to_string());
            Ok(())
        }
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("rbe-oid-security-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn protected_cache_uses_one_index_and_detects_record_tampering() {
        let root = temp_root("record-tamper");
        let io = AtomicIo::new();
        let vault = Arc::new(MockVault::default());
        let security = OidSecurity::acquire(&root, vault.clone()).unwrap();
        let oid_root = root.join(".cache/compiler/oid");
        let raw_index = b"index-v1";
        security.write_index(&oid_root, &io, raw_index, 1).unwrap();
        let aad = record_aad(321, "linux-x86_64", "relc-v1");
        security
            .stage_record_write(&oid_root, &io, 321, &aad, b"machine-code")
            .unwrap();
        security.write_index(&oid_root, &io, raw_index, 1).unwrap();
        assert!(!oid_root.join("manifest").exists());
        drop(security);

        let security = OidSecurity::acquire(&root, vault).unwrap();
        assert_eq!(security.open_index(&oid_root, &io).unwrap(), raw_index);
        let path = oid_root.join("321");
        let mut bytes = io.read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0x55;
        io.write_atomic(&path, &bytes).unwrap();
        assert!(matches!(
            security.open_record(&oid_root, &io, 321, &aad),
            Err(OidError::Security(_))
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protected_index_replay_or_replacement_is_rejected_by_vault_head() {
        let root = temp_root("index-tamper");
        let io = AtomicIo::new();
        let vault = Arc::new(MockVault::default());
        let security = OidSecurity::acquire(&root, vault.clone()).unwrap();
        let oid_root = root.join(".cache/compiler/oid");
        security
            .write_index(&oid_root, &io, b"index-v1", 1)
            .unwrap();
        drop(security);

        let path = oid_root.join("index");
        let mut bytes = io.read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        io.write_atomic(&path, &bytes).unwrap();
        let security = OidSecurity::acquire(&root, vault).unwrap();
        assert!(matches!(
            security.open_index(&oid_root, &io),
            Err(OidError::Security(_))
        ));
        let _ = fs::remove_dir_all(&root);
    }
}
