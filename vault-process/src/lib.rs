use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

const STARTUP_TARGET: Duration = Duration::from_secs(2);
const STARTUP_MAX: Duration = Duration::from_secs(300);
const MONITOR_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_DELAY: Duration = Duration::from_millis(200);
const RECOVERY_MAX_BACKOFF: Duration = Duration::from_secs(30);
const VAULT_STABLE_WINDOW: Duration = Duration::from_secs(60);
const MAX_RESTARTS: u32 = 5;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    seq: u64,
    op: String,
    name: String,
    caller: String,
    value: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    kind: String,
    seq: u64,
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ready {
    kind: String,
    token: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct NeedsDbus {
    kind: String,
}

enum ClientCommand {
    Get {
        name: String,
        caller: String,
        response: Sender<anyhow::Result<String>>,
    },
    Set {
        name: String,
        value: String,
        caller: String,
        response: Sender<anyhow::Result<()>>,
    },
    Refresh {
        response: Sender<anyhow::Result<()>>,
    },
}

#[derive(Debug, Clone)]
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

#[derive(Clone)]
pub struct VaultClient {
    tx: Sender<ClientCommand>,
}

impl VaultClient {
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

    pub fn credential(&self, name: &str, caller: &str) -> anyhow::Result<secrecy::SecretString> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(ClientCommand::Get {
                name: name.to_owned(),
                caller: caller.to_owned(),
                response: tx,
            })
            .map_err(|_| anyhow::anyhow!("Vault client worker is unavailable"))?;
        Ok(secrecy::SecretString::new(rx.recv().map_err(|_| {
            anyhow::anyhow!("Vault client worker stopped responding")
        })??))
    }

    pub fn set_credential(&self, name: &str, value: &str, caller: &str) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(ClientCommand::Set {
                name: name.to_owned(),
                value: value.to_owned(),
                caller: caller.to_owned(),
                response: tx,
            })
            .map_err(|_| anyhow::anyhow!("Vault client worker is unavailable"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("Vault client worker stopped responding"))?
    }

    pub fn refresh_process(&self) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(ClientCommand::Refresh { response: tx })
            .map_err(|_| anyhow::anyhow!("Vault client worker is unavailable"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("Vault client worker stopped responding during refresh"))?
    }
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    token: String,
    seq: u64,
    pid: u32,
    started_at: Instant,
}

struct Worker {
    exe: PathBuf,
    service_name: String,
    data_dir: PathBuf,
    ready_tx: Option<SyncSender<anyhow::Result<()>>>,
    connection: Option<Connection>,
    recovery_authority: Option<Arc<dyn VaultRecoveryAuthority>>,
    unexpected_restart_attempts: u32,
    last_operation: Option<&'static str>,
}

impl Worker {
    fn new(
        exe: PathBuf,
        service_name: String,
        data_dir: PathBuf,
        ready_tx: SyncSender<anyhow::Result<()>>,
        recovery_authority: Option<Arc<dyn VaultRecoveryAuthority>>,
    ) -> Self {
        Self {
            exe,
            service_name,
            data_dir,
            ready_tx: Some(ready_tx),
            connection: None,
            recovery_authority,
            unexpected_restart_attempts: 0,
            last_operation: None,
        }
    }

    fn run(&mut self, rx: Receiver<ClientCommand>) {
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

    fn request(
        &mut self,
        op: &str,
        name: &str,
        caller: &str,
        value: Option<String>,
    ) -> anyhow::Result<String> {
        let c = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Vault connection is unavailable"))?;
        let seq = c.seq;
        c.seq = c.seq.saturating_add(1);
        let request = Request {
            token: c.token.clone(),
            seq,
            op: op.into(),
            name: name.into(),
            caller: caller.into(),
            value,
        };
        writeln!(c.stdin, "{}", serde_json::to_string(&request)?)?;
        c.stdin.flush()?;
        let mut line = String::new();
        c.reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(anyhow::anyhow!("Vault process closed the protocol pipe"));
        }
        let response: Response = serde_json::from_str(&line)?;
        if response.seq != seq {
            return Err(anyhow::anyhow!("Vault protocol sequence mismatch"));
        }
        if !response.ok {
            return Err(anyhow::anyhow!(
                "Vault request failed: {}",
                response.error.unwrap_or_else(|| "unknown error".into())
            ));
        }
        Ok(response.value.unwrap_or_default())
    }

