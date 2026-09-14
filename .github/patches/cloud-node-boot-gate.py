from pathlib import Path


def patch(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: {path}: expected one anchor, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))
    print(f"patched: {label}")


cfg = "engine/crates/cloud-node/src/config.rs"
patch(
    cfg,
    '''#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSettings {
    #[serde(default)]
    pub targets: Vec<ReplicationTarget>,
}
''',
    '''#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSettings {
    #[serde(default)]
    pub require_boot_recovery: bool,
    #[serde(default = "default_boot_recovery_timeout_ms")]
    pub boot_recovery_timeout_ms: u64,
    #[serde(default)]
    pub targets: Vec<ReplicationTarget>,
}

impl Default for ReplicationSettings {
    fn default() -> Self {
        Self {
            require_boot_recovery: false,
            boot_recovery_timeout_ms: default_boot_recovery_timeout_ms(),
            targets: Vec::new(),
        }
    }
}
''',
    "replication boot policy fields",
)
patch(
    cfg,
    '''        for target in &self.replication.targets {
''',
    '''        if !(1_000..=3_600_000).contains(&self.replication.boot_recovery_timeout_ms) {
            anyhow::bail!("Cloud Node bootRecoveryTimeoutMs must be between 1000 and 3600000");
        }
        if self.replication.require_boot_recovery && self.replication.targets.is_empty() {
            anyhow::bail!(
                "Cloud Node requireBootRecovery needs at least one trusted replication target"
            );
        }
        for target in &self.replication.targets {
''',
    "replication boot policy validation",
)
patch(
    cfg,
    '''const fn default_reconnect_delay_ms() -> u64 {
    2_000
}
''',
    '''const fn default_reconnect_delay_ms() -> u64 {
    2_000
}
const fn default_boot_recovery_timeout_ms() -> u64 {
    60_000
}
''',
    "boot timeout default",
)
patch(
    cfg,
    '''        assert!(settings.node.preserve_original);
        settings.validate().unwrap();
''',
    '''        assert!(settings.node.preserve_original);
        assert!(!settings.replication.require_boot_recovery);
        assert_eq!(settings.replication.boot_recovery_timeout_ms, 60_000);
        settings.validate().unwrap();
''',
    "boot policy default test",
)

