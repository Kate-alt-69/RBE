use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REQUEST_VERSION: u8 = 1;
const REQUEST_MAX_BYTES: u64 = 4 * 1024;
const REQUEST_TTL: Duration = Duration::from_secs(60);
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartCommand {
    Whole,
    Service(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestartRequest {
    version: u8,
    kind: String,
    target: Option<String>,
    created_unix_ms: u64,
}

#[derive(Debug, Default)]
struct RestartBatch {
    whole: bool,
    services: BTreeSet<String>,
}

pub fn command_from_args(args: &[String]) -> anyhow::Result<Option<RestartCommand>> {
    if args
        .iter()
        .any(|arg| arg == "-restart-whole" || arg == "--restart-whole")
    {
        return Ok(Some(RestartCommand::Whole));
    }

    if let Some(target) = args
        .windows(2)
        .find(|pair| pair[0] == "--restart-service")
        .map(|pair| pair[1].clone())
    {
        return Ok(Some(RestartCommand::Service(validate_target(&target)?)));
    }

    for arg in args {
        if let Some(target) = arg.strip_prefix("-restart-") {
            if target == "whole" {
                return Ok(Some(RestartCommand::Whole));
            }
            return Ok(Some(RestartCommand::Service(validate_target(target)?)));
        }
    }
    Ok(None)
}

pub fn submit(command: &RestartCommand) -> anyhow::Result<PathBuf> {
    let directory = control_dir();
    ensure_control_dir(&directory)?;
    let created_unix_ms = unix_millis();
    let (kind, target) = match command {
        RestartCommand::Whole => ("whole", None),
        RestartCommand::Service(target) => ("service", Some(target.as_str())),
    };
    let seed = format!(
        "{created_unix_ms}:{}:{kind}:{}",
        std::process::id(),
        target.unwrap_or_default()
    );
    let digest = Sha256::digest(seed.as_bytes());
    let id = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let final_path = directory.join(format!("restart-{id}.request"));
    let temporary = directory.join(format!(".restart-{id}.tmp"));
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": REQUEST_VERSION,
        "kind": kind,
        "target": target,
        "createdUnixMs": created_unix_ms,
        "requesterPid": std::process::id(),
    }))?;
    if payload.len() as u64 > REQUEST_MAX_BYTES {
        anyhow::bail!("service restart request exceeded {REQUEST_MAX_BYTES} bytes");
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    secure_request_file(&temporary)?;
    drop(file);
    fs::rename(&temporary, &final_path)?;
    Ok(final_path)
}

pub async fn run_mother_control(manager: service_runtime::ServiceManager) {
    let mut interval = tokio::time::interval(CONTROL_POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let batch = match take_restart_requests() {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!(error = %error, "failed to read Service restart control queue");
                continue;
            }
        };
        if batch.whole {
            tracing::warn!("explicit whole Service runtime restart requested");
            return;
        }
        for target in batch.services {
            match manager.restart_service(&target).await {
                Ok(service) => tracing::info!(
                    requested = %target,
                    service = %service,
                    "accepted explicit Service restart request"
                ),
                Err(error) => tracing::error!(
                    requested = %target,
                    error = %error,
                    "Service restart request could not be accepted"
                ),
            }
        }
    }
}

fn validate_target(raw: &str) -> anyhow::Result<String> {
    let target = raw.trim();
    if target.is_empty() || target.len() > 128 {
        anyhow::bail!("service restart target must contain 1..=128 bytes");
    }
    if !target
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        anyhow::bail!(
            "service restart target {target:?} may contain only ASCII letters, digits, '-', '_' and '.'"
        );
    }
    Ok(target.to_string())
}

fn control_dir() -> PathBuf {
    runtime_paths::default_admin_dir().join("service-control")
}

fn ensure_control_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn secure_request_file(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn take_restart_requests() -> anyhow::Result<RestartBatch> {
    let directory = control_dir();
    ensure_control_dir(&directory)?;
    let mut paths = fs::read_dir(&directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("request"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut batch = RestartBatch::default();
    let now = unix_millis();
    let ttl_ms = REQUEST_TTL.as_millis().min(u128::from(u64::MAX)) as u64;
    for path in paths {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not inspect Service restart request");
                continue;
            }
        };
        if metadata.len() > REQUEST_MAX_BYTES {
            tracing::warn!(path = %path.display(), bytes = metadata.len(), "discarding oversized Service restart request");
            let _ = fs::remove_file(&path);
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read Service restart request");
                continue;
            }
        };
        let request = match serde_json::from_slice::<RestartRequest>(&bytes) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "discarding malformed Service restart request");
                let _ = fs::remove_file(&path);
                continue;
            }
        };
        let _ = fs::remove_file(&path);

        if request.version != REQUEST_VERSION {
            tracing::warn!(
                version = request.version,
                "discarding unsupported Service restart request version"
            );
            continue;
        }
        let age = now.saturating_sub(request.created_unix_ms);
        if request.created_unix_ms > now.saturating_add(5_000) || age > ttl_ms {
            tracing::warn!(age_ms = age, "discarding stale Service restart request");
            continue;
        }
        match request.kind.as_str() {
            "whole" => batch.whole = true,
            "service" => match request.target {
                Some(target) => match validate_target(&target) {
                    Ok(target) => {
                        batch.services.insert(target);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "discarding invalid Service restart target")
                    }
                },
                None => tracing::warn!("discarding Service restart request without target"),
            },
            other => {
                tracing::warn!(kind = %other, "discarding unknown Service restart request kind")
            }
        }
    }
    Ok(batch)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_requested_restart_forms() {
        assert_eq!(
            command_from_args(&args(&["-restart-whole"])).unwrap(),
            Some(RestartCommand::Whole)
        );
        assert_eq!(
            command_from_args(&args(&["-restart-auth.service"])).unwrap(),
            Some(RestartCommand::Service("auth.service".into()))
        );
        assert_eq!(
            command_from_args(&args(&["--restart-service", "cache.service"])).unwrap(),
            Some(RestartCommand::Service("cache.service".into()))
        );
    }

    #[test]
    fn rejects_path_traversal_restart_target() {
        assert!(command_from_args(&args(&["-restart-../auth.service"])).is_err());
    }
}
