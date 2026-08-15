//! Migration-plan §8: a gatekeeper over the OS credential store, not a
//! from-scratch secret store. `vault.credential(name, caller)` — ACL
//! check, then fetch from whichever backend this process is actually
//! using, every access audit-logged.

mod acl;
mod file_store;

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use error_client::{IssueInput, IssueLevel};
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
        } else if probe_keyring_with_install(&service_name, &log) {
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

const DASHBOARD_LINES: usize = 7;

struct Dashboard {
    active: bool,
    rendered: usize,
}

impl Dashboard {
    fn new() -> Self {
        Self {
            active: std::io::stderr().is_terminal(),
            rendered: 0,
        }
    }

    fn render(&mut self, stage: &str, command: &str, started: Instant, logs: &[String], install: bool) {
        if !self.active {
            return;
        }

        let mut lines = Vec::with_capacity(DASHBOARD_LINES + 4);
        lines.push(format!("Vault setup — {stage}"));
        lines.push(format!("Command: {command}"));
        lines.push(format!("Elapsed: {}", elapsed(started)));
        lines.push(String::from("Command output:"));

        let first = logs.len().saturating_sub(DASHBOARD_LINES);
        for line in &logs[first..] {
            lines.push(format!("  {line}"));
        }
        while lines.len() < 4 + DASHBOARD_LINES {
            lines.push(String::new());
        }

        if install {
            lines.push("  [================--------] INSTALLING".to_string());
        }

        let mut out = std::io::stderr();
        if self.rendered > 0 {
            let _ = write!(out, "\x1b[{}A", self.rendered);
        }
        for line in &lines {
            let _ = write!(out, "\x1b[2K\r{line}\n");
        }
        let _ = out.flush();
        self.rendered = lines.len();
    }

    fn clear(&mut self) {
        if !self.active || self.rendered == 0 {
            return;
        }

        let mut out = std::io::stderr();
        let _ = write!(out, "\x1b[{}A", self.rendered);
        for _ in 0..self.rendered {
            let _ = write!(out, "\x1b[2K\r\n");
        }
        let _ = write!(out, "\x1b[{}A\r", self.rendered);
        let _ = out.flush();
        self.rendered = 0;
    }
}

fn elapsed(started: Instant) -> String {
    let seconds = started.elapsed().as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let minutes = (seconds / 60.0).floor() as u64;
        let remaining = seconds - minutes as f64 * 60.0;
        format!("{minutes}m {remaining:.1}s")
    }
}

fn run_command(
    dashboard: &mut Dashboard,
    stage: &str,
    command: &str,
    use_sudo: bool,
    show_install_bar: bool,
) -> (bool, String) {
    let started = Instant::now();
    let mut logs = Vec::new();
    dashboard.render(stage, command, started, &logs, show_install_bar);

    let shell_command = if use_sudo {
        format!("sudo {command} 2>&1")
    } else {
        format!("{command} 2>&1")
    };

    let spawn = Command::new("sh")
        .arg("-c")
        .arg(shell_command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .spawn();

    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            logs.push(format!("failed to start command: {err}"));
            dashboard.render(stage, command, started, &logs, show_install_bar);
            dashboard.clear();
            return (false, format!("failed to start command: {err}"));
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    logs.push(line.trim_end().to_string());
                    dashboard.render(stage, command, started, &logs, show_install_bar);
                }
                Ok(_) => {}
                Err(err) => {
                    logs.push(format!("output read error: {err}"));
                    dashboard.render(stage, command, started, &logs, show_install_bar);
                }
            }
        }
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            logs.push(format!("wait error: {err}"));
            dashboard.render(stage, command, started, &logs, show_install_bar);
            dashboard.clear();
            return (false, format!("wait error: {err}"));
        }
    };

    let exit = status
        .code()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let detail = format!(
        "stage={stage}; command={command}; duration={}; exit={exit}; output:\n{}",
        elapsed(started),
        logs.join("\n")
    );
    let success = status.success();

    dashboard.clear();
    (success, detail)
}

fn report_install_failure(log: &Logger, details: &str) {
    error_client::report_issue(IssueInput {
        source: "vault.secret-service-installer",
        level: Some(IssueLevel::Error),
        category: None,
        message: "failed to install Secret Service",
        stack: Some(details),
    });
    log.warn("failed to install Secret Service; check error reporter log for more details");
}

fn try_install_secret_service(log: &Logger) -> bool {
    let mut dashboard = Dashboard::new();
    let mut diagnostics = Vec::new();

    let distro = detect_linux_distro();
    diagnostics.push(format!("detected distro: {distro}"));

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
            diagnostics.push("unsupported Linux distribution".to_string());
            report_install_failure(log, &diagnostics.join("\n"));
            return false;
        }
    };

    for (pm_check, pm_install, pm_name) in pm_commands {
        let (components_present, details) = run_command(
            &mut dashboard,
            "checking installed Secret Service components",
            "command -v gnome-keyring dbus-daemon",
            false,
            false,
        );
        diagnostics.push(details.clone());

        if components_present {
            if probe_keyring("vault-probe") {
                return true;
            }
            diagnostics.push("components are installed but Secret Service probe failed".to_string());
        }

        let (updated, details) = run_command(
            &mut dashboard,
            "refreshing package metadata",
            pm_check,
            true,
            false,
        );
        diagnostics.push(details);
        if !updated {
            continue;
        }

        let (installed, details) = run_command(
            &mut dashboard,
            &format!("installing Secret Service via {pm_name}"),
            pm_install,
            true,
            true,
        );
        diagnostics.push(details);
        if !installed {
            continue;
        }

        if probe_keyring("vault-probe") {
            return true;
        }

        diagnostics.push("package installation completed, but the Secret Service probe still failed".to_string());
    }

    report_install_failure(log, &diagnostics.join("\n\n"));
    false
}

fn probe_keyring_with_install(service_name: &str, log: &Logger) -> bool {
    if probe_keyring(service_name) {
        return true;
    }

    if try_install_secret_service(log) {
        probe_keyring(service_name)
    } else {
        false
    }
}