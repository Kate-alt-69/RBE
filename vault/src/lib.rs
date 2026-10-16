//! Migration-plan §8: a gatekeeper over the OS credential store, not a
//! from-scratch secret store. `vault.credential(name, caller)` — ACL
//! check, then fetch from whichever backend this process is actually
//! using, every access audit-logged.

mod acl;
mod file_store;

use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use logging::Logger;
use rand::RngCore;
use secrecy::SecretString;
use zeroize::Zeroizing;

use acl::Acl;
use file_store::FileStore;

const FALLBACK_MASTER_KEY_ENV: &str = "RBE_VAULT_FALLBACK_MASTER_KEY";
const INTERNAL_PREFIX: &str = "__rbe.internal.";
const INTERNAL_SEAL_MAGIC: &[u8; 8] = b"RBEVSL01";

enum Backend {
    Keyring { service_name: String },
    File { store: FileStore },
}

pub struct Vault {
    backend: Backend,
    acl: Acl,
    log: Logger,
}

impl Vault {
    pub fn new(
        io: atomic_io::AtomicIo,
        service_name: impl Into<String>,
        data_dir: &Path,
    ) -> anyhow::Result<Self> {
        let service_name = service_name.into();
        let log = Logger::new("VAULT");
        let acl = Acl::load(data_dir)?;

        let backend = if cfg!(any(target_os = "windows", target_os = "macos"))
            || probe_keyring(&service_name)
        {
            Backend::Keyring { service_name }
        } else {
            let master_key = Zeroizing::new(std::env::var(FALLBACK_MASTER_KEY_ENV).map_err(|_| {
                anyhow::anyhow!(
                    "OS credential service is unavailable and {FALLBACK_MASTER_KEY_ENV} is not set; refusing to create a plaintext vault master key beside the encrypted fallback store"
                )
            })?);
            log.warn(
                "Secret Service unavailable; using encrypted file store with an externally supplied master key",
            );
            Backend::File {
                store: FileStore::open(io, data_dir, &master_key)?,
            }
        };

        Ok(Self { backend, acl, log })
    }

    pub fn credential(&self, name: &str, caller: &str) -> anyhow::Result<SecretString> {
        reject_reserved_credential_name(name)?;
        if !self.acl.is_allowed(name, caller) {
            self.log.warn(format!(
                "ACL DENY: {caller} attempted to read credential {name:?}"
            ));
            anyhow::bail!("access denied: {caller} is not permitted to read {name:?}");
        }

        let value = match &self.backend {
            Backend::Keyring { service_name } => {
                let entry = keyring::Entry::new(service_name, name)?;
                entry
                    .get_password()
                    .map_err(|e| anyhow::anyhow!("credential {name:?} not found: {e}"))?
            }
            Backend::File { store } => store.get(name)?,
        };

        self.log
            .info(format!("ACL ALLOW: {caller} read credential {name:?}"));

        Ok(SecretString::new(value))
    }

    pub fn set_credential(&self, name: &str, value: &str, caller: &str) -> anyhow::Result<()> {
        reject_reserved_credential_name(name)?;
        if !self.acl.is_allowed(name, caller) {
            self.log.warn(format!(
                "ACL DENY: {caller} attempted to write credential {name:?}"
            ));
            anyhow::bail!("access denied: {caller} is not permitted to write {name:?}");
        }

        match &self.backend {
            Backend::Keyring { service_name } => {
                let entry = keyring::Entry::new(service_name, name)?;
                entry.set_password(value)?;
            }
            Backend::File { store } => store.set(name, value)?,
        }

        self.log
            .info(format!("ACL ALLOW: {caller} wrote credential {name:?}"));

        Ok(())
    }

