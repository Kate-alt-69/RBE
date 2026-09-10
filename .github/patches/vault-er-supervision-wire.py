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
# vault-process owns its child on a dedicated blocking I/O thread. Give that
# crate a neutral recovery-authority contract so backend can adapt CONTROL ER
# without creating a backend -> vault-process -> backend dependency cycle.
# ---------------------------------------------------------------------------
path = "vault-process/src/lib.rs"
text = read(path)
text = replace_once(
    text,
    "use std::process::{Child, ChildStdin, Command, Stdio};\n",
    "use std::process::{Child, ChildStdin, Command, Stdio};\nuse std::sync::Arc;\n",
    "vault Arc import",
)
text = replace_once(
    text,
    "use std::time::Duration;\n",
    "use std::time::{Duration, Instant};\n",
    "vault Instant import",
)
text = replace_once(
    text,
    '''const RESTART_DELAY: Duration = Duration::from_millis(200);\nconst MAX_RESTARTS: u32 = 5;''',
    '''const RESTART_DELAY: Duration = Duration::from_millis(200);\nconst RECOVERY_MAX_BACKOFF: Duration = Duration::from_secs(30);\nconst VAULT_STABLE_WINDOW: Duration = Duration::from_secs(60);\nconst MAX_RESTARTS: u32 = 5;''',
    "vault recovery constants",
)

marker = "#[derive(Clone)]\npub struct VaultClient {\n"
insert = r'''#[derive(Debug, Clone)]
pub struct VaultProcessExitReport {
    pub pid: u32,
    pub exit_success: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub previous_restart_attempts: u32,
    pub uptime_ms: u64,
    pub phase: String,
    pub last_operation: Option<String>,
    pub observation_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct VaultRecoveryAdvice {
    pub minimum_backoff: Duration,
    pub reason: Option<String>,
}

/// Optional backend-owned policy hook for unexpected Vault child exits.
/// Implementations run only on the dedicated `vault-process-io` thread and
/// must remain bounded. The Vault worker retains final recovery ownership.
pub trait VaultRecoveryAuthority: Send + Sync {
    fn decide(&self, report: VaultProcessExitReport) -> anyhow::Result<VaultRecoveryAdvice>;
}

'''
if marker not in text:
    raise SystemExit("missing VaultClient marker")
text = text.replace(marker, insert + marker, 1)

text = replace_between(
    text,
    "impl VaultClient {\n    pub fn spawn(",
    "    pub fn credential(",
    r'''impl VaultClient {
    pub fn spawn(service_name: impl Into<String>, data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with_recovery(service_name, data_dir, None)
    }

    pub fn spawn_with_recovery(
        service_name: impl Into<String>,
        data_dir: &Path,
        recovery_authority: Option<Arc<dyn VaultRecoveryAuthority>>,
    ) -> anyhow::Result<Self> {
        let service_name = service_name.into();
        let data_dir = data_dir.to_path_buf();
        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("could not resolve backend executable: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("vault-process-io".into())
            .spawn(move || {
                Worker::new(exe, service_name, data_dir, ready_tx, recovery_authority).run(rx)
            })?;

        match ready_rx.recv_timeout(STARTUP_TARGET) {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("Vault client worker exited during startup"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match ready_rx.recv_timeout(STARTUP_MAX.saturating_sub(STARTUP_TARGET)) {
                    Ok(Ok(())) => Ok(Self { tx }),
                    Ok(Err(err)) => Err(err),
                    Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                        "Vault did not become ready within {:?} (normal target: {:?})",
                        STARTUP_MAX,
                        STARTUP_TARGET
                    )),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Err(anyhow::anyhow!("Vault client worker exited during startup"))
                    }
                }
            }
        }
    }

''',
    "VaultClient spawn API",
)

