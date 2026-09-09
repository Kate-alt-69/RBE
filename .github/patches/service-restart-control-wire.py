from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Portable restart request queue. The CLI never needs the Service Mother auth
# token. Requests are short-lived, atomic files in RBE's binary-relative admin
# directory. Mother is the only consumer and still owns all process lifecycle.
# ---------------------------------------------------------------------------
write(
    "engine/crates/backend/src/service_control.rs",
    r'''use std::collections::BTreeSet;
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
    if args.iter().any(|arg| arg == "-restart-whole" || arg == "--restart-whole") {
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
            tracing::warn!(version = request.version, "discarding unsupported Service restart request version");
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
                    Err(error) => tracing::warn!(%error, "discarding invalid Service restart target"),
                },
                None => tracing::warn!("discarding Service restart request without target"),
            },
            other => tracing::warn!(kind = %other, "discarding unknown Service restart request kind"),
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
''',
)


# ---------------------------------------------------------------------------
# service.exe CLI dispatch happens before internal Mother/worker mode checks.
# backend.exe itself never accepts these service-control commands.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
if "mod service_control;" not in text:
    text = text.replace("mod service_boot;", "mod service_boot;\nmod service_control;", 1)
anchor = '''    let has = |flag: &str| args.iter().any(|arg| arg == flag);\n\n'''
if anchor not in text:
    raise SystemExit("missing backend args anchor for service control")