m = "engine/crates/backend/src/maintenance_notice.rs"
patch(m, "use std::io::Read as _;\n", "use std::io::{Read as _, Write as _};\n", "stdout write import")
patch(
    m,
    "use std::sync::{Arc, Mutex};\n",
    "use std::sync::atomic::{AtomicBool, Ordering};\nuse std::sync::{Arc, Mutex};\n",
    "recovery completion atomic import",
)
patch(
    m,
    '''use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, Command};
''',
    '''use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
''',
    "parent control pipe imports",
)
patch(
    m,
    "const MAX_TRANSFER_REQUEST_BYTES: usize = MAX_FRAME_BYTES + 32;\n",
    '''const MAX_TRANSFER_REQUEST_BYTES: usize = MAX_FRAME_BYTES + 32;
const CONTROL_PREFIX: &str = "RBE-CN-CONTROL/1";
const CONTROL_COMPLETE_PREFIX: &str = "RBE-CN-CONTROL/1 complete ";
const CONTROL_POLICY_TIMEOUT: Duration = Duration::from_secs(5);
''',
    "parent control constants",
)
patch(
    m,
    "pub struct MaintenanceNoticeProcess {\n",
    '''#[derive(Debug, Clone, Copy)]
enum BootRecoveryAdmission {
    Disabled,
    Optional,
    Required { timeout: Duration },
}

pub struct MaintenanceNoticeProcess {
''',
    "boot admission enum",
)
patch(
    m,
    '''    lease: Option<ChildStdin>,
    host: String,
''',
    '''    lease: Option<ChildStdin>,
    control: BufReader<ChildStdout>,
    recovery_admission: BootRecoveryAdmission,
    host: String,
''',
    "parent control fields",
)
patch(
    m,
    '''    recoveries: Mutex<HashMap<[u8; 16], RecoveryState>>,
}

#[derive(Clone)]
''',
    '''    recoveries: Mutex<HashMap<[u8; 16], RecoveryState>>,
    recovery_admission: BootRecoveryAdmission,
    recovery_complete: AtomicBool,
}

#[derive(Clone)]
''',
    "helper recovery policy state",
)
patch(
    m,
    '''            .stdin(Stdio::piped())
            .kill_on_drop(true)
''',
    '''            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
''',
    "pipe helper stdout",
)
patch(
    m,
    '''        let lease = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdin lifetime pipe was not created")
        })?;

        let mut process = Self {
            child,
            lease: Some(lease),
''',
    '''        let lease = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdin lifetime pipe was not created")
        })?;
        let control = child.stdout.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdout control pipe was not created")
        })?;

        let mut process = Self {
            child,
            lease: Some(lease),
            control: BufReader::new(control),
            recovery_admission: BootRecoveryAdmission::Disabled,
''',
    "capture helper stdout",
)
patch(
    m,
    '''        };
        process.wait_until_ready().await?;
''',
    '''        };
        process.recovery_admission = process.read_admission_policy().await?;
        process.wait_until_ready().await?;
''',
    "read helper boot policy",
)
patch(
    m,
    "    /// Close the lifetime pipe first so the helper can release the listener\n",
    '''    pub async fn wait_for_required_cloud_node_recovery(&mut self) -> anyhow::Result<()> {
        let timeout = match self.recovery_admission {
            BootRecoveryAdmission::Disabled | BootRecoveryAdmission::Optional => return Ok(()),
            BootRecoveryAdmission::Required { timeout } => timeout,
        };
        tracing::info!(
            timeout_ms = timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            "waiting for required Cloud Node boot recovery before runtime admission"
        );
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("required Cloud Node boot recovery timed out");
            }
            let line = self.read_control_line(remaining).await?;
            if let Some(root) = line.strip_prefix(CONTROL_COMPLETE_PREFIX) {
                if root.len() != 64 || !root.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    anyhow::bail!(
                        "maintenance responder returned an invalid Cloud Node recovery root"
                    );
                }
                tracing::info!(root = %root, "required Cloud Node boot recovery completed");
                return Ok(());
            }
            anyhow::bail!("maintenance responder returned unexpected control message {line:?}");
        }
    }

    async fn read_admission_policy(&mut self) -> anyhow::Result<BootRecoveryAdmission> {
        let line = self.read_control_line(CONTROL_POLICY_TIMEOUT).await?;
        if line == format!("{CONTROL_PREFIX} disabled") {
            return Ok(BootRecoveryAdmission::Disabled);
        }
        if line == format!("{CONTROL_PREFIX} optional") {
            return Ok(BootRecoveryAdmission::Optional);
        }
        if let Some(value) = line.strip_prefix(&format!("{CONTROL_PREFIX} required ")) {
            let timeout_ms = value.parse::<u64>().map_err(|_| {
                anyhow::anyhow!("maintenance responder returned an invalid recovery timeout")
            })?;
            return Ok(BootRecoveryAdmission::Required {
                timeout: Duration::from_millis(timeout_ms),
            });
        }
        anyhow::bail!("maintenance responder returned invalid control policy {line:?}")
    }

    async fn read_control_line(&mut self, timeout: Duration) -> anyhow::Result<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.control.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow::anyhow!("timed out waiting for maintenance responder control message")
            })??;
        if read == 0 {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "maintenance responder exited while waiting for Cloud Node control message: {status}"
                );
            }
            anyhow::bail!("maintenance responder closed its Cloud Node control pipe");
        }
        Ok(line.trim().to_owned())
    }

    /// Close the lifetime pipe first so the helper can release the listener
''',
    "parent boot recovery wait",
)
patch(
    m,
    '''pub async fn run(host: String, port: u16) -> anyhow::Result<()> {
    let cloud_node = load_cloud_node_runtime()?;
''',
    '''pub async fn run(host: String, port: u16) -> anyhow::Result<()> {
    let cloud_node = load_cloud_node_runtime()?;
    emit_boot_recovery_policy(cloud_node.as_deref())?;
''',
    "emit helper boot policy",
)
patch(
    m,
    "    if remote_header != local_header {\n",
    '''    if remote_header == local_header {
        if let Err(error) = signal_boot_recovery_complete(runtime, local_header) {
            tracing::error!(error = %error, "Cloud Node could not signal completed boot recovery");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }

    if remote_header != local_header {
''',
    "signal already-matched root",
)
patch(
    m,
    '''            if let Ok(mut recoveries) = runtime.recoveries.lock() {
                recoveries.remove(&session.session);
            }
''',
    '''            if let Err(error) = signal_boot_recovery_complete(runtime, actual) {
                tracing::error!(error = %error, "Cloud Node could not signal activated boot recovery");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if let Ok(mut recoveries) = runtime.recoveries.lock() {
                recoveries.remove(&session.session);
            }
''',
    "signal activated root",
)
patch(
    m,
    "fn encode_cloud_node_response(frame: Frame, operation: &str) -> Response {\n",
    '''fn emit_boot_recovery_policy(runtime: Option<&CloudNodeRuntime>) -> anyhow::Result<()> {
    let line = match runtime.map(|runtime| runtime.recovery_admission) {
        None | Some(BootRecoveryAdmission::Disabled) => format!("{CONTROL_PREFIX} disabled"),
        Some(BootRecoveryAdmission::Optional) => format!("{CONTROL_PREFIX} optional"),
        Some(BootRecoveryAdmission::Required { timeout }) => format!(
            "{CONTROL_PREFIX} required {}",
            timeout.as_millis().min(u128::from(u64::MAX)) as u64
        ),
    };
    write_control_line(&line)
}

fn signal_boot_recovery_complete(
    runtime: &CloudNodeRuntime,
    header: SyncPlanHeader,
) -> anyhow::Result<()> {
    if !matches!(
        runtime.recovery_admission,
        BootRecoveryAdmission::Required { .. }
    ) {
        return Ok(());
    }
    if runtime
        .recovery_complete
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    let result = write_control_line(&format!(
        "{CONTROL_COMPLETE_PREFIX}{}",
        hex::encode(header.root_sha256)
    ));
    if result.is_err() {
        runtime.recovery_complete.store(false, Ordering::Release);
    }
    result
}

fn write_control_line(line: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn encode_cloud_node_response(frame: Frame, operation: &str) -> Response {
''',
    "helper control protocol",
)
patch(
    m,
    "    let authenticator = CloudNodeAuthenticator::from_env(&settings)?;\n",
    '''    let recovery_admission = if settings.replication.require_boot_recovery {
        BootRecoveryAdmission::Required {
            timeout: Duration::from_millis(settings.replication.boot_recovery_timeout_ms),
        }
    } else {
        BootRecoveryAdmission::Optional
    };
    let authenticator = CloudNodeAuthenticator::from_env(&settings)?;
''',
    "helper admission policy",
)
patch(
    m,
    '''        recoveries: Mutex::new(HashMap::new()),
    })))
''',
    '''        recoveries: Mutex::new(HashMap::new()),
        recovery_admission,
        recovery_complete: AtomicBool::new(false),
    })))
''',
    "helper admission state init",
)

main = "engine/crates/backend/src/main.rs"
patch(main, "    let maintenance_notice =\n", "    let mut maintenance_notice =\n", "mutable maintenance helper")
patch(
    main,
    '''    boot_trace("temporary API maintenance responder ready");
    lifecycle.set(BackendState::ServicesStarting);
''',
    '''    boot_trace("temporary API maintenance responder ready");
    maintenance_notice
        .wait_for_required_cloud_node_recovery()
        .await?;
    boot_trace("Cloud Node boot recovery admission satisfied");
    lifecycle.set(BackendState::ServicesStarting);
''',
    "runtime admission gate",
)