    pub fn seal_internal(
        &self,
        namespace: &str,
        aad: &[u8],
        plaintext: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        validate_internal_namespace(namespace)?;
        let key = self.internal_key(namespace, true)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*key));
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|error| anyhow::anyhow!("Vault internal seal failed: {error}"))?;
        let mut out = Vec::with_capacity(INTERNAL_SEAL_MAGIC.len() + 12 + ciphertext.len());
        out.extend_from_slice(INTERNAL_SEAL_MAGIC);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn open_internal(
        &self,
        namespace: &str,
        aad: &[u8],
        sealed: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        validate_internal_namespace(namespace)?;
        if sealed.len() < INTERNAL_SEAL_MAGIC.len() + 12 + 16
            || &sealed[..INTERNAL_SEAL_MAGIC.len()] != INTERNAL_SEAL_MAGIC
        {
            anyhow::bail!("Vault internal sealed object has an invalid envelope");
        }
        let key = self.internal_key(namespace, false)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*key));
        let nonce_start = INTERNAL_SEAL_MAGIC.len();
        let nonce_end = nonce_start + 12;
        cipher
            .decrypt(
                Nonce::from_slice(&sealed[nonce_start..nonce_end]),
                Payload {
                    msg: &sealed[nonce_end..],
                    aad,
                },
            )
            .map_err(|error| anyhow::anyhow!(
                "Vault internal object authentication failed (wrong key, AAD, or tampering): {error}"
            ))
    }

    pub fn internal_head(&self, namespace: &str) -> anyhow::Result<Option<String>> {
        validate_internal_namespace(namespace)?;
        self.backend_get_optional(&internal_name(namespace, "head"))
    }

    pub fn set_internal_head(&self, namespace: &str, value: &str) -> anyhow::Result<()> {
        validate_internal_namespace(namespace)?;
        self.backend_set(&internal_name(namespace, "head"), value)
    }

    fn internal_key(&self, namespace: &str, create: bool) -> anyhow::Result<Zeroizing<[u8; 32]>> {
        let name = internal_name(namespace, "seal-key");
        if let Some(value) = self.backend_get_optional(&name)? {
            return parse_internal_key(&value);
        }
        if !create {
            anyhow::bail!("Vault internal sealing key for {namespace:?} is unavailable");
        }
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        self.backend_set(&name, &hex::encode(key))?;
        Ok(Zeroizing::new(key))
    }

    fn backend_get_optional(&self, name: &str) -> anyhow::Result<Option<String>> {
        match &self.backend {
            Backend::Keyring { service_name } => {
                let entry = keyring::Entry::new(service_name, name)?;
                match entry.get_password() {
                    Ok(value) => Ok(Some(value)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
            Backend::File { store } => store.get_optional(name),
        }
    }

    fn backend_set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Keyring { service_name } => {
                keyring::Entry::new(service_name, name)?.set_password(value)?;
                Ok(())
            }
            Backend::File { store } => store.set(name, value),
        }
    }
}

fn reject_reserved_credential_name(name: &str) -> anyhow::Result<()> {
    if name.starts_with(INTERNAL_PREFIX) {
        anyhow::bail!(
            "reserved Vault internal credential namespace is not available to normal callers"
        );
    }
    Ok(())
}

fn validate_internal_namespace(namespace: &str) -> anyhow::Result<()> {
    if namespace.is_empty()
        || namespace.len() > 160
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("invalid Vault internal namespace {namespace:?}");
    }
    Ok(())
}

fn internal_name(namespace: &str, suffix: &str) -> String {
    format!("{INTERNAL_PREFIX}{namespace}.{suffix}")
}

fn parse_internal_key(value: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let decoded = Zeroizing::new(hex::decode(value.trim())?);
    let key: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Vault internal sealing key has the wrong length"))?;
    Ok(Zeroizing::new(key))
}

fn probe_keyring(service_name: &str) -> bool {
    const PROBE_KEY: &str = "__vault_startup_probe__";

    if ensure_linux_secret_service().is_err() {
        return false;
    }

    let Ok(entry) = keyring::Entry::new(service_name, PROBE_KEY) else {
        return false;
    };
    if entry.set_password("probe").is_err() {
        return false;
    }
    let ok = entry.get_password().is_ok();
    let _ = entry.delete_password();
    ok
}

#[cfg(target_os = "linux")]
fn ensure_linux_secret_service() -> Result<(), String> {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return start_gnome_keyring_secrets();
    }

    let output = Command::new("dbus-daemon")
        .args(["--session", "--fork", "--print-address"])
        .output()
        .map_err(|err| format!("failed to start session D-Bus: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "session D-Bus exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let output_text = String::from_utf8_lossy(&output.stdout).into_owned();
    let address = output_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "session D-Bus did not print a bus address".to_string())?;

    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address);
    start_gnome_keyring_secrets()
}

#[cfg(target_os = "linux")]
fn start_gnome_keyring_secrets() -> Result<(), String> {
    let output = Command::new("gnome-keyring-daemon")
        .args(["--start", "--components=secrets"])
        .output()
        .map_err(|err| format!("failed to start gnome-keyring-daemon: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gnome-keyring-daemon exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        match name.trim() {
            "GNOME_KEYRING_CONTROL" | "GNOME_KEYRING_PID" | "SSH_AUTH_SOCK" | "GPG_AGENT_INFO" => {
                std::env::set_var(name.trim(), value.trim());
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_linux_secret_service() -> Result<(), String> {
    Ok(())
}
