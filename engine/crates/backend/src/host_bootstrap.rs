use std::process::Stdio;

use rand::RngCore;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy)]
pub struct HostBootstrapReady {
    secure_credentials: bool,
}

impl HostBootstrapReady {
    pub fn er_control_enabled(self) -> bool {
        self.secure_credentials
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
    #[cfg(target_os = "linux")]
    {
        evaluate_linux(args).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        Ok(HostBootstrapReady {
            secure_credentials: true,
        })
    }
}

#[cfg(target_os = "linux")]
async fn evaluate_linux(args: &[String]) -> anyhow::Result<HostBootstrapReady> {
    const PROBE: &str = include_str!("../scripts/bootstrap/linux/probe-secret-service.sh");
    const INSTALL: &str = include_str!("../scripts/bootstrap/linux/install-secret-service.sh");

    let verbose = verbose_debug(args);
    let first = run_embedded_shell("probe-secret-service.sh", PROBE, verbose).await?;
    apply_exports(&first.stdout);

    if first.status != 0 {
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

    verify_secret_service(verbose)?;
    Ok(HostBootstrapReady {
        secure_credentials: true,
    })
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
    let start = value.len().saturating_sub(MAX);
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
}
