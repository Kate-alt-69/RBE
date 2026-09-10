from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing anchor: {label}")
    return text.replace(old, new, 1)


def replace_between(text: str, start: str, end: str, replacement: str, label: str) -> str:
    start_at = text.find(start)
    if start_at < 0:
        raise SystemExit(f"missing start anchor: {label}")
    end_at = text.find(end, start_at)
    if end_at < 0:
        raise SystemExit(f"missing end anchor: {label}")
    return text[:start_at] + replacement + text[end_at:]


# ---------------------------------------------------------------------------
# Preserve container process identity/uptime after Child::try_wait reaps the
# process so backend supervision can produce useful metadata-only ER reports.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/container_process.rs"
text = read(path)
text = replace_once(
    text,
    "use std::path::{Path, PathBuf};\n",
    "use std::path::{Path, PathBuf};\nuse std::process::ExitStatus;\n",
    "container ExitStatus import",
)
text = replace_once(
    text,
    "use std::time::Duration;\n",
    "use std::time::{Duration, Instant};\n",
    "container Instant import",
)
text = replace_once(
    text,
    '''pub struct ContainerProcess {\n    child: Child,\n    pub address: SocketAddr,\n    token: String,\n}''',
    '''pub struct ContainerProcess {\n    child: Child,\n    pub address: SocketAddr,\n    token: String,\n    pid: Option<u32>,\n    started_at: Instant,\n}''',
    "ContainerProcess metadata fields",
)
text = replace_once(
    text,
    '''            let child = command.spawn().map_err(|err| {\n                anyhow::anyhow!(\n                    \"failed to spawn verified container process {}: {err}\",\n                    binary.display()\n                )\n            })?;\n\n            let mut process = Self {\n                child,\n                address,\n                token,\n            };''',
    '''            let child = command.spawn().map_err(|err| {\n                anyhow::anyhow!(\n                    \"failed to spawn verified container process {}: {err}\",\n                    binary.display()\n                )\n            })?;\n            let pid = child.id();\n\n            let mut process = Self {\n                child,\n                address,\n                token,\n                pid,\n                started_at: Instant::now(),\n            };''',
    "ContainerProcess construction",
)
text = replace_once(
    text,
    '''    pub fn endpoint(&self) -> (SocketAddr, String, Option<u32>) {\n        (self.address, self.token.clone(), self.child.id())\n    }\n}''',
    '''    pub fn endpoint(&self) -> (SocketAddr, String, Option<u32>) {\n        (self.address, self.token.clone(), self.pid)\n    }\n\n    pub fn pid(&self) -> Option<u32> {\n        self.pid\n    }\n\n    pub fn uptime(&self) -> Duration {\n        self.started_at.elapsed()\n    }\n\n    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {\n        self.child.try_wait()\n    }\n}''',
    "ContainerProcess supervision methods",
)
write(path, text)


# ---------------------------------------------------------------------------
# Upgrade the old scheduled-only refresh task into a real critical-process
# supervisor. Scheduled rolling refreshes stay distinct from unexpected exits.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
text = replace_once(
    text,
    "use std::collections::{HashMap, HashSet};\n",
    "use std::collections::{BTreeMap, HashMap, HashSet};\n",
    "backend BTreeMap import",
)
text = replace_once(
    text,
    '''    let container_refresh_task = spawn_container_refresh(\n        container_path.clone(),\n        config.containers.clone(),\n        container_process.clone(),\n        container_client.clone(),\n        maintenance.clone(),\n        refresh_interval,\n    );''',
    '''    let container_supervisor_task = spawn_container_supervisor(\n        container_path.clone(),\n        config.containers.clone(),\n        container_process.clone(),\n        container_client.clone(),\n        maintenance.clone(),\n        refresh_interval,\n        er_control_key.clone(),\n    );''',
    "container supervisor startup",
)
text = replace_once(
    text,
    "    container_refresh_task.abort();\n",
    "    container_supervisor_task.abort();\n",
    "container supervisor shutdown",
)