branch = r'''    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if running_as_service_executable() {
        match service_control::command_from_args(&args) {
            Ok(Some(command)) => {
                match service_control::submit(&command) {
                    Ok(path) => {
                        println!(
                            "Service restart request queued: {}",
                            path.display()
                        );
                    }
                    Err(error) => {
                        eprintln!("failed to queue Service restart request: {error:#}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("invalid Service restart command: {error:#}");
                std::process::exit(2);
            }
        }
    }

'''
text = text.replace(anchor, branch, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Mother runs one local control queue task. A whole-runtime request exits the
# Mother only after graceful child shutdown; backend's existing supervisor then
# recreates Mother and therefore the whole Service process tree.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
old = r'''    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    match parent_liveness.as_mut() {
        Some(parent_liveness) => {
            tokio::select! {
                result = &mut server_task => result??,
                _ = parent_liveness => {
                    tracing::warn!(
                        "Service Mother parent liveness pipe closed; shutting down managed services"
                    );
                    manager.shutdown_all().await;
                    server_task.abort();
                    let _ = (&mut server_task).await;
                }
            }
        }
        None => server_task.await??,
    }
    Ok(())
'''
if old not in text:
    raise SystemExit("missing Service Mother liveness select anchor")
new = r'''    let control_manager = manager.clone();
    let mut control_task = tokio::spawn(async move {
        crate::service_control::run_mother_control(control_manager).await;
    });
    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    let whole_restart_requested = match parent_liveness.as_mut() {
        Some(parent_liveness) => {
            tokio::select! {
                result = &mut server_task => {
                    control_task.abort();
                    result??;
                    false
                }
                result = &mut control_task => {
                    result.map_err(|error| anyhow::anyhow!("Service control task failed: {error}"))?;
                    true
                }
                _ = parent_liveness => {
                    control_task.abort();
                    tracing::warn!(
                        "Service Mother parent liveness pipe closed; shutting down managed services"
                    );
                    manager.shutdown_all().await;
                    server_task.abort();
                    let _ = (&mut server_task).await;
                    false
                }
            }
        }
        None => {
            tokio::select! {
                result = &mut server_task => {
                    control_task.abort();
                    result??;
                    false
                }
                result = &mut control_task => {
                    result.map_err(|error| anyhow::anyhow!("Service control task failed: {error}"))?;
                    true
                }
            }
        }
    };
    if whole_restart_requested {
        tracing::warn!("restarting complete Service runtime by explicit operator request");
        manager.shutdown_all().await;
        server_task.abort();
        let _ = (&mut server_task).await;
    }
    Ok(())
'''
text = text.replace(old, new, 1)
# Planned clean exits are valid because -restart-whole intentionally exits the
# Mother; don't call every successful exit "unexpected" in the parent logs.
text = text.replace(
    'Ok(status) => tracing::warn!(%status, uptime_ms = uptime.as_millis(), "Service Mother exited unexpectedly; supervising replacement"),',
    'Ok(status) => tracing::warn!(%status, uptime_ms = uptime.as_millis(), "Service Mother exited; supervising replacement"),',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# ServiceManager explicit restart path. It overrides per-service crash policy
# because the operator deliberately requested a restart. If replacement boot
# fails it keeps retrying with bounded exponential backoff instead of hot-looping.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
const_anchor = '''const SERVICE_STDOUT_LINE_MAX_BYTES: usize = 64 * 1024;\n'''
if const_anchor not in text:
    raise SystemExit("missing service manager constants anchor")
text = text.replace(
    const_anchor,
    const_anchor
    + '''const MANUAL_RESTART_MAX_BACKOFF: Duration = Duration::from_secs(30);\nconst CRASH_LOOP_BACKOFF_THRESHOLD: u32 = 6;\n''',
    1,
)
text = text.replace(
    '''pub enum ServiceRuntimeState {\n    Dormant,\n    Running,\n    Restarting,\n    Stopped,\n    Unknown,\n}''',
    '''pub enum ServiceRuntimeState {\n    Dormant,\n    Running,\n    Restarting,\n    CrashLoopBackoff,\n    Stopped,\n    Unknown,\n}''',
    1,
)

call_anchor = '''    pub async fn call(\n        &self,\n        service_name: &str,\n'''
if call_anchor not in text:
    raise SystemExit("missing ServiceManager call anchor")
restart_methods = r'''    pub async fn restart_service(&self, target: &str) -> Result<String, ServiceCallError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ServiceCallError::Unavailable {
                service: target.to_string(),
            });
        }
        if self.mother.is_some() {
            return Err(ServiceCallError::Unavailable {
                service: target.to_string(),
            });
        }
        let (service_name, handle) = self.find_restart_target(target).await?;
        let (file, old_process) = {
            let mut service = handle.lock().await;
            if service.restarting {
                tracing::info!(
                    service = %service.file.name,
                    "explicit restart request joined an already-running restart"
                );
                return Ok(service.file.name.clone());
            }
            service.restarting = true;
            service.exit_observed = false;
            service.restart_attempts = 0;
            (service.file.clone(), service.process.take())
        };

        let manager = self.clone();
        tokio::spawn(async move {
            if let Some(mut process) = old_process {
                stop_process(&file.name, &mut process).await;
            }
            manager
                .restart_requested_until_ready(handle, file)
                .await;
        });
        Ok(service_name)
    }

    async fn find_restart_target(
        &self,
        target: &str,
    ) -> Result<(String, Arc<Mutex<Managed>>), ServiceCallError> {
        let target = target.trim();
        let without_extension = target.strip_suffix(".service").unwrap_or(target);
        let handles = {
            let services = self.services.read().await;
            if let Some(handle) = services.get(target) {
                return Ok((target.to_string(), handle.clone()));
            }
            if let Some(handle) = services.get(without_extension) {
                return Ok((without_extension.to_string(), handle.clone()));
            }
            services.values().cloned().collect::<Vec<_>>()
        };

        for handle in handles {
            let service = handle.lock().await;
            let filename = service
                .file
                .path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let stem = service
                .file
                .path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if target == filename || target == stem || without_extension == stem {
                return Ok((service.file.name.clone(), handle.clone()));
            }
        }

        Err(ServiceCallError::Unknown {
            service: target.to_string(),
        })
    }

    async fn restart_requested_until_ready(
        &self,
        handle: Arc<Mutex<Managed>>,
        file: ServiceFile,
    ) {
        let mut attempt = 0u32;
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                let mut service = handle.lock().await;
                service.restarting = false;
                return;
            }
            attempt = attempt.saturating_add(1);
            match spawn_process(&file, self.fabric.as_ref(), self.runtime_env.as_deref()).await {
                Ok(mut replacement) => {
                    let pid = replacement.ready.pid;
                    let mut service = handle.lock().await;
                    if self.shutting_down.load(Ordering::Acquire) || !service.restarting {
                        drop(service);
                        stop_process(&file.name, &mut replacement).await;
                        return;
                    }
                    service.process = Some(replacement);
                    service.restart_attempts = 0;
                    service.exit_observed = false;
                    service.restarting = false;
                    service.last_activity = Instant::now();
                    tracing::info!(
                        service = %file.name,
                        pid,
                        attempts = attempt,
                        "explicit Service restart completed"
                    );
                    return;
                }
                Err(error) => {
                    let delay = restart_delay(attempt, MANUAL_RESTART_MAX_BACKOFF);
                    {
                        let mut service = handle.lock().await;
                        if !service.restarting {
                            return;
                        }
                        service.restart_attempts = attempt;
                    }
                    if attempt >= CRASH_LOOP_BACKOFF_THRESHOLD {
                        tracing::error!(
                            service = %file.name,
                            attempt,
                            retry_in_ms = delay.as_millis() as u64,
                            error = %error,
                            "Service remains in crash-loop backoff; retrying explicit restart"
                        );
                    } else {
                        tracing::warn!(
                            service = %file.name,
                            attempt,
                            retry_in_ms = delay.as_millis() as u64,
                            error = %error,
                            "explicit Service restart failed; retry scheduled"
                        );
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

'''
text = text.replace(call_anchor, restart_methods + call_anchor, 1)

# Snapshot should distinguish an ordinary short restart from a sustained crash loop.
old_snapshot_prefix = '''        let restarting = service.restarting;\n        let wakeable = service.wakeable();\n        let exit_observed = service.exit_observed;\n        let restart = service.file.restart;\n'''
if old_snapshot_prefix not in text:
    raise SystemExit("missing service snapshot local-state anchor")
text = text.replace(
    old_snapshot_prefix,
    old_snapshot_prefix + '''        let restart_attempts = service.restart_attempts;\n''',
    1,
)
text = text.replace(
    '''            None if restarting => (None, ServiceRuntimeState::Restarting, None),''',
    '''            None if restarting && restart_attempts >= CRASH_LOOP_BACKOFF_THRESHOLD => {
                (None, ServiceRuntimeState::CrashLoopBackoff, None)
            }
            None if restarting => (None, ServiceRuntimeState::Restarting, None),''',
    1,
)
text = text.replace(
    '''                Ok(Some(_)) if restarting => (None, ServiceRuntimeState::Restarting, None),''',
    '''                Ok(Some(_))
                    if restarting && restart_attempts >= CRASH_LOOP_BACKOFF_THRESHOLD =>
                {
                    (None, ServiceRuntimeState::CrashLoopBackoff, None)
                }
                Ok(Some(_)) if restarting => (None, ServiceRuntimeState::Restarting, None),''',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# Docs: explicit operator restart commands and crash/restart semantics.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
if "-restart-whole" not in text:
    text += r'''

## Process restart controls

The Service runtime intentionally uses OS-process supervision. With the default `restart = on-failure`, terminating one worker through Task Manager, `kill`, `htop`, or another process manager is treated as a failed worker and Mother restarts only that service. Terminating Mother closes every child parent-liveness pipe; the backend supervisor recreates Mother and therefore recreates the complete Service process tree.

Operators can request the same lifecycle explicitly without exposing Mother authentication:

```text
./service.exe -restart-whole
./service.exe -restart-auth.service
./service.exe --restart-service auth.service
```

On Unix the executable is `./service` instead of `./service.exe`. Restart commands write a short-lived atomic request under the binary-relative RBE admin directory. Mother consumes the request and remains the only component allowed to create/replace Service workers.

An explicit per-service restart overrides that service's `restart = never` crash policy for the requested restart. If replacement startup keeps failing, RBE retries with bounded exponential backoff rather than a hot loop; after repeated failures the snapshot state becomes `crash_loop_backoff`, while `restartAttempts` shows continued recovery attempts.
'''
write(path, text)

path = "doc/x.service/README.md"
text = read(path)
if "-restart-whole" not in text:
    text += r'''

### Operator restart behavior

Service processes are intentionally supervisor-friendly. Ending one default (`on-failure`) worker causes Mother to replace only that worker. Ending Mother causes the backend supervisor to replace Mother and rebuild the entire Service process set after the old children lose their liveness pipe.

The same behavior can be requested portably:

```text
service.exe -restart-whole
service.exe -restart-auth.service
```

(`service` without `.exe` on Unix.) Explicit service restart requests retry with bounded exponential backoff and surface sustained failures as `crash_loop_backoff` instead of busy-looping.
'''
write(path, text)