text = replace_once(
    text,
    '''struct Connection {\n    child: Child,\n    stdin: ChildStdin,\n    reader: BufReader<std::process::ChildStdout>,\n    token: String,\n    seq: u64,\n}''',
    '''struct Connection {\n    child: Child,\n    stdin: ChildStdin,\n    reader: BufReader<std::process::ChildStdout>,\n    token: String,\n    seq: u64,\n    pid: u32,\n    started_at: Instant,\n}''',
    "Vault Connection metadata",
)
text = replace_once(
    text,
    '''struct Worker {\n    exe: PathBuf,\n    service_name: String,\n    data_dir: PathBuf,\n    ready_tx: Option<SyncSender<anyhow::Result<()>>>,\n    connection: Option<Connection>,\n}''',
    '''struct Worker {\n    exe: PathBuf,\n    service_name: String,\n    data_dir: PathBuf,\n    ready_tx: Option<SyncSender<anyhow::Result<()>>>,\n    connection: Option<Connection>,\n    recovery_authority: Option<Arc<dyn VaultRecoveryAuthority>>,\n    unexpected_restart_attempts: u32,\n    last_operation: Option<&'static str>,\n}''',
    "Vault Worker recovery fields",
)

old_ctor = '''    fn new(\n        exe: PathBuf,\n        service_name: String,\n        data_dir: PathBuf,\n        ready_tx: SyncSender<anyhow::Result<()>>,\n    ) -> Self {\n        Self {\n            exe,\n            service_name,\n            data_dir,\n            ready_tx: Some(ready_tx),\n            connection: None,\n        }\n    }'''
new_ctor = '''    fn new(\n        exe: PathBuf,\n        service_name: String,\n        data_dir: PathBuf,\n        ready_tx: SyncSender<anyhow::Result<()>>,\n        recovery_authority: Option<Arc<dyn VaultRecoveryAuthority>>,\n    ) -> Self {\n        Self {\n            exe,\n            service_name,\n            data_dir,\n            ready_tx: Some(ready_tx),\n            connection: None,\n            recovery_authority,\n            unexpected_restart_attempts: 0,\n            last_operation: None,\n        }\n    }'''
text = replace_once(text, old_ctor, new_ctor, "Vault Worker constructor")

text = replace_between(
    text,
    "    fn run(&mut self, rx: Receiver<ClientCommand>) {\n",
    "    fn request(\n",
    r'''    fn run(&mut self, rx: Receiver<ClientCommand>) {
        if let Err(err) = self.ensure_connection() {
            if let Some(tx) = self.ready_tx.take() {
                let _ = tx.send(Err(err));
            }
            return;
        }
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        loop {
            match rx.recv_timeout(MONITOR_INTERVAL) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(report) = self.observe_connection_failure("idle-monitor") {
                        if let Err(error) = self.recover_unexpected(report) {
                            tracing::error!(
                                error = %error,
                                attempt = self.unexpected_restart_attempts,
                                "unexpected Vault child recovery failed"
                            );
                        }
                    } else if self.connection.is_none() {
                        let _ = self.retry_missing_connection();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown_child();
                    return;
                }
            }
        }
    }

    fn handle(&mut self, command: ClientCommand) {
        match command {
            ClientCommand::Refresh { response } => {
                self.last_operation = Some("scheduled-refresh");
                let result = self.restart_connection();
                if result.is_ok() {
                    self.unexpected_restart_attempts = 0;
                }
                let _ = response.send(result);
            }
            ClientCommand::Get {
                name,
                caller,
                response,
            } => {
                self.last_operation = Some("credential-get");
                if let Err(err) = self.ensure_live_connection("client-request") {
                    let _ = response.send(Err(err));
                    return;
                }
                let result = self.request("get", &name, &caller, None);
                let failed = result.is_err();
                let _ = response.send(result);
                if failed {
                    self.recover_if_process_failed("credential-get");
                }
            }
            ClientCommand::Set {
                name,
                value,
                caller,
                response,
            } => {
                self.last_operation = Some("credential-set");
                if let Err(err) = self.ensure_live_connection("client-request") {
                    let _ = response.send(Err(err));
                    return;
                }
                let result = self.request("set", &name, &caller, Some(value)).map(|_| ());
                let failed = result.is_err();
                let _ = response.send(result);
                if failed {
                    self.recover_if_process_failed("credential-set");
                }
            }
        }
    }

''',
    "Vault worker run/handle",
)

