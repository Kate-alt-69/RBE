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

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

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

/// Detect which Linux distro is running.
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

const DASHBOARD_LOG_LINES: usize = 7;

struct Dashboard {
    rendered_lines: usize,
}

impl Dashboard {
    fn new() -> Self {
        Self { rendered_lines: 0 }
    }

    fn clear(&mut self) {
        if self.rendered_lines == 0 {
            return;
        }

        let mut out = std::io::stderr();
        let _ = write!(out, "\x1b[{}A", self.rendered_lines);
        for _ in 0..self.rendered_lines {
            let _ = write!(out, "\x1b[2K\r\n");
        }
        let _ = write!(out, "\x1b[{}A\r", self.rendered_lines);
        let _ = out.flush();
        self.rendered_lines = 0;
    }

    fn render(
        &mut self,
        stage: &str,
        command: &str,
        started: Instant,
        logs: &[String],
        progress: Option<u8>,
    ) {
        let mut lines = Vec::with_capacity(DASHBOARD_LOG_LINES + 4);
        lines.push(format!("Vault setup — {stage}"));
        lines.push(format!("Command: {command}"));
        lines.push(format!("Elapsed: {}", format_elapsed(started.elapsed().as_secs_f64())));
        lines.push(String::from("Logs:"));

        let start = logs.len().saturating_sub(DASHBOARD_LOG_LINES);
        for line in &logs[start..] {
            lines.push(format!("  {line}"));
        }
        while lines.len() < 4 + DASHBOARD_LOG_LINES {
            lines.push(String::new());
        }

        if let Some(percent) = progress {
            lines.push(format_progress_bar(stage, percent));
        }

        let mut out = std::io::stderr();
        if self.rendered_lines > 0 {
            let _ = write!(out, "\x1b[{}A", self.rendered_lines);
        }
        for line in &lines {
            let _ = write!(out, "\x1b[2K\r{line}\n");
        }
        self.rendered_lines = lines.len();
        let _ = out.flush();
    }
}

fn format_elapsed(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let minutes = (seconds / 60.0).floor() as u64;
        let remaining = seconds - (minutes as f64 * 60.0);
        format!("{minutes}m {remaining:.1}s")
    }
}

fn format_progress_bar(stage: &str, percent: u8) -> String {
    let width = 32usize;
    let filled = ((percent as usize * width) / 100).min(width);
    format!(
        "  [{:<width$}] {:>3}%  {stage}",
        "=".repeat(filled),
        percent,
        width = width
    )
}

fn run_dashboard_command(
    dashboard: &mut Dashboard,
    stage: &str,
    command: &str,
    progress: Option<u8>,
) -> bool {
    let started = Instant::now();
    let mut logs = Vec::<String>::new();

    dashboard.render(stage, command, started, &logs, progress);

    let spawn = Command::new("sh")
        .arg("-c")
        .arg(format!("sudo {command} 2>&1"))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .spawn();

    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            logs.push(format!("failed to start command: {err}"));
            dashboard.render(stage, command, started, &logs, progress);
            dashboard.clear();
            eprintln!(
                "Vault setup: {stage} failed after {} — could not start command: {err}",
                format_elapsed(started.elapsed().as_secs_f64())
            );
            return false;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    logs.push(line.trim_end().to_string());
                    dashboard.render(stage, command, started, &logs, progress);
                }
                Ok(_) => {}
                Err(err) => {
                    logs.push(format!("output read error: {err}"));
                    dashboard.render(stage, command, started, &logs, progress);
                    break;
                }
            }
        }
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            logs.push(format!("wait error: {err}"));
            dashboard.render(stage, command, started, &logs, progress);
            dashboard.clear();
            eprintln!(
                "Vault setup: {stage} failed after {} — could not wait for command: {err}",
                format_elapsed(started.elapsed().as_secs_f64())
            );
            return false;
        }
    };

    let elapsed = format_elapsed(started.elapsed().as_secs_f64());
    let success = status.success();
    let exit_code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());

    dashboard.clear();

    if success {
        eprintln!("Vault setup: {stage} completed in {elapsed} (exit {exit_code})");
    } else {
        eprintln!(
            "Vault setup: {stage} failed in {elapsed} (exit {exit_code}) — see command output above for the failure reason"
        );
        for line in logs.iter().rev().take(3).rev() {
            eprintln!("  {line}");
        }
    }

    success
}

/// Try to install Secret Service daemon using available package managers.
fn try_install_secret_service() -> bool {
    let mut dashboard = Dashboard::new();

    let distro_started = Instant::now();
    let mut distro_logs = vec!["Reading /etc/os-release".to_string()];
    dashboard.render(
        "checking Linux distribution",
        "read /etc/os-release",
        distro_started,
        &distro_logs,
        None,
    );
    let distro = detect_linux_distro();
    distro_logs.push(format!("detected distro: {distro}"));
    dashboard.render(
        "checking Linux distribution",
        "read /etc/os-release",
        distro_started,
        &distro_logs,
        None,
    );
    dashboard.clear();

    // Detect which package manager is available.
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
            eprintln!("Vault setup: unsupported Linux distribution '{distro}' — using fallback");
            vec![]
        }
    };

    if pm_commands.is_empty() {
        return false;
    }

    for (pm_check, pm_install, pm_name) in pm_commands {
        // Only the package installation itself gets the bottom progress bar.
        if !run_dashboard_command(
            &mut dashboard,
            "checking installed Secret Service components",
            "command -v gnome-keyring dbus-daemon",
            None,
        ) {
            // Missing binaries are expected on the first run. Continue to the package manager.
            eprintln!("Vault setup: Secret Service components are not installed yet");
        }

        if run_dashboard_command(&mut dashboard, "refreshing package metadata", pm_check, None) {
            let install_command = format!("{pm_install} --no-install-recommends");
            if run_dashboard_command(
                &mut dashboard,
                &format!("installing Secret Service via {pm_name}"),
                &install_command,
                Some(0),
            ) {
                if probe_keyring("vault-probe") {
                    // The installation/probe stage is the only place where the bar appears.
                    eprintln!("Vault setup: Secret Service installation verified successfully");
                    return true;
                }

                eprintln!(
                    "Vault setup: packages installed, but the Secret Service probe still failed"
                );
                eprintln!("Vault setup: no usable Secret Service session is available to this process");
            } else {
                eprintln!("Vault setup: package installation failed; keeping the local fallback");
            }
        } else {
            eprintln!("Vault setup: package metadata refresh failed; keeping the local fallback");
        }
    }

    eprintln!("Vault setup: Secret Service installation failed, using fallback");
    false
}

fn probe_keyring_with_install(service_name: &str) -> bool {
    if probe_keyring(service_name) {
        return true;
    }

    // Attempt to install Secret Service if it's not available.
    if try_install_secret_service() {
        probe_keyring(service_name)
    } else {
        false
    }
}