    fn ensure_live_connection(&mut self, phase: &'static str) -> anyhow::Result<()> {
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

    fn ensure_connection(&mut self) -> anyhow::Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let mut last_error = None;
        for _ in 0..MAX_RESTARTS {
            match self.spawn_connection(false) {
                Ok(connection) => {
                    self.connection = Some(connection);
                    return Ok(());
                }
                Err(err) if err.to_string() == "VAULT_NEEDS_DBUS" => {
                    match self.spawn_connection(true) {
                        Ok(connection) => {
                            self.connection = Some(connection);
                            return Ok(());
                        }
                        Err(err) => last_error = Some(err),
                    }
                }
                Err(err) => last_error = Some(err),
            }
            thread::sleep(RESTART_DELAY);
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Vault failed to start")))
    }

    fn restart_connection(&mut self) -> anyhow::Result<()> {
        self.shutdown_child();
        self.ensure_connection()
    }

    fn spawn_connection(&self, force_dbus: bool) -> anyhow::Result<Connection> {
        let mut command = Command::new(&self.exe);
        command
            .args([
                "--vault",
                "--separate-process",
                "--service-name",
                &self.service_name,
                "--data-dir",
            ])
            .arg(&self.data_dir);
        if force_dbus {
            command.arg("--dbus");
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn Vault process: {e}"))?;
        let pid = child.id();
        let started_at = Instant::now();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Vault stdin pipe unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Vault stdout pipe unavailable"))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("Vault exited before handshake"));
        }
        if serde_json::from_str::<NeedsDbus>(&line)
            .map(|v| v.kind == "needs_dbus")
            .unwrap_or(false)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("VAULT_NEEDS_DBUS"));
        }
        let ready: Ready = serde_json::from_str(&line)
            .map_err(|e| anyhow::anyhow!("invalid Vault handshake: {e}"))?;
        if ready.kind != "ready" || ready.token.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("Vault returned an invalid ready handshake"));
        }
        Ok(Connection {
            child,
            stdin,
            reader,
            token: ready.token,
            seq: 1,
            pid,
            started_at,
        })
    }

    fn shutdown_child(&mut self) {
        if let Some(mut c) = self.connection.take() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

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
    service_name: String,
    data_dir: PathBuf,
    force_dbus: bool,
) -> anyhow::Result<()> {
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &data_dir);
    error_client::install_panic_hook();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .try_init();

    #[cfg(target_os = "linux")]
    if !force_dbus
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
            .map(|v| v.is_empty())
            .unwrap_or(true)
    {
        let message = serde_json::to_string(&NeedsDbus {
            kind: "needs_dbus".into(),
        })?;
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        writeln!(writer, "{message}")?;
        writer.flush()?;
        return Ok(());
    }

    let vault = vault::Vault::new(io, service_name, &data_dir)?;
    let token = generate_session_token();
    let ready = serde_json::to_string(&Ready {
        kind: "ready".into(),
        token: token.clone(),
    })?;
    {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        writeln!(writer, "{ready}")?;
        writer.flush()?;
    }

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();
    let mut expected_seq = 1u64;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(error) => {
                let response = Response {
                    kind: "response".into(),
                    seq: expected_seq,
                    ok: false,
                    value: None,
                    error: Some(format!("invalid request: {error}")),
                };
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        };

        let response = if request.token != token {
            Response {
                kind: "response".into(),
                seq: request.seq,
                ok: false,
                value: None,
                error: Some("invalid Vault session token".into()),
            }
        } else if request.seq != expected_seq {
            Response {
                kind: "response".into(),
                seq: request.seq,
                ok: false,
                value: None,
                error: Some("invalid Vault request sequence".into()),
            }
        } else {
            expected_seq = expected_seq.saturating_add(1);
            match request.op.as_str() {
                "get" => match vault.credential(&request.name, &request.caller) {
                    Ok(value) => Response {
                        kind: "response".into(),
                        seq: request.seq,
                        ok: true,
                        value: Some(value.expose_secret().to_owned()),
                        error: None,
                    },
                    Err(error) => Response {
                        kind: "response".into(),
                        seq: request.seq,
                        ok: false,
                        value: None,
                        error: Some(format!("{error:#}")),
                    },
                },
                "set" => match request.value.as_deref() {
                    Some(value) => {
                        match vault.set_credential(&request.name, value, &request.caller) {
                            Ok(()) => Response {
                                kind: "response".into(),
                                seq: request.seq,
                                ok: true,
                                value: None,
                                error: None,
                            },
                            Err(error) => Response {
                                kind: "response".into(),
                                seq: request.seq,
                                ok: false,
                                value: None,
                                error: Some(format!("{error:#}")),
                            },
                        }
                    }
                    None => Response {
                        kind: "response".into(),
                        seq: request.seq,
                        ok: false,
                        value: None,
                        error: Some("set request is missing value".into()),
                    },
                },
                _ => Response {
                    kind: "response".into(),
                    seq: request.seq,
                    ok: false,
                    value: None,
                    error: Some(format!("unknown Vault operation {:?}", request.op)),
                },
            }
        };
        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
        writer.flush()?;
    }
    Ok(())
}

fn generate_session_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

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