text = replace_between(
    text,
    "    fn connection_is_dead(&mut self) -> bool {\n",
    "    fn ensure_connection(&mut self) -> anyhow::Result<()> {\n",
    r'''    fn ensure_live_connection(&mut self, phase: &'static str) -> anyhow::Result<()> {
        if let Some(report) = self.observe_connection_failure(phase) {
            return self.recover_unexpected(report);
        }
        if self.connection.is_none() {
            return self.retry_missing_connection();
        }
        Ok(())
    }

    fn recover_if_process_failed(&mut self, phase: &'static str) {
        if let Some(report) = self.observe_connection_failure(phase) {
            if let Err(error) = self.recover_unexpected(report) {
                tracing::error!(
                    error = %error,
                    attempt = self.unexpected_restart_attempts,
                    "Vault child failed during request and recovery did not become ready"
                );
            }
        }
    }

    fn observe_connection_failure(
        &mut self,
        phase: &'static str,
    ) -> Option<VaultProcessExitReport> {
        let connection = self.connection.as_mut()?;
        let pid = connection.pid;
        let uptime = connection.started_at.elapsed();
        let (exit_success, exit_code, exit_signal, observation_error) =
            match connection.child.try_wait() {
                Ok(None) => return None,
                Ok(Some(status)) => (
                    status.success(),
                    status.code(),
                    process_exit_signal(&status),
                    None,
                ),
                Err(error) => (
                    false,
                    None,
                    None,
                    Some(bounded_diagnostic(&error.to_string(), 1024)),
                ),
            };
        if uptime >= VAULT_STABLE_WINDOW {
            self.unexpected_restart_attempts = 0;
        }
        Some(VaultProcessExitReport {
            pid,
            exit_success,
            exit_code,
            exit_signal,
            previous_restart_attempts: self.unexpected_restart_attempts,
            uptime_ms: uptime.as_millis().min(u128::from(u64::MAX)) as u64,
            phase: phase.into(),
            last_operation: self.last_operation.map(str::to_string),
            observation_error,
        })
    }

    fn recover_unexpected(&mut self, report: VaultProcessExitReport) -> anyhow::Result<()> {
        let attempt = self.unexpected_restart_attempts.saturating_add(1);
        self.unexpected_restart_attempts = attempt;
        let local_delay = vault_restart_delay(attempt);
        let mut authority_minimum = Duration::ZERO;
        let mut authority_reason = None;
        if let Some(authority) = self.recovery_authority.as_ref() {
            match authority.decide(report.clone()) {
                Ok(advice) => {
                    authority_minimum = advice.minimum_backoff.min(RECOVERY_MAX_BACKOFF);
                    authority_reason = bounded_reason(advice.reason, 512);
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    pid = report.pid,
                    "CONTROL ER Vault decision failed; using local critical-process policy"
                ),
            }
        }
        let delay = recovery_backoff(attempt, authority_minimum);
        tracing::warn!(
            pid = report.pid,
            exit_code = ?report.exit_code,
            exit_signal = ?report.exit_signal,
            uptime_ms = report.uptime_ms,
            attempt,
            backoff_ms = delay.as_millis() as u64,
            authority_reason = authority_reason
                .as_deref()
                .unwrap_or("local critical-process policy"),
            "Vault child exited unexpectedly; scheduling recovery"
        );
        thread::sleep(delay.max(local_delay));
        self.restart_connection()
    }

    fn retry_missing_connection(&mut self) -> anyhow::Result<()> {
        let attempt = self.unexpected_restart_attempts.saturating_add(1);
        self.unexpected_restart_attempts = attempt;
        let delay = vault_restart_delay(attempt);
        tracing::warn!(
            attempt,
            backoff_ms = delay.as_millis() as u64,
            "Vault child remains unavailable after a failed recovery; retrying locally"
        );
        thread::sleep(delay);
        self.ensure_connection()
    }

''',
    "Vault connection failure recovery",
)

text = replace_once(
    text,
    '''    fn ensure_connection(&mut self) -> anyhow::Result<()> {\n        if self.connection.is_some() && !self.connection_is_dead() {\n            return Ok(());\n        }\n        self.shutdown_child();''',
    '''    fn ensure_connection(&mut self) -> anyhow::Result<()> {\n        if self.connection.is_some() {\n            return Ok(());\n        }''',
    "Vault ensure_connection entry",
)

