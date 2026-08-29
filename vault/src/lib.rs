//! Migration-plan §8: a gatekeeper over the OS credential store, not a
//! from-scratch secret store. `vault.credential(name, caller)` — ACL
//! check, then fetch from whichever backend this process is actually
//! using, every access audit-logged.

mod acl;
mod file_store;

use std::path::Path;
use std::process::Command;

use logging::Logger;
use secrecy::SecretString;

use acl::Acl;
use file_store::FileStore;

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

        let backend = if cfg!(any(target_os = "windows", target_os = "macos")) {
            Backend::Keyring { service_name }
        } else if probe_keyring(&service_name) {
            Backend::Keyring { service_name }
        } else {
            log.warn(
                "Secret Service unavailable; falling back to local encrypted file store for this run",
            );
            Backend::File {
                store: FileStore::open(io, data_dir)?,
            }
        };

        Ok(Self { backend, acl, log })
    }

    pub fn credential(&self, name: &str, caller: &str) -> anyhow::Result<SecretString> {
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
            "GNOME_KEYRING_CONTROL"
            | "GNOME_KEYRING_PID"
            | "SSH_AUTH_SOCK"
            | "GPG_AGENT_INFO" => {
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
