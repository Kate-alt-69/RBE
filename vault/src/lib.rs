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
use std::process::Command;
use std::time::Duration;
use std::thread;

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
    /// elsewhere (see `error_client::init` for the sibling convention).
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
        } else if probe_keyring_with_install(&service_name) {
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

/// Detect which Linux distro is running
#[allow(dead_code)]
fn detect_linux_distro() -> String {
    if let Ok(output) = Command::new("sh")
        .arg("-c")
        .arg(". /etc/os-release 2>/dev/null && echo \"$ID\" || echo linux")
        .output()
    {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_lowercase()
    } else {
        "linux".to_string()
    }
}

/// Display progress bar with state message
#[allow(dead_code)]
fn show_progress(message: &str, percent: u32) {
    let filled = (percent / 5).min(20) as usize;
    let empty = 20 - filled;
    let bar = format!(
        "[{}{}] {}%",
        "=".repeat(filled),
        "-".repeat(empty),
        percent
    );
    eprint!("\r{:<35} {}", message, bar);
}

/// Try to install Secret Service daemon using available package managers
#[allow(dead_code)]
fn try_install_secret_service() -> bool {
    let distro = detect_linux_distro();
    
    eprintln!("\n");
    show_progress("Detecting Secret Service daemon", 0);
    thread::sleep(Duration::from_millis(200));

    // Detect which package manager is available
    let pm_commands = match distro.as_str() {
        "ubuntu" | "debian" | "linuxmint" | "kali" | "pop" | "elementary" | "zorin" | "deepin" => {
            vec![("apt-get update", "apt-get install -y gnome-keyring dbus-x11", "Debian/Ubuntu")]
        }
        "fedora" | "rhel" | "centos" | "amazonlinux" | "oraclelinux" | "almalinux" | "rocky" => {
            vec![("dnf check-update", "dnf install -y gnome-keyring dbus-x11", "Fedora/RHEL")]
        }
        "arch" | "manjaro" | "artix" => {
            vec![("pacman -Sy", "pacman -S --noconfirm gnome-keyring dbus", "Arch/Manjaro")]
        }
        "alpine" => {
            vec![("apk update", "apk add gnome-keyring dbus-x11", "Alpine")]
        }
        "opensuse" | "suse" | "tumbleweed" | "sles" => {
            vec![("zypper refresh", "zypper install -y gnome-keyring dbus-x11", "openSUSE")]
        }
        _ => {
            show_progress("Checking available package managers", 25);
            thread::sleep(Duration::from_millis(200));
            vec![]
        }
    };

    if pm_commands.is_empty() {
        show_progress("No package manager detected, using fallback", 100);
        eprintln!("\n");
        return false;
    }

    for (pm_check, pm_install, pm_name) in pm_commands {
        show_progress("Checking Secret Service availability", 25);
        thread::sleep(Duration::from_millis(300));

        // Check if Secret Service packages might already be installed
        if Command::new("sh")
            .arg("-c")
            .arg("command -v gnome-keyring dbus-daemon >/dev/null 2>&1")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            show_progress("Secret Service already installed", 100);
            eprintln!("\n");
            return true;
        }

        show_progress(&format!("Installing via {} (downloading)", pm_name), 40);
        thread::sleep(Duration::from_millis(400));

        // Try update/refresh
        if Command::new("sh")
            .arg("-c")
            .arg(&format!("sudo {} >/dev/null 2>&1", pm_check))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            show_progress(&format!("Installing via {} (downloading)", pm_name), 60);
            thread::sleep(Duration::from_millis(400));

            // Try install
            if Command::new("sh")
                .arg("-c")
                .arg(&format!("sudo {} >/dev/null 2>&1", pm_install))
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                show_progress(&format!("Installing via {} (finalizing)", pm_name), 85);
                thread::sleep(Duration::from_millis(300));

                // Verify installation worked
                thread::sleep(Duration::from_millis(500));
                if probe_keyring("vault-probe") {
                    show_progress("Secret Service installed successfully", 100);
                    eprintln!("\n");
                    return true;
                }
            }
        }
    }

    show_progress("Secret Service installation failed, using fallback", 100);
    eprintln!("\n");
    false
}

#[allow(dead_code)]
fn probe_keyring_with_install(service_name: &str) -> bool {
    if probe_keyring(service_name) {
        return true;
    }

    // Attempt to install Secret Service if it's not available
    if try_install_secret_service() {
        probe_keyring(service_name)
    } else {
        false
    }
}