text = replace_once(
    text,
    '''        let mut child = command\n            .stdin(Stdio::piped())\n            .stdout(Stdio::piped())\n            .stderr(Stdio::inherit())\n            .spawn()\n            .map_err(|e| anyhow::anyhow!(\"failed to spawn Vault process: {e}\"))?;\n        let stdin = child''',
    '''        let mut child = command\n            .stdin(Stdio::piped())\n            .stdout(Stdio::piped())\n            .stderr(Stdio::inherit())\n            .spawn()\n            .map_err(|e| anyhow::anyhow!(\"failed to spawn Vault process: {e}\"))?;\n        let pid = child.id();\n        let started_at = Instant::now();\n        let stdin = child''',
    "Vault child process metadata capture",
)
text = replace_once(
    text,
    '''        Ok(Connection {\n            child,\n            stdin,\n            reader,\n            token: ready.token,\n            seq: 1,\n        })''',
    '''        Ok(Connection {\n            child,\n            stdin,\n            reader,\n            token: ready.token,\n            seq: 1,\n            pid,\n            started_at,\n        })''',
    "Vault Connection construction",
)

# Add platform exit signal and bounded helper outside Worker.
worker_end = "}\n\npub fn run_vault_daemon(\n"
helpers = r'''}

fn vault_restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = RESTART_DELAY.as_millis().min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(millis.saturating_mul(factor)).min(RECOVERY_MAX_BACKOFF)
}

fn recovery_backoff(attempt: u32, authority_minimum: Duration) -> Duration {
    vault_restart_delay(attempt)
        .max(authority_minimum)
        .min(RECOVERY_MAX_BACKOFF)
}

fn bounded_reason(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.and_then(|value| {
        let mut out = String::new();
        for character in value.chars() {
            if character.is_control() || character == '\0' {
                continue;
            }
            if out.len().saturating_add(character.len_utf8()) > max_bytes {
                break;
            }
            out.push(character);
        }
        (!out.is_empty()).then_some(out)
    })
}

fn bounded_diagnostic(value: &str, max_bytes: usize) -> String {
    bounded_reason(Some(value.to_string()), max_bytes).unwrap_or_else(|| "unavailable".into())
}

#[cfg(unix)]
fn process_exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn process_exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

pub fn run_vault_daemon(
'''
text = replace_once(text, worker_end, helpers, "Vault recovery helper insertion")

# Append unit tests at file end, after all production items.
tests = r'''

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn vault_recovery_backoff_is_exponential_capped_and_never_shortened() {
        assert_eq!(vault_restart_delay(1), Duration::from_millis(200));
        assert_eq!(vault_restart_delay(2), Duration::from_millis(400));
        assert_eq!(vault_restart_delay(3), Duration::from_millis(800));
        assert_eq!(vault_restart_delay(30), RECOVERY_MAX_BACKOFF);
        assert_eq!(
            recovery_backoff(1, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            recovery_backoff(6, Duration::from_millis(1)),
            vault_restart_delay(6)
        );
        assert_eq!(
            recovery_backoff(1, Duration::from_secs(300)),
            RECOVERY_MAX_BACKOFF
        );
    }

    #[test]
    fn recovery_reason_is_bounded_and_strips_control_characters() {
        let value = bounded_reason(Some("why\nthis\0failed".into()), 8).unwrap();
        assert!(!value.contains('\n'));
        assert!(!value.contains('\0'));
        assert!(value.len() <= 8);
    }
}
'''
if "mod recovery_tests" not in text:
    text = text.rstrip() + tests + "\n"
write(path, text)