text = replace_between(
    text,
    "fn spawn_container_refresh(\n",
    "fn spawn_vault_refresh(\n",
    r'''fn spawn_container_supervisor(
    binary: PathBuf,
    settings: config::ContainersConfig,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
    client: ContainerClient,
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
    er_control_key: Option<host_bootstrap::ErControlKey>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        const DRAIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
        const MONITOR_INTERVAL: Duration = Duration::from_millis(250);
        const STABLE_WINDOW: Duration = Duration::from_secs(60);

        let mut refresh = tokio::time::interval(refresh_interval);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        refresh.tick().await;
        let mut monitor = tokio::time::interval(MONITOR_INTERVAL);
        monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        monitor.tick().await;
        let mut restart_attempts = 0u32;

        loop {
            tokio::select! {
                _ = monitor.tick() => {
                    let observed = {
                        let mut guard = process.lock().await;
                        let pid = guard.pid().unwrap_or_default();
                        let uptime = guard.uptime();
                        match guard.try_wait() {
                            Ok(Some(status)) => Some((pid, uptime, status)),
                            Ok(None) => None,
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    pid,
                                    "failed to inspect verified container process; retaining current process identity"
                                );
                                None
                            }
                        }
                    };
                    let Some((pid, uptime, status)) = observed else {
                        continue;
                    };

                    if uptime >= STABLE_WINDOW {
                        restart_attempts = 0;
                    }
                    let generation = client.endpoint_snapshot().generation;
                    let report = container_exit_report(
                        pid,
                        &status,
                        uptime,
                        restart_attempts,
                        generation,
                        &settings,
                    );
                    let (authority_minimum_backoff, authority_reason) =
                        decide_container_recovery(er_control_key.as_ref(), report).await;

                    loop {
                        restart_attempts = restart_attempts.saturating_add(1);
                        let delay = container_recovery_backoff(
                            restart_attempts,
                            authority_minimum_backoff,
                        );
                        tracing::warn!(
                            pid,
                            %status,
                            attempt = restart_attempts,
                            backoff_ms = delay.as_millis() as u64,
                            authority_reason = authority_reason
                                .as_deref()
                                .unwrap_or("local critical-process policy"),
                            "verified container process exited unexpectedly; scheduling replacement"
                        );
                        tokio::time::sleep(delay).await;

                        match container_process::ContainerProcess::spawn(&binary, &settings).await {
                            Ok(replacement) => {
                                let (address, token, new_pid) = replacement.endpoint();
                                let old = {
                                    let mut guard = process.lock().await;
                                    std::mem::replace(&mut *guard, replacement)
                                };
                                client.update_endpoint(address, token, new_pid);
                                drop(old);
                                tracing::info!(
                                    old_pid = pid,
                                    pid = new_pid,
                                    %address,
                                    attempt = restart_attempts,
                                    authority_reason = authority_reason
                                        .as_deref()
                                        .unwrap_or("local critical-process policy"),
                                    "verified container replacement healthy; IPC switched to recovered process"
                                );
                                break;
                            }
                            Err(error) => {
                                tracing::error!(
                                    old_pid = pid,
                                    attempt = restart_attempts,
                                    error = %error,
                                    "verified container crash replacement failed; retrying with bounded backoff"
                                );
                            }
                        }
                    }
                }
                _ = refresh.tick() => {
                    tracing::info!(
                        hours = refresh_interval.as_secs() / 3600,
                        "starting scheduled rolling container refresh"
                    );

                    if let Err(error) = client.prepare_refresh(DRAIN_TIMEOUT).await {
                        tracing::warn!(
                            error = %error,
                            "container did not complete refresh drain; retaining current process"
                        );
                        if let Err(resume_error) = client.resume().await {
                            tracing::error!(
                                error = %resume_error,
                                "failed to resume container after refresh drain failure"
                            );
                        }
                        continue;
                    }

                    match container_process::ContainerProcess::spawn(&binary, &settings).await {
                        Ok(replacement) => {
                            let (address, token, pid) = replacement.endpoint();
                            let old = {
                                let mut guard = process.lock().await;
                                std::mem::replace(&mut *guard, replacement)
                            };
                            client.update_endpoint(address, token, pid);
                            maintenance.record_container_refresh();
                            restart_attempts = 0;
                            tracing::info!(
                                pid,
                                %address,
                                "replacement container healthy; IPC switched to new process"
                            );
                            // The old process has already stopped accepting work and is
                            // confirmed idle. This short grace only lets in-flight
                            // health/inspection IPC calls release their old socket.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            drop(old);
                        }
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "scheduled container replacement failed; resuming existing healthy container"
                            );
                            if let Err(resume_error) = client.resume().await {
                                tracing::error!(
                                    error = %resume_error,
                                    "failed to resume existing container after replacement failure"
                                );
                            }
                        }
                    }
                }
            }
        }
    })
}

fn container_exit_report(
    pid: u32,
    status: &std::process::ExitStatus,
    uptime: Duration,
    previous_restart_attempts: u32,
    endpoint_generation: u64,
    settings: &config::ContainersConfig,
) -> er_recovery::ProcessExitReport {
    let mut context = BTreeMap::new();
    context.insert(
        "endpoint_generation".into(),
        endpoint_generation.to_string(),
    );
    context.insert(
        "configured_environments".into(),
        settings.environments.to_string(),
    );
    context.insert("supervision_scope".into(), "execution-runtime".into());

    er_recovery::ProcessExitReport {
        component: "container-runtime".into(),
        process_image: if cfg!(windows) {
            "container.exe".into()
        } else {
            "container".into()
        },
        pid,
        exit_success: status.success(),
        exit_code: status.code(),
        exit_signal: container_exit_signal(status),
        previous_restart_attempts,
        uptime_ms: uptime.as_millis().min(u128::from(u64::MAX)) as u64,
        expected: false,
        phase: "runtime-supervision".into(),
        last_operation: Some("serve-authenticated-container-control".into()),
        observation_error: None,
        context,
    }
}

async fn decide_container_recovery(
    key: Option<&host_bootstrap::ErControlKey>,
    report: er_recovery::ProcessExitReport,
) -> (Duration, Option<String>) {
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let Some(key) = key else {
        return (Duration::ZERO, None);
    };
    let client = er_recovery::ErRecoveryClient::new(key.clone());
    match tokio::time::timeout(Duration::from_millis(700), client.decide_process(report)).await {
        Ok(Ok(service_runtime::ServiceRestartDirective::Restart {
            minimum_backoff_ms,
            reason,
        })) => (
            Duration::from_millis(minimum_backoff_ms).min(MAX_BACKOFF),
            Some(reason),
        ),
        Ok(Ok(service_runtime::ServiceRestartDirective::Default)) => (Duration::ZERO, None),
        Ok(Ok(service_runtime::ServiceRestartDirective::Stop { reason })) => {
            tracing::error!(
                authority_reason = %reason,
                "CONTROL ER requested Stop for unexpected critical container exit; ignoring unsafe stop directive"
            );
            (
                Duration::ZERO,
                Some(format!("unsafe CONTROL ER stop ignored: {reason}")),
            )
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "CONTROL ER container decision failed; using local critical-process policy"
            );
            (Duration::ZERO, None)
        }
        Err(_) => {
            tracing::warn!(
                "CONTROL ER container decision timed out; using local critical-process policy"
            );
            (Duration::ZERO, None)
        }
    }
}

fn container_restart_delay(attempt: u32) -> Duration {
    const BASE: Duration = Duration::from_millis(250);
    const MAX: Duration = Duration::from_secs(30);
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = BASE.as_millis().min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(millis.saturating_mul(factor)).min(MAX)
}

fn container_recovery_backoff(attempt: u32, authority_minimum: Duration) -> Duration {
    const MAX: Duration = Duration::from_secs(30);
    container_restart_delay(attempt)
        .max(authority_minimum)
        .min(MAX)
}

#[cfg(unix)]
fn container_exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn container_exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod container_supervision_tests {
    use super::*;

    #[test]
    fn container_recovery_backoff_is_exponential_capped_and_never_shortened() {
        assert_eq!(container_restart_delay(1), Duration::from_millis(250));
        assert_eq!(container_restart_delay(2), Duration::from_millis(500));
        assert_eq!(container_restart_delay(3), Duration::from_secs(1));
        assert_eq!(container_restart_delay(30), Duration::from_secs(30));
        assert_eq!(
            container_recovery_backoff(1, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            container_recovery_backoff(6, Duration::from_millis(1)),
            container_restart_delay(6)
        );
        assert_eq!(
            container_recovery_backoff(1, Duration::from_secs(300)),
            Duration::from_secs(30)
        );
    }
}

''',
    "container critical-process supervisor",
)
write(path, text)


