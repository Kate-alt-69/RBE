use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use service_runtime::{
    new_service_mother_token, run_service_mother, ServiceManager, ServiceMotherReady,
};

pub struct ServiceMotherProcess {
    manager: ServiceManager,
    child: Child,
    _liveness: ChildStdin,
    alias: PathBuf,
}

impl ServiceMotherProcess {
    pub fn manager(&self) -> ServiceManager {
        self.manager.clone()
    }

    pub async fn shutdown(mut self, timeout: Duration) {
        let started = tokio::time::Instant::now();
        if tokio::time::timeout(timeout, self.manager.shutdown_all())
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "Service Mother shutdown RPC exceeded graceful shutdown budget"
            );
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
            let _ = std::fs::remove_file(&self.alias);
            return;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, self.child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(%status, "Service Mother exited after shutdown");
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed waiting for Service Mother shutdown");
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "Service Mother exceeded graceful shutdown budget; terminating process"
                );
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
        let _ = std::fs::remove_file(&self.alias);
    }
}

pub async fn run_child(args: &[String]) -> anyhow::Result<()> {
    let token = flag_value(args, "--service-token")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("backend --service-mother requires --service-token <token>")
        })?;
    if !args
        .iter()
        .any(|arg| arg == "--launch-separate" || arg == "--launch-saperate")
    {
        anyhow::bail!("backend --service-mother requires --launch-separate");
    }

    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".into());
    let config = config::Config::load(&settings_path).map_err(|error| {
        anyhow::anyhow!("Service Mother failed to load {settings_path}: {error}")
    })?;
    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile(&config.services, &io)?;
    let manager = match catalog.as_ref() {
        Some(catalog) => ServiceManager::spawn_all(catalog).await?,
        None => ServiceManager::default(),
    };
    tracing::info!(
        pid = std::process::id(),
        services = catalog
            .as_ref()
            .map(|catalog| catalog.services().len())
            .unwrap_or(0),
        "Service Mother runtime ready"
    );
    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    match parent_liveness.as_mut() {
        Some(parent_liveness) => {
            tokio::select! {
                result = run_service_mother(manager.clone(), token) => result,
                _ = parent_liveness => {
                    tracing::warn!(
                        "Service Mother parent liveness pipe closed; shutting down managed services"
                    );
                    manager.shutdown_all().await;
                    Ok(())
                }
            }
        }
        None => run_service_mother(manager, token).await,
    }
}

pub async fn spawn(settings_path: impl AsRef<Path>) -> anyhow::Result<ServiceMotherProcess> {
    let exe = std::env::current_exe().context("resolve backend executable for Service Mother")?;
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backend executable has no parent directory"))?;
    let process_dir = parent.join(".runtime").join("process");
    std::fs::create_dir_all(&process_dir)?;
    let extension = exe
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let alias = process_dir.join(format!(
        "rbe-service-mother-parent-{}{}",
        std::process::id(),
        extension
    ));
    let _ = std::fs::remove_file(&alias);
    if std::fs::hard_link(&exe, &alias).is_err() {
        std::fs::copy(&exe, &alias)
            .with_context(|| format!("create Service Mother process alias {}", alias.display()))?;
    }

    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
        format!(
            "canonicalize Service Mother settings path {}",
            settings_path.as_ref().display()
        )
    })?;
    let token = new_service_mother_token();
    let mut child = match Command::new(&alias)
        .args(["--service-mother", "--launch-separate", "--service-token"])
        .arg(&token)
        .current_dir(parent)
        .env("SETTINGS_PATH", &settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&alias);
            return Err(error.into());
        }
    };

    let liveness = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("Service Mother parent liveness pipe unavailable");
        }
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Service Mother stdout unavailable"))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let bytes =
        match tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                cleanup_failed_spawn(&alias, &mut child).await;
                return Err(error.into());
            }
            Err(_) => {
                cleanup_failed_spawn(&alias, &mut child).await;
                anyhow::bail!("Service Mother readiness timed out");
            }
        };
    if bytes == 0 {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("Service Mother exited before readiness");
    }
    let ready: ServiceMotherReady = match serde_json::from_str(line.trim()) {
        Ok(ready) => ready,
        Err(error) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(error.into());
        }
    };
    if !ready.address.ip().is_loopback() {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("Service Mother advertised a non-loopback endpoint");
    }
    if child.id() != Some(ready.pid) {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("Service Mother readiness PID does not match child process");
    }

    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return,
                Ok(_) => {
                    let output = line.trim_end();
                    if !output.is_empty() {
                        tracing::info!(%output, "Service Mother stdout");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "failed to drain Service Mother stdout");
                    return;
                }
            }
        }
    });

    let manager = ServiceManager::remote(ready.address, token)?;
    tracing::info!(
        pid = ready.pid,
        address = %ready.address,
        settings = %settings_path.display(),
        "Service Mother process ready"
    );
    Ok(ServiceMotherProcess {
        manager,
        child,
        _liveness: liveness,
        alias,
    })
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

async fn cleanup_failed_spawn(alias: &Path, child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(alias);
}
