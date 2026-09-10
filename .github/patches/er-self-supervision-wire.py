from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_between(text: str, start: str, end: str, replacement: str, label: str) -> str:
    start_at = text.find(start)
    if start_at < 0:
        raise SystemExit(f"missing start anchor: {label}")
    end_at = text.find(end, start_at)
    if end_at < 0:
        raise SystemExit(f"missing end anchor: {label}")
    return text[:start_at] + replacement + text[end_at:]


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing anchor: {label}")
    return text.replace(old, new, 1)


path = "engine/crates/backend/src/main.rs"
text = read(path)
text = replace_between(
    text,
    "fn spawn_error_reporter_daemon_process(\n",
    "fn resolve_settings_path() -> String {\n",
    r'''fn spawn_error_reporter_daemon_process(
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
    control_key: Option<host_bootstrap::ErControlKey>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let exe = std::env::current_exe().map_err(|error| {
        anyhow::anyhow!("could not resolve current_exe to spawn the error-reporter daemon: {error}")
    })?;
    Ok(tokio::spawn(async move {
        const STABLE_WINDOW: Duration = Duration::from_secs(60);
        let authority = if control_key.is_some() {
            "control"
        } else {
            "basic"
        };
        let mut consecutive_failures = 0u32;

        loop {
            let frame = control_key
                .as_ref()
                .map(error_reporter_daemon::ParentBootstrapFrame::control);
            let mut command = tokio::process::Command::new(&exe);
            command.args(["--er", "--separate-process", "--launch"]);
            if frame.is_some() {
                command
                    .arg("--er-bootstrap-stdin")
                    .stdin(std::process::Stdio::piped());
            } else {
                command.stdin(std::process::Stdio::null());
            }

            let mut child = match command.kill_on_drop(true).spawn() {
                Ok(child) => child,
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = er_restart_delay(consecutive_failures);
                    report_er_supervisor_failure(
                        "spawn-failed",
                        None,
                        None,
                        None,
                        None,
                        Duration::ZERO,
                        consecutive_failures,
                        authority,
                        Some(&error.to_string()),
                    );
                    tracing::error!(
                        consecutive_failures,
                        backoff_ms = delay.as_millis() as u64,
                        error = %error,
                        "failed to spawn error-reporter daemon process"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            let pid = child.id();
            let started_at = tokio::time::Instant::now();

            if let Some(frame) = frame {
                use tokio::io::AsyncWriteExt;
                let encoded = match serde_json::to_vec(&frame) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let delay = er_restart_delay(consecutive_failures);
                        report_er_supervisor_failure(
                            "bootstrap-serialize-failed",
                            pid,
                            None,
                            None,
                            None,
                            started_at.elapsed(),
                            consecutive_failures,
                            authority,
                            Some(&error.to_string()),
                        );
                        tracing::error!(
                            error = %error,
                            consecutive_failures,
                            backoff_ms = delay.as_millis() as u64,
                            "failed to serialize ER bootstrap frame"
                        );
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                };
                let Some(mut stdin) = child.stdin.take() else {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = er_restart_delay(consecutive_failures);
                    report_er_supervisor_failure(
                        "bootstrap-pipe-unavailable",
                        pid,
                        None,
                        None,
                        None,
                        started_at.elapsed(),
                        consecutive_failures,
                        authority,
                        Some("CONTROL ER child did not expose inherited bootstrap stdin"),
                    );
                    tracing::error!(
                        consecutive_failures,
                        backoff_ms = delay.as_millis() as u64,
                        "CONTROL ER child did not expose inherited bootstrap stdin"
                    );
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    tokio::time::sleep(delay).await;
                    continue;
                };
                if let Err(error) = stdin.write_all(&encoded).await {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = er_restart_delay(consecutive_failures);
                    report_er_supervisor_failure(
                        "bootstrap-write-failed",
                        pid,
                        None,
                        None,
                        None,
                        started_at.elapsed(),
                        consecutive_failures,
                        authority,
                        Some(&error.to_string()),
                    );
                    tracing::error!(
                        error = %error,
                        consecutive_failures,
                        backoff_ms = delay.as_millis() as u64,
                        "failed to deliver ER bootstrap frame"
                    );
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    tokio::time::sleep(delay).await;
                    continue;
                }
                drop(stdin);
            }

            tracing::info!(
                pid,
                authority,
                consecutive_failures,
                "error-reporter daemon process spawned"
            );

            tokio::select! {
                status = child.wait() => {
                    let uptime = started_at.elapsed();
                    if uptime >= STABLE_WINDOW {
                        consecutive_failures = 0;
                    }
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = er_restart_delay(consecutive_failures);
                    match status {
                        Ok(status) => {
                            report_er_supervisor_failure(
                                "unexpected-exit",
                                pid,
                                Some(status.success()),
                                status.code(),
                                er_exit_signal(&status),
                                uptime,
                                consecutive_failures,
                                authority,
                                None,
                            );
                            tracing::warn!(
                                %status,
                                pid,
                                authority,
                                uptime_ms = uptime.as_millis() as u64,
                                consecutive_failures,
                                backoff_ms = delay.as_millis() as u64,
                                "error-reporter daemon exited unexpectedly; replacement scheduled"
                            );
                        }
                        Err(error) => {
                            report_er_supervisor_failure(
                                "wait-failed",
                                pid,
                                None,
                                None,
                                None,
                                uptime,
                                consecutive_failures,
                                authority,
                                Some(&error.to_string()),
                            );
                            tracing::warn!(
                                error = %error,
                                pid,
                                authority,
                                uptime_ms = uptime.as_millis() as u64,
                                consecutive_failures,
                                backoff_ms = delay.as_millis() as u64,
                                "error watching error-reporter daemon process; replacement scheduled"
                            );
                        }
                    }
                    tokio::time::sleep(delay).await;
                }
                _ = tokio::time::sleep(refresh_interval) => {
                    tracing::info!(
                        pid,
                        hours = refresh_interval.as_secs() / 3600,
                        "scheduled error-reporter process refresh"
                    );
                    if let Err(error) = child.kill().await {
                        tracing::warn!(error = %error, "failed to terminate error-reporter for scheduled refresh");
                    }
                    let _ = child.wait().await;
                    consecutive_failures = 0;
                    maintenance.record_error_reporter_refresh();
                }
            }
        }
    }))
}

fn er_restart_delay(attempt: u32) -> Duration {
    const BASE: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = BASE.as_millis().min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(millis.saturating_mul(factor)).min(MAX)
}

fn report_er_supervisor_failure(
    phase: &str,
    pid: Option<u32>,
    exit_success: Option<bool>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    uptime: Duration,
    consecutive_failures: u32,
    authority: &str,
    observation_error: Option<&str>,
) {
    let why = match observation_error {
        Some(error) => format!("supervisor observation/bootstrap failure: {error}"),
        None => match (exit_signal, exit_code, exit_success) {
            (Some(signal), _, _) => format!("terminated by signal {signal}"),
            (_, Some(code), _) => format!("exited with code {code}"),
            (_, _, Some(true)) => "exited successfully but outside a planned refresh".into(),
            _ => "process ended without a portable code or signal".into(),
        },
    };
    let details = serde_json::json!({
        "kind": "critical_process_postmortem",
        "component": "error-reporter-daemon",
        "processImage": if cfg!(windows) { "backend.exe" } else { "backend" },
        "pid": pid,
        "phase": phase,
        "authority": authority,
        "why": why,
        "how": "backend-owned Error Reporter supervisor observed the child state transition",
        "whatWasDoing": "tail-sign-dedupe-and-compact-error-queue",
        "exitSuccess": exit_success,
        "exitCode": exit_code,
        "exitSignal": exit_signal,
        "uptimeMs": uptime.as_millis().min(u128::from(u64::MAX)) as u64,
        "consecutiveFailures": consecutive_failures,
        "recovery": {
            "owner": "backend-local-supervisor",
            "nextBackoffMs": er_restart_delay(consecutive_failures).as_millis() as u64,
            "controlErDecisionAvailable": false,
            "reason": "ER cannot synchronously authorize recovery of its own dead process; the replacement ER signs this queued postmortem after startup"
        }
    });
    let stack = serde_json::to_string(&details).unwrap_or_else(|_| {
        format!(
            "phase={phase} pid={pid:?} authority={authority} why={why} failures={consecutive_failures}"
        )
    });
    error_client::report_issue(error_client::IssueInput {
        source: "backend.er.supervisor",
        level: Some(error_client::IssueLevel::Error),
        category: Some(error_client::IssueCategory::OperationFailure),
        message: "Error Reporter process failed; backend local supervisor scheduled a replacement",
        stack: Some(&stack),
    });
}

#[cfg(unix)]
fn er_exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn er_exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

''',
    "ER self-supervisor",
)