# ---------------------------------------------------------------------------
# Document scheduled-vs-crash semantics and the metadata ER sees.
# ---------------------------------------------------------------------------
path = "docs/container-integrity.md"
text = read(path)
append = r'''

## Runtime crash supervision

The verified container dependency is now supervised continuously, not only at
scheduled refresh time. Backend polls the owned child at a bounded interval. An
unexpected exit keeps the existing `ContainerClient` generation unavailable only
until a cryptographically verified replacement becomes healthy; backend then
atomically retargets the shared client to the replacement endpoint. Rapid
failures use exponential backoff from 250 ms up to 30 seconds, and a process
that survives the stable window resets accumulated crash attempts.

Scheduled rolling refresh remains a separate path: backend asks the healthy
container to drain, starts and verifies a replacement, switches IPC, and only
then releases the old process. A planned refresh is therefore not reported as a
crash and does not inflate crash-loop attempts.

When HostBootstrap granted CONTROL ER, an unexpected container exit also emits
a bounded authenticated `ProcessExitReport` before replacement. The report
contains only the stable component/image identity, PID, exit code or Unix
signal, uptime, previous recovery attempts, endpoint generation, configured
environment count, runtime phase, supervision scope, and a fixed activity label.
The container authentication token, request payloads, execution data, Vault
credentials, Runtime ENV values, and worker memory are never included. CONTROL
ER may increase the minimum recovery delay, but it cannot exceed the local
30-second cap or permanently stop this critical process; BASIC/dead/timed-out
ER falls back to the local supervisor.
'''
if "## Runtime crash supervision" not in text:
    text = text.rstrip() + append + "\n"
write(path, text)
