//! Migration-plan §8: a gatekeeper over the OS credential store, not a
//! from-scratch secret store. `vault.credential(name, caller)` — ACL
//! check, then fetch from whichever backend this process is actually
//! using, every access audit-logged.
//!
//! **Backend selection happens once, at [`Vault::new`], not per-call.**
//! Windows/macOS are assumed to always have a working OS credential
//! store (DPAPI/Keychain are part of the OS, not an optional daemon).
//! Linux is probed once at startup — no Secret Service daemon running
//! (common on a bare VPS) falls back to the local encrypted file store
//! in [`file_store`] for the rest of this process's lifetime. Splitting
//! reads/writes across backends mid-run would be a real correctness
//! hazard (write goes to the store that happened to be probed
//! successfully that call, read checks a different one) — probing once
//! and committing to a backend avoids that entirely.
//!
//! **Memory-dump mitigation status, honestly:** credentials are
//! returned wrapped in [`secrecy::SecretString`], which zeroizes on
//! drop and refuses `Debug`/`Display` — that's the layer-1 mitigation
//! from §8.2 (never let the key/value sit around in an un-zeroized,
//! accidentally-loggable form). `mlock`/hardware-backed keys (§8.2's
//! layers 2–3) are NOT implemented here — flagged, not silently
//! skipped.

mod acl;
mod file_store;

use std::path::Path;

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
    /// `io` should be the same shared `AtomicIo` instance the rest of
    /// the process uses (constructed once in `main.rs`) — see
    /// `file_store::FileStore::open`'s doc comment for why sharing it
    /// matters. `service_name` namespaces this app's entries in the OS
    /// credential store (so they don't collide with some other
    /// application's entries of the same credential name). `data_dir`
    /// is where the ACL file and, if needed, the fallback encrypted
    /// store live — pass the same `./data/admin`-style directory used
    /// elsewhere (see `logging::spawn_error_reporter` for the sibling
    /// convention).
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
                "OS credential store unavailable (no Secret Service daemon?) — \
                 falling back to local encrypted file store for this run",
            );
            Backend::File {
                store: FileStore::open(io, data_dir)?,
            }
        };

        Ok(Self { backend, acl, log })
    }

    /// Fetches a credential. `caller` identifies the requesting
    /// subsystem (`"email_service"`, `"uac"`, ...) and is checked
    /// against the ACL — see `acl`'s doc comment for the fail-closed
    /// behavior. Every access, allowed or denied, is audit-logged.
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

    /// Writes/overwrites a credential. Same ACL gate as reads — a
    /// caller not permitted to *read* a credential can't write it
    /// either in v1 (no separate read/write ACL granularity yet).
    /// Intended for bootstrap/admin tooling, not general request-path
    /// code.
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

/// One-time startup probe: does the OS credential store actually work
/// here, or is there no backing daemon (headless Linux without Secret
/// Service)? Round-trips a sentinel entry through the real backend
/// rather than just checking "did `Entry::new` succeed" — `Entry::new`
/// only constructs a handle, it doesn't touch the OS store at all, so
/// it can't tell us whether the store is actually reachable.
fn probe_keyring(service_name: &str) -> bool {
    const PROBE_KEY: &str = "__vault_startup_probe__";

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
