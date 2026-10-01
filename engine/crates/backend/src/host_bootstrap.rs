use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use cloud_node::{discover_settings_path, CloudNodeSettings};
use rand::RngCore;
#[cfg(target_os = "linux")]
use tokio::io::AsyncWriteExt;

const FALLBACK_MASTER_KEY_ENV: &str = "RBE_VAULT_FALLBACK_MASTER_KEY";
const CLOUD_NODE_SETTINGS_ENV: &str = "RBE_CN_SETTINGS";
const CLOUD_NODE_STABLE_WINDOW: Duration = Duration::from_secs(60);
const CLOUD_NODE_RESTART_BASE: Duration = Duration::from_millis(500);
const CLOUD_NODE_RESTART_MAX: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HostBootstrapReady {
    secure_credentials: bool,
}

impl HostBootstrapReady {
    pub fn er_control_enabled(&self) -> bool {
        self.secure_credentials
    }

    pub fn issue_er_control_key(self) -> Option<ErControlKey> {
        self.er_control_enabled().then(ErControlKey::random)
    }
}

struct ErControlKeyMaterial([u8; 32]);

impl Drop for ErControlKeyMaterial {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone)]
pub struct ErControlKey(Arc<ErControlKeyMaterial>);

impl ErControlKey {
    fn random() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(Arc::new(ErControlKeyMaterial(bytes)))
    }

    pub(crate) fn from_inherited_hex(value: &str) -> anyhow::Result<Self> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("ER CONTROL key must be a 32-byte hexadecimal value");
        }
        let decoded = hex::decode(value)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("ER CONTROL key decoded to the wrong length"))?;
        Ok(Self(Arc::new(ErControlKeyMaterial(bytes))))
    }

    pub(crate) fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0.as_ref().0
    }
}

pub fn verbose_debug(args: &[String]) -> bool {
    cfg!(debug_assertions)
        && args.iter().any(|arg| arg == "-debug" || arg == "--debug")
        && !std::env::var("RBE_ENV")
            .map(|value| value.eq_ignore_ascii_case("production"))
            .unwrap_or(false)
}

pub async fn evaluate(args: &[String]) -> anyhow::Result<HostBootstrapReady> {
    let ready = {
        #[cfg(target_os = "linux")]
        {
            evaluate_linux(args).await?
        }
        #[cfg(not(target_os = "linux"))]
        {
            HostBootstrapReady {
                secure_credentials: true,
            }
        }
    };

    if cloud_node_backend_autostart_allowed(args) {
        start_provider_cloud_node(args).await?;
    }
    Ok(ready)
}

fn cloud_node_backend_autostart_allowed(args: &[String]) -> bool {
    !args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--vault" | "check" | "--help" | "-h" | "help"))
}

async fn start_provider_cloud_node(args: &[String]) -> anyhow::Result<()> {
    let Some(config_path) = resolve_cloud_node_settings_path()? else {
        return Ok(());
    };
    let settings = CloudNodeSettings::load(&config_path).map_err(|error| {
        anyhow::anyhow!(
            "Cloud Node settings {} failed backend boot validation: {error:#}",
            config_path.display()
        )
    })?;
    let Some(provider) = settings.provider.as_ref() else {
        return Ok(());
    };
    if !provider.sync_on_connect && !provider.auto_reconnect {
        return Ok(());
    }

    let executable = sibling_cloud_node_executable()?;
    if !executable.is_file() {
        anyhow::bail!(
            "Cloud Node provider mode is configured in {}, but the packaged Cloud Node binary is missing at {}",
            config_path.display(),
            executable.display()
        );
    }
    let project_root = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("resolve backend project root for Cloud Node: {error}"))?
        .canonicalize()
        .map_err(|error| {
            anyhow::anyhow!("canonicalize backend project root for Cloud Node: {error}")
        })?;
    let verbose = verbose_debug(args);

    if provider.sync_on_connect {
        if verbose {
            eprintln!(
                "[HostBootstrap/CloudNode] boot synchronization starting config={} project={}",
                config_path.display(),
                project_root.display()
            );
        }
        run_cloud_node_boot_sync(&executable, &config_path, &project_root).await?;
        if verbose {
            eprintln!("[HostBootstrap/CloudNode] boot synchronization completed");
        }
    }

    if provider.auto_reconnect {
        spawn_cloud_node_supervisor(executable, config_path, project_root, verbose);
    }
    Ok(())
}

fn resolve_cloud_node_settings_path() -> anyhow::Result<Option<PathBuf>> {
    if let Some(explicit) = std::env::var_os(CLOUD_NODE_SETTINGS_ENV) {
        let path = PathBuf::from(explicit);
        if !path.is_file() {
            anyhow::bail!(
                "{CLOUD_NODE_SETTINGS_ENV} points to a missing Cloud Node settings file: {}",
                path.display()
            );
        }
        return Ok(Some(path));
    }

    let executable = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("resolve backend executable for Cloud Node: {error}"))?;
    discover_settings_path(&executable)
}