text = replace_once(
    text,
    "mod container_supervision_tests {\n",
    "mod critical_process_supervision_tests {\n",
    "critical supervision test module rename",
)
test_anchor = '''        assert_eq!(\n            container_recovery_backoff(1, Duration::from_secs(300)),\n            Duration::from_secs(30)\n        );\n    }\n}'''
test_replacement = '''        assert_eq!(\n            container_recovery_backoff(1, Duration::from_secs(300)),\n            Duration::from_secs(30)\n        );\n    }\n\n    #[test]\n    fn er_self_recovery_backoff_is_exponential_and_capped() {\n        assert_eq!(er_restart_delay(1), Duration::from_millis(500));\n        assert_eq!(er_restart_delay(2), Duration::from_secs(1));\n        assert_eq!(er_restart_delay(3), Duration::from_secs(2));\n        assert_eq!(er_restart_delay(30), Duration::from_secs(30));\n    }\n}'''
text = replace_once(text, test_anchor, test_replacement, "ER self-supervision backoff test")
write(path, text)

path = "docs/service-runtime.md"
text = read(path)
append = r'''

### Error Reporter self-crash postmortems

ER cannot safely ask itself whether its own dead process should restart. The
backend therefore remains the final recovery owner for the Error Reporter daemon
and uses a bounded local exponential replacement delay (500 ms through 30 s).
A process that survives the 60-second stable window resets the crash streak.
Scheduled ER refreshes remain planned maintenance and do not create crash
postmortems or inflate the crash counter.

For every unexpected ER exit, wait failure, spawn failure, or CONTROL bootstrap
failure, backend writes one normal `error-client` issue into the existing bounded
queue. The replacement ER later consumes and signs that record. The structured
postmortem says `why`, `how`, `whatWasDoing`, child PID when available, authority
mode, exit code or Unix signal, uptime, failure streak, and the local recovery
backoff. It explicitly records that no synchronous CONTROL-ER decision was
possible because the decision engine was the failed process itself.

The self-postmortem contains no CONTROL key, report-signing key, queue contents,
issue bodies, Runtime ENV values, credentials, request data, or arbitrary process
memory. It goes through `error-client`'s normal redaction, de-duplication, and
bounded queue path before the replacement ER signs it. This avoids an ER
self-dependency while still preserving enough failure chronology for later
diagnostics.
'''
if "### Error Reporter self-crash postmortems" not in text:
    text = text.rstrip() + append + "\n"
write(path, text)