# ---------------------------------------------------------------------------
# Backend adapter: translate the neutral Vault child report into the existing
# bounded ProcessExitReport and synchronously bridge to async CONTROL ER from
# the dedicated vault-process-io thread. No secret data crosses this boundary.
# ---------------------------------------------------------------------------
write(
    "engine/crates/backend/src/vault_recovery.rs",
    r'''use std::collections::BTreeMap;
use std::time::Duration;

use service_runtime::ServiceRestartDirective;

pub struct VaultErRecoveryAuthority {
    client: crate::er_recovery::ErRecoveryClient,
    runtime: tokio::runtime::Handle,
}

impl VaultErRecoveryAuthority {
    pub fn new(key: crate::host_bootstrap::ErControlKey) -> Self {
        Self {
            client: crate::er_recovery::ErRecoveryClient::new(key),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl vault_process::VaultRecoveryAuthority for VaultErRecoveryAuthority {
    fn decide(
        &self,
        report: vault_process::VaultProcessExitReport,
    ) -> anyhow::Result<vault_process::VaultRecoveryAdvice> {
        let mut context = BTreeMap::new();
        context.insert("supervision_scope".into(), "credential-runtime".into());
        context.insert("transport".into(), "stdio-json".into());
        context.insert("recovery_owner".into(), "vault-process-io".into());

        let report = crate::er_recovery::ProcessExitReport {
            component: "vault-runtime".into(),
            process_image: if cfg!(windows) {
                "backend.exe".into()
            } else {
                "backend".into()
            },
            pid: report.pid,
            exit_success: report.exit_success,
            exit_code: report.exit_code,
            exit_signal: report.exit_signal,
            previous_restart_attempts: report.previous_restart_attempts,
            uptime_ms: report.uptime_ms,
            expected: false,
            phase: report.phase,
            last_operation: report.last_operation,
            observation_error: report.observation_error,
            context,
        };

        let decision = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(700), self.client.decide_process(report))
                .await
        });
        match decision {
            Ok(Ok(ServiceRestartDirective::Restart {
                minimum_backoff_ms,
                reason,
            })) => Ok(vault_process::VaultRecoveryAdvice {
                minimum_backoff: Duration::from_millis(minimum_backoff_ms),
                reason: Some(reason),
            }),
            Ok(Ok(ServiceRestartDirective::Default)) => Ok(vault_process::VaultRecoveryAdvice::default()),
            Ok(Ok(ServiceRestartDirective::Stop { reason })) => {
                tracing::error!(
                    authority_reason = %reason,
                    "CONTROL ER requested Stop for unexpected critical Vault exit; ignoring unsafe stop directive"
                );
                Ok(vault_process::VaultRecoveryAdvice {
                    minimum_backoff: Duration::ZERO,
                    reason: Some(format!("unsafe CONTROL ER stop ignored: {reason}")),
                })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => anyhow::bail!("CONTROL ER Vault recovery decision timed out"),
        }
    }
}
''',
)

path = "engine/crates/backend/src/main.rs"
text = read(path)
text = replace_once(
    text,
    "mod service_mother;\n",
    "mod service_mother;\nmod vault_recovery;\n",
    "backend vault_recovery module",
)
text = replace_once(
    text,
    '''    let vault_instance = match vault_process::VaultClient::spawn("backend-rs", &admin_dir) {''',
    '''    let vault_recovery_authority: Option<Arc<dyn vault_process::VaultRecoveryAuthority>> =\n        er_control_key.as_ref().map(|key| {\n            Arc::new(vault_recovery::VaultErRecoveryAuthority::new(key.clone()))\n                as Arc<dyn vault_process::VaultRecoveryAuthority>\n        });\n    let vault_instance = match vault_process::VaultClient::spawn_with_recovery(\n        "backend-rs",\n        &admin_dir,\n        vault_recovery_authority,\n    ) {''',
    "backend VaultClient recovery wiring",
)
write(path, text)


# ---------------------------------------------------------------------------
# Document exactly what CONTROL ER sees, and preserve planned refresh semantics.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
append = r'''

### CONTROL ER Vault child recovery

The separate Vault child is also connected to the critical-process recovery
boundary without making `vault-process` depend on backend/ER. `vault-process`
defines a neutral bounded recovery-authority interface; backend adapts that
interface to the same per-backend-generation CONTROL ER capability from a
separate `vault-process-io` thread.

When the Vault child exits unexpectedly, the report contains only its PID,
portable exit code or Unix signal, uptime, prior recovery attempts, observation
phase, and a fixed non-sensitive operation class such as `credential-get` or
`credential-set`. Backend adds only fixed runtime context (`credential-runtime`,
`stdio-json`, and `vault-process-io`). Credential names, callers, credential
values, session tokens, Secret Service data, Vault ACL contents, Runtime ENV
values, and protocol request/response bodies are never sent to ER.

Vault keeps final recovery ownership. Local recovery uses exponential backoff
from 200 ms to a 30-second cap. CONTROL ER may raise the minimum delay but cannot
exceed that cap or permanently stop the critical Vault child; an unavailable,
BASIC, failed, or timed-out ER decision falls back to the local Vault recovery
path. A scheduled `refresh_process()` remains an intentional restart and resets
unexpected-crash attempts rather than being classified as a crash.
'''
if "### CONTROL ER Vault child recovery" not in text:
    text = text.rstrip() + append + "\n"
write(path, text)