fn sibling_cloud_node_executable() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("resolve backend executable for Cloud Node: {error}"))?;
    let parent = executable.parent().ok_or_else(|| {
        anyhow::anyhow!("backend executable has no parent directory for Cloud Node binary")
    })?;
    let name = if cfg!(windows) {
        "cloud_node.exe"
    } else {
        "cloud_node"
    };
    Ok(parent.join(name))
}

async fn run_cloud_node_boot_sync(
    executable: &Path,
    config_path: &Path,
    project_root: &Path,
) -> anyhow::Result<()> {
    let status = tokio::process::Command::new(executable)
        .arg(format!("--config={}", config_path.display()))
        .arg("sync")
        .arg("--bootstrap")
        .env("RBE_PROJECT_ROOT", project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to launch Cloud Node boot synchronization {}: {error}",
                executable.display()
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "Cloud Node boot synchronization failed with status {status}; backend startup is blocked so recovered/provider state cannot be bypassed"
        );
    }
    Ok(())
}

fn spawn_cloud_node_supervisor(
    executable: PathBuf,
    config_path: PathBuf,
    project_root: PathBuf,
    verbose: bool,
) {
    std::mem::drop(tokio::spawn(async move {
        let mut consecutive_failures = 0u32;
        loop {
            let mut command = tokio::process::Command::new(&executable);
            command
                .arg(format!("--config={}", config_path.display()))
                .arg("run")
                .env("RBE_PROJECT_ROOT", &project_root)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);

            let started = tokio::time::Instant::now();
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = cloud_node_restart_delay(consecutive_failures);
                    eprintln!(
                        "[HostBootstrap/CloudNode] daemon spawn failed: {error}; retrying in {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            if verbose {
                eprintln!(
                    "[HostBootstrap/CloudNode] provider daemon started pid={:?} config={}",
                    child.id(),
                    config_path.display()
                );
            }

            let observed = child.wait().await;
            let uptime = started.elapsed();
            if uptime >= CLOUD_NODE_STABLE_WINDOW {
                consecutive_failures = 0;
            }
            consecutive_failures = consecutive_failures.saturating_add(1);
            let delay = cloud_node_restart_delay(consecutive_failures);
            match observed {
                Ok(status) => eprintln!(
                    "[HostBootstrap/CloudNode] daemon exited unexpectedly status={status} uptime_ms={} retry_ms={}",
                    uptime.as_millis(),
                    delay.as_millis()
                ),
                Err(error) => eprintln!(
                    "[HostBootstrap/CloudNode] daemon wait failed error={error} uptime_ms={} retry_ms={}",
                    uptime.as_millis(),
                    delay.as_millis()
                ),
            }
            tokio::time::sleep(delay).await;
        }
    }));
}

fn cloud_node_restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = CLOUD_NODE_RESTART_BASE
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(millis.saturating_mul(factor)).min(CLOUD_NODE_RESTART_MAX)
}

#[cfg(target_os = "linux")]
async fn evaluate_linux(args: &[String]) -> anyhow::Result<HostBootstrapReady> {
    const PROBE: &str = include_str!("../scripts/bootstrap/linux/probe-secret-service.sh");
    const INSTALL: &str = include_str!("../scripts/bootstrap/linux/install-secret-service.sh");

    let verbose = verbose_debug(args);
    let fallback_ready = configured_fallback_master_key()?;
    let first = run_embedded_shell("probe-secret-service.sh", PROBE, verbose).await?;
    apply_exports(&first.stdout);

    if first.status != 0 {
        if fallback_ready {
            if verbose {
                eprintln!(
                    "[HostBootstrap] Secret Service unavailable; using configured encrypted-file Vault fallback"
                );
            }
            return Ok(HostBootstrapReady {
                secure_credentials: true,
            });
        }
        if first.status != 10 {
            anyhow::bail!("Linux credential host evaluation failed during Secret Service probe");
        }
        let install = run_embedded_shell("install-secret-service.sh", INSTALL, verbose).await?;
        if install.status != 0 {
            anyhow::bail!(
                "Linux credential host evaluation could not provision a Secret Service provider"
            );
        }
        let second = run_embedded_shell("probe-secret-service.sh", PROBE, verbose).await?;
        apply_exports(&second.stdout);
        if second.status != 0 {
            anyhow::bail!("Linux Secret Service remained unavailable after host provisioning");
        }
    }

    if let Err(error) = verify_secret_service(verbose) {
        if fallback_ready {
            if verbose {
                eprintln!(
                    "[HostBootstrap] Secret Service verification failed ({error}); using configured encrypted-file Vault fallback"
                );
            }
            return Ok(HostBootstrapReady {
                secure_credentials: true,
            });
        }
        return Err(error);
    }

    Ok(HostBootstrapReady {
        secure_credentials: true,
    })
}

#[cfg(target_os = "linux")]
fn configured_fallback_master_key() -> anyhow::Result<bool> {
    match std::env::var(FALLBACK_MASTER_KEY_ENV) {
        Ok(value) => {
            validate_fallback_master_key(&value)?;
            Ok(true)
        }
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{FALLBACK_MASTER_KEY_ENV} must be a UTF-8 32-byte hexadecimal value")
        }
    }
}

fn validate_fallback_master_key(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!(
            "{FALLBACK_MASTER_KEY_ENV} must contain exactly 64 hexadecimal characters (32 bytes)"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct ScriptOutput {
    status: i32,
    stdout: String,
}

#[cfg(target_os = "linux")]
async fn run_embedded_shell(
    name: &str,
    script: &str,
    verbose: bool,
) -> anyhow::Result<ScriptOutput> {
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("could not start embedded Linux host script {name}: {error}")
        })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("embedded Linux host script {name} did not expose stdin"))?;
    stdin.write_all(script.as_bytes()).await?;
    stdin.shutdown().await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if verbose {
        eprintln!("[HostBootstrap/{name}] exit={}", output.status);
        if !stdout.trim().is_empty() {
            eprintln!("[HostBootstrap/{name}/stdout]\n{}", bounded_debug(&stdout));
        }
        if !stderr.trim().is_empty() {
            eprintln!("[HostBootstrap/{name}/stderr]\n{}", bounded_debug(&stderr));
        }
    }
    Ok(ScriptOutput {
        status: output.status.code().unwrap_or(255),
        stdout,
    })
}

#[cfg(target_os = "linux")]
fn bounded_debug(value: &str) -> String {
    const MAX: usize = 64 * 1024;
    if value.len() <= MAX {
        return value.to_string();
    }
    let mut start = value.len().saturating_sub(MAX);
    while start < value.len() && !value.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    format!("[...truncated...]\n{}", &value[start..])
}

#[cfg(target_os = "linux")]
fn apply_exports(stdout: &str) {
    const ALLOWED: &[&str] = &[
        "DBUS_SESSION_BUS_ADDRESS",
        "GNOME_KEYRING_CONTROL",
        "GNOME_KEYRING_PID",
    ];
    for line in stdout.lines() {
        let Some(value) = line.strip_prefix("RBE_EXPORT_") else {
            continue;
        };
        let Some((name, value)) = value.split_once('=') else {
            continue;
        };
        if ALLOWED.contains(&name) && !value.contains('\0') && value.len() <= 4096 {
            std::env::set_var(name, value);
        }
    }
}

#[cfg(target_os = "linux")]
fn verify_secret_service(verbose: bool) -> anyhow::Result<()> {
    let mut random = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let account = format!("__rbe_host_probe_{}", hex::encode(random));
    let mut value_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut value_bytes);
    let value = hex::encode(value_bytes);
    let entry = keyring::Entry::new("rbe.host-bootstrap", &account)
        .map_err(|error| anyhow::anyhow!("Secret Service verification entry failed: {error}"))?;
    entry
        .set_password(&value)
        .map_err(|error| anyhow::anyhow!("Secret Service verification write failed: {error}"))?;
    let read = entry
        .get_password()
        .map_err(|error| anyhow::anyhow!("Secret Service verification read failed: {error}"))?;
    let _ = entry.delete_password();
    if read != value {
        anyhow::bail!("Secret Service verification returned a different disposable value");
    }
    if verbose {
        eprintln!("[HostBootstrap] Secret Service write/read/delete verification passed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_never_enables_verbose_bootstrap() {
        std::env::set_var("RBE_ENV", "production");
        assert!(!verbose_debug(&["-debug".into()]));
        std::env::remove_var("RBE_ENV");
    }

    #[test]
    fn fallback_master_key_requires_exactly_32_hex_bytes() {
        assert!(validate_fallback_master_key(&"a5".repeat(32)).is_ok());
        assert!(validate_fallback_master_key(&"A5".repeat(32)).is_ok());
        assert!(validate_fallback_master_key(&"a5".repeat(31)).is_err());
        assert!(validate_fallback_master_key(&"a5".repeat(33)).is_err());
        assert!(validate_fallback_master_key(&format!("{}zz", "a5".repeat(31))).is_err());
    }

    #[test]
    fn cloud_node_autostart_skips_auxiliary_modes() {
        assert!(!cloud_node_backend_autostart_allowed(&["--vault".into()]));
        assert!(!cloud_node_backend_autostart_allowed(&["check".into()]));
        assert!(!cloud_node_backend_autostart_allowed(&["--help".into()]));
        assert!(cloud_node_backend_autostart_allowed(&["-debug".into()]));
    }

    #[test]
    fn cloud_node_restart_backoff_is_bounded() {
        assert_eq!(cloud_node_restart_delay(1), Duration::from_millis(500));
        assert_eq!(cloud_node_restart_delay(2), Duration::from_secs(1));
        assert_eq!(cloud_node_restart_delay(32), CLOUD_NODE_RESTART_MAX);
    }
}
