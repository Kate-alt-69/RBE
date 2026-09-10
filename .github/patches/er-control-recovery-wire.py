from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# HostBootstrap owns issuance of the per-backend-generation ER CONTROL key.
# The key exists only in RAM and can be reconstructed by trusted child modes
# only from their inherited one-shot bootstrap pipe.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/host_bootstrap.rs"
text = read(path)
text = text.replace(
    "use std::process::Stdio;\n",
    "use std::process::Stdio;\nuse std::sync::Arc;\n",
    1,
)
old = '''impl HostBootstrapReady {\n    pub fn er_control_enabled(self) -> bool {\n        self.secure_credentials\n    }\n}\n'''
new = '''impl HostBootstrapReady {\n    pub fn er_control_enabled(self) -> bool {\n        self.secure_credentials\n    }\n\n    pub fn issue_er_control_key(self) -> Option<ErControlKey> {\n        self.secure_credentials.then(ErControlKey::random)\n    }\n}\n\nstruct ErControlKeyMaterial([u8; 32]);\n\nimpl Drop for ErControlKeyMaterial {\n    fn drop(&mut self) {\n        self.0.fill(0);\n    }\n}\n\n#[derive(Clone)]\npub struct ErControlKey(Arc<ErControlKeyMaterial>);\n\nimpl ErControlKey {\n    fn random() -> Self {\n        let mut bytes = [0u8; 32];\n        rand::thread_rng().fill_bytes(&mut bytes);\n        Self(Arc::new(ErControlKeyMaterial(bytes)))\n    }\n\n    pub(crate) fn from_inherited_hex(value: &str) -> anyhow::Result<Self> {\n        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {\n            anyhow::bail!(\"ER CONTROL key must be a 32-byte hexadecimal value\");\n        }\n        let decoded = hex::decode(value)?;\n        let bytes: [u8; 32] = decoded\n            .try_into()\n            .map_err(|_| anyhow::anyhow!(\"ER CONTROL key decoded to the wrong length\"))?;\n        Ok(Self(Arc::new(ErControlKeyMaterial(bytes))))\n    }\n\n    pub(crate) fn to_hex(&self) -> String {\n        hex::encode(self.as_bytes())\n    }\n\n    pub(crate) fn as_bytes(&self) -> &[u8; 32] {\n        &self.0.as_ref().0\n    }\n}\n'''
if old not in text:
    raise SystemExit("missing HostBootstrapReady impl anchor")
text = text.replace(old, new, 1)
old = '''    let start = value.len().saturating_sub(MAX);\n    format!(\"[...truncated...]\\n{}\", &value[start..])\n'''
new = '''    let mut start = value.len().saturating_sub(MAX);\n    while start < value.len() && !value.is_char_boundary(start) {\n        start = start.saturating_add(1);\n    }\n    format!(\"[...truncated...]\\n{}\", &value[start..])\n'''
if old not in text:
    raise SystemExit("missing UTF-8 debug truncation anchor")
text = text.replace(old, new, 1)
write(path, text)

# Avoid leaving a freshly-forked session bus behind when no provider exists or
# when the provider cannot claim Secret Service. The successful bus remains the
# RBE process tree's session bus and is inherited by trusted children.
write(
    "engine/crates/backend/scripts/bootstrap/linux/probe-secret-service.sh",
    r'''#!/bin/sh
set -u

has_secret_service() {
    if command -v gdbus >/dev/null 2>&1; then
        gdbus call --session \
            --dest org.freedesktop.secrets \
            --object-path /org/freedesktop/secrets \
            --method org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1 && return 0
    fi
    if command -v dbus-send >/dev/null 2>&1; then
        dbus-send --session --dest=org.freedesktop.DBus --type=method_call --print-reply \
            /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>/dev/null \
            | grep -q 'org.freedesktop.secrets' && return 0
    fi
    return 1
}

if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && has_secret_service; then
    echo 'RBE_RESULT=READY'
    exit 0
fi

# Do not create a private session bus merely to discover that there is no
# provider to attach to it. Provision first, then make the bus.
if ! command -v gnome-keyring-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=secret-service-provider'
    exit 10
fi
if ! command -v dbus-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=dbus-daemon'
    exit 10
fi

started_bus_pid=''
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    bus_output="$(dbus-daemon --session --fork --print-address=1 --print-pid=1 2>/dev/null || true)"
    address="$(printf '%s\n' "$bus_output" | sed -n '1p')"
    started_bus_pid="$(printf '%s\n' "$bus_output" | sed -n '2p')"
    if [ -z "$address" ]; then
        echo 'RBE_RESULT=FAILED'
        echo 'RBE_STAGE=session-dbus'
        exit 11
    fi
    DBUS_SESSION_BUS_ADDRESS="$address"
    export DBUS_SESSION_BUS_ADDRESS
    printf 'RBE_EXPORT_DBUS_SESSION_BUS_ADDRESS=%s\n' "$DBUS_SESSION_BUS_ADDRESS"
fi

if has_secret_service; then
    echo 'RBE_RESULT=READY'
    exit 0
fi

keyring_output="$(gnome-keyring-daemon --start --components=secrets 2>/dev/null || true)"
printf '%s\n' "$keyring_output" | while IFS= read -r line; do
    case "$line" in
        GNOME_KEYRING_CONTROL=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
        GNOME_KEYRING_PID=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
    esac
done

i=0
while [ "$i" -lt 20 ]; do
    if has_secret_service; then
        echo 'RBE_RESULT=READY'
        exit 0
    fi
    i=$((i + 1))
    sleep 0.05
 done

# The bus was created solely for this failed bootstrap attempt. Do not strand it.
case "$started_bus_pid" in
    ''|*[!0-9]*) ;;
    *) kill "$started_bus_pid" >/dev/null 2>&1 || true ;;
esac

echo 'RBE_RESULT=FAILED'
echo 'RBE_STAGE=secret-service-start'
exit 11
''',
)

# ---------------------------------------------------------------------------
# Generic service-runtime recovery authority contract. service-runtime never
# depends on backend/ER; Mother may optionally install any authority provider.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace(
    "use std::collections::HashMap;\n",
    "use std::collections::HashMap;\nuse std::future::Future;\n",
    1,
)
text = text.replace(
    "use std::process::Stdio;\n",
    "use std::pin::Pin;\nuse std::process::{ExitStatus, Stdio};\n",
    1,
)
old = '''impl ServiceOperation {\n    fn into_request(self, token: String) -> ServiceRequest {\n'''
new = '''impl ServiceOperation {\n    fn diagnostic_label(&self) -> String {\n        match self {\n            Self::Call { function, .. } => {\n                let function = function\n                    .chars()\n                    .filter(|character| {\n                        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')\n                    })\n                    .take(96)\n                    .collect::<String>();\n                if function.is_empty() {\n                    \"call\".into()\n                } else {\n                    format!(\"call:{function}\")\n                }\n            }\n            Self::Event { .. } => \"event\".into(),\n        }\n    }\n\n    fn into_request(self, token: String) -> ServiceRequest {\n'''
if old not in text:
    raise SystemExit("missing ServiceOperation impl anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''    restarting: bool,\n    active_calls: Arc<AtomicU32>,\n    last_activity: Instant,\n''',
    '''    restarting: bool,\n    active_calls: Arc<AtomicU32>,\n    last_activity: Instant,\n    last_operation: Option<String>,\n''',
    1,
)
text = text.replace(
    '''            active_calls: Arc::new(AtomicU32::new(0)),\n            last_activity: Instant::now(),\n''',
    '''            active_calls: Arc::new(AtomicU32::new(0)),\n            last_activity: Instant::now(),\n            last_operation: None,\n''',
    1,
)
insert = '''#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(rename_all = "camelCase")]\npub struct ServiceExitReport {\n    pub service: String,\n    pub title: String,\n    pub service_file: String,\n    pub pid: u32,\n    pub exit_success: bool,\n    pub exit_code: Option<i32>,\n    pub exit_signal: Option<i32>,\n    pub restart: RestartPolicy,\n    pub mode: ServiceMode,\n    pub previous_restart_attempts: u32,\n    pub uptime_ms: u64,\n    pub active_calls: u32,\n    pub idle_for_ms: u64,\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub last_operation: Option<String>,\n    pub expected: bool,\n    pub phase: String,\n}\n\n#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(tag = "decision", rename_all = "snake_case")]\npub enum ServiceRestartDirective {\n    Default,\n    Restart {\n        minimum_backoff_ms: u64,\n        reason: String,\n    },\n    Stop {\n        reason: String,\n    },\n}\n\npub type ServiceRestartAuthorityFuture<'a> = Pin<\n    Box<dyn Future<Output = anyhow::Result<ServiceRestartDirective>> + Send + 'a>,\n>;\n\npub trait ServiceRestartAuthority: Send + Sync {\n    fn decide<'a>(&'a self, report: ServiceExitReport) -> ServiceRestartAuthorityFuture<'a>;\n}\n\n'''
anchor = "#[derive(Clone, Default)]\npub struct ServiceManager {\n"
if anchor not in text:
    raise SystemExit("missing ServiceManager anchor")
text = text.replace(anchor, insert + anchor, 1)
text = text.replace(
    '''    fabric: Option<ServiceFabricEndpoint>,\n    runtime_env: Option<Arc<Value>>,\n''',
    '''    fabric: Option<ServiceFabricEndpoint>,\n    runtime_env: Option<Arc<Value>>,\n    restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,\n''',
    1,
)
text = text.replace(
    "        let manager = Self::prepare_all(catalog, None, None).await;",
    "        let manager = Self::prepare_all(catalog, None, None, None).await;",
    1,
)
text = text.replace(
    "        Self::prepare_all(catalog, Some(fabric), None).await",
    "        Self::prepare_all(catalog, Some(fabric), None, None).await",
    1,
)
text = text.replace(
    "        Self::prepare_all(catalog, Some(fabric), Some(runtime_env)).await",
    "        Self::prepare_all(catalog, Some(fabric), Some(runtime_env), None).await",
    1,
)
old = '''    async fn prepare_all(\n        catalog: &ServiceCatalog,\n        fabric: Option<ServiceFabricEndpoint>,\n        runtime_env: Option<Arc<Value>>,\n    ) -> Self {\n        let manager = Self {\n            fabric,\n            runtime_env,\n            ..Self::default()\n        };\n'''
new = '''    pub async fn prepare_all_with_fabric_runtime_env_and_restart_authority(\n        catalog: &ServiceCatalog,\n        fabric: ServiceFabricEndpoint,\n        runtime_env: Arc<Value>,\n        restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,\n    ) -> Self {\n        Self::prepare_all(\n            catalog,\n            Some(fabric),\n            Some(runtime_env),\n            restart_authority,\n        )\n        .await\n    }\n\n    async fn prepare_all(\n        catalog: &ServiceCatalog,\n        fabric: Option<ServiceFabricEndpoint>,\n        runtime_env: Option<Arc<Value>>,\n        restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,\n    ) -> Self {\n        let manager = Self {\n            fabric,\n            runtime_env,\n            restart_authority,\n            ..Self::default()\n        };\n'''
if old not in text:
    raise SystemExit("missing prepare_all anchor")
text = text.replace(old, new, 1)

# Replace the automatic restart-policy branch with a CONTROL-authority decision
# that fails open to the already-safe local policy. ER may lengthen, never
# shorten, the local bounded backoff.
old = '''            if !should_restart(service.file.restart, status.success()) {\n                service.exit_observed = true;\n                service.restarting = false;\n                tracing::warn!(\n                    service = %service.file.name,\n                    pid = old_pid,\n                    %status,\n                    restart = ?service.file.restart,\n                    "service process exited and restart policy leaves it stopped"\n                );\n                continue;\n            }\n\n            let stable = service\n'''
new = '''            let local_restart = should_restart(service.file.restart, status.success());\n            let mut authority_minimum_backoff = Duration::ZERO;\n            let mut authority_reason: Option<String> = None;\n            let restart = if let Some(authority) = self.restart_authority.as_ref() {\n                let report = build_service_exit_report(&service, &status, old_pid);\n                match tokio::time::timeout(\n                    Duration::from_millis(700),\n                    authority.decide(report),\n                )\n                .await\n                {\n                    Ok(Ok(ServiceRestartDirective::Restart {\n                        minimum_backoff_ms,\n                        reason,\n                    })) => {\n                        authority_minimum_backoff = Duration::from_millis(minimum_backoff_ms)\n                            .min(max_restart_backoff);\n                        authority_reason = Some(reason);\n                        true\n                    }\n                    Ok(Ok(ServiceRestartDirective::Stop { reason })) => {\n                        authority_reason = Some(reason);\n                        false\n                    }\n                    Ok(Ok(ServiceRestartDirective::Default)) => local_restart,\n                    Ok(Err(error)) => {\n                        tracing::warn!(\n                            service = %service.file.name,\n                            error = %error,\n                            "CONTROL ER restart decision failed; using local restart policy"\n                        );\n                        local_restart\n                    }\n                    Err(_) => {\n                        tracing::warn!(\n                            service = %service.file.name,\n                            "CONTROL ER restart decision timed out; using local restart policy"\n                        );\n                        local_restart\n                    }\n                }\n            } else {\n                local_restart\n            };\n\n            if !restart {\n                service.exit_observed = true;\n                service.restarting = false;\n                tracing::warn!(\n                    service = %service.file.name,\n                    pid = old_pid,\n                    %status,\n                    restart = ?service.file.restart,\n                    authority_reason = authority_reason.as_deref().unwrap_or("local restart policy"),\n                    "service process exited and recovery policy leaves it stopped"\n                );\n                continue;\n            }\n\n            let stable = service\n'''
if old not in text:
    raise SystemExit("missing monitor restart policy anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''            let delay = restart_delay(attempt, max_restart_backoff);\n            let file = service.file.clone();\n''',
    '''            let delay = restart_delay(attempt, max_restart_backoff)\n                .max(authority_minimum_backoff)\n                .min(max_restart_backoff);\n            let file = service.file.clone();\n''',
    1,
)
text = text.replace(
    '''                backoff_ms = delay.as_millis() as u64,\n                "service process exited; scheduling restart"\n''',
    '''                backoff_ms = delay.as_millis() as u64,\n                authority_reason = authority_reason.as_deref().unwrap_or("local restart policy"),\n                "service process exited; scheduling restart"\n''',
    1,
)

# Record only the operation identity, never args/event contents.
old = '''        let (address, token, active_call) = {\n            let mut service = handle.lock().await;\n'''
new = '''        let operation_label = operation.diagnostic_label();\n        let (address, token, active_call) = {\n            let mut service = handle.lock().await;\n'''
if old not in text:
    raise SystemExit("missing invoke operation anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''            service.last_activity = Instant::now();\n            let active_call = ActiveCallGuard::acquire(service.active_calls.clone());\n''',
    '''            service.last_activity = Instant::now();\n            service.last_operation = Some(operation_label);\n            let active_call = ActiveCallGuard::acquire(service.active_calls.clone());\n''',
    1,
)

# Portable exit metadata helpers. Unix gets the terminating signal; other
# platforms report the native exit code and leave signal unset.
anchor = "fn should_restart(policy: RestartPolicy, success: bool) -> bool {\n"
helpers = '''fn build_service_exit_report(\n    service: &Managed,\n    status: &ExitStatus,\n    pid: u32,\n) -> ServiceExitReport {\n    let uptime_ms = service\n        .process\n        .as_ref()\n        .map(|process| duration_ms_saturated(process.started_at.elapsed()))\n        .unwrap_or(0);\n    let service_file = service\n        .file\n        .path\n        .file_name()\n        .map(|value| value.to_string_lossy().to_string())\n        .unwrap_or_else(|| format!(\"{}.service\", service.file.name));\n    ServiceExitReport {\n        service: service.file.name.clone(),\n        title: service.file.title.clone(),\n        service_file,\n        pid,\n        exit_success: status.success(),\n        exit_code: status.code(),\n        exit_signal: exit_signal(status),\n        restart: service.file.restart,\n        mode: service.file.mode,\n        previous_restart_attempts: service.restart_attempts,\n        uptime_ms,\n        active_calls: service.active_calls.load(Ordering::Acquire),\n        idle_for_ms: duration_ms_saturated(service.last_activity.elapsed()),\n        last_operation: service.last_operation.clone(),\n        expected: false,\n        phase: \"runtime-monitor\".into(),\n    }\n}\n\nfn duration_ms_saturated(duration: Duration) -> u64 {\n    duration\n        .as_millis()\n        .min(u128::from(u64::MAX))\n        .try_into()\n        .unwrap_or(u64::MAX)\n}\n\n#[cfg(unix)]\nfn exit_signal(status: &ExitStatus) -> Option<i32> {\n    use std::os::unix::process::ExitStatusExt;\n    status.signal()\n}\n\n#[cfg(not(unix))]\nfn exit_signal(_status: &ExitStatus) -> Option<i32> {\n    None\n}\n\n'''
if anchor not in text:
    raise SystemExit("missing should_restart anchor")
text = text.replace(anchor, helpers + anchor, 1)

# Tests protect two invariants: operation diagnostics contain no arguments, and
# CONTROL recommendations cannot create a shorter-than-local restart delay.
test_anchor = "#[cfg(test)]\nmod tests {\n"
if test_anchor not in text:
    raise SystemExit("missing manager test module")
tests = '''    #[test]\n    fn diagnostic_operation_never_contains_call_arguments() {\n        let operation = ServiceOperation::Call {\n            function: \"lookup_user\".into(),\n            args: vec![serde_json::json!({\"secret\": \"must-not-leak\"})],\n        };\n        assert_eq!(operation.diagnostic_label(), \"call:lookup_user\");\n        assert!(!operation.diagnostic_label().contains(\"must-not-leak\"));\n    }\n\n    #[test]\n    fn authority_backoff_is_never_allowed_to_shorten_local_backoff() {\n        let maximum = Duration::from_secs(30);\n        let local = restart_delay(6, maximum);\n        let authority = Duration::from_millis(1);\n        assert_eq!(local.max(authority).min(maximum), local);\n    }\n\n'''
if "diagnostic_operation_never_contains_call_arguments" not in text:
    text = text.replace(test_anchor, test_anchor + tests, 1)
write(path, text)

# Re-export the authority contract from service-runtime's public boundary.
path = "engine/crates/service-runtime/src/lib.rs"
text = read(path)
old = "pub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};"
new = '''pub use manager::{\n    ServiceCallError, ServiceExitReport, ServiceManager, ServiceRestartAuthority,\n    ServiceRestartAuthorityFuture, ServiceRestartDirective, ServiceRuntimeState, ServiceSnapshot,\n};'''
if old not in text:
    raise SystemExit("missing service-runtime manager re-export anchor")
text = text.replace(old, new, 1)
write(path, text)

# ---------------------------------------------------------------------------
# CONTROL ER recovery protocol. Requests/responses may live briefly on disk
# because they are deliberately non-secret diagnostics; every frame is HMAC
# authenticated with a per-backend RAM-only key. The key itself is never in a
# file, argv, or environment variable.
# ---------------------------------------------------------------------------
write(
    "engine/crates/backend/src/er_recovery.rs",
    r'''use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use service_runtime::{
    RestartPolicy, ServiceExitReport, ServiceRestartAuthority, ServiceRestartAuthorityFuture,
    ServiceRestartDirective,
};
use sha2::Sha256;

use crate::host_bootstrap::ErControlKey;

type HmacSha256 = Hmac<Sha256>;

const PROTOCOL_VERSION: u8 = 1;
const REQUEST_MAX_BYTES: usize = 64 * 1024;
const RESPONSE_MAX_BYTES: usize = 16 * 1024;
const REQUEST_TTL: Duration = Duration::from_secs(5);
const FUTURE_SKEW: Duration = Duration::from_secs(2);
const CLIENT_TIMEOUT: Duration = Duration::from_millis(450);
const CLIENT_POLL: Duration = Duration::from_millis(20);
const MAX_REQUESTS_PER_POLL: usize = 32;
const DECISION_LOG_MAX_BYTES: u64 = 512 * 1024;
const DECISION_LOG_MAX_LINES: usize = 2_500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryBootstrapFrame {
    version: u8,
    key_hex: String,
}

impl RecoveryBootstrapFrame {
    pub fn from_key(key: &ErControlKey) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            key_hex: key.to_hex(),
        }
    }

    pub fn into_key(self) -> anyhow::Result<ErControlKey> {
        if self.version != PROTOCOL_VERSION {
            anyhow::bail!(
                "unsupported ER recovery bootstrap version {}",
                self.version
            );
        }
        ErControlKey::from_inherited_hex(&self.key_hex)
    }
}

#[derive(Clone)]
pub struct ErRecoveryClient {
    root: PathBuf,
    key: ErControlKey,
}

impl ErRecoveryClient {
    pub fn new(key: ErControlKey) -> Self {
        Self {
            root: runtime_paths::default_admin_dir().join("er-recovery"),
            key,
        }
    }

    async fn decide_inner(&self, report: ServiceExitReport) -> anyhow::Result<ServiceRestartDirective> {
        ensure_layout(&self.root)?;
        let request_id = random_request_id();
        let created_at_ms = now_unix_ms();
        let mac = request_mac(&self.key, PROTOCOL_VERSION, &request_id, created_at_ms, &report)?;
        let request = SignedRecoveryRequest {
            version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            created_at_ms,
            report,
            mac,
        };
        let request_path = request_path(&self.root, &request_id);
        let response_path = response_path(&self.root, &request_id);
        let io = atomic_io::AtomicIo::new();
        let payload = serde_json::to_vec(&request)?;
        if payload.len() > REQUEST_MAX_BYTES {
            anyhow::bail!("ER recovery request exceeded {REQUEST_MAX_BYTES} bytes");
        }
        io.write_atomic(&request_path, &payload)?;
        harden_file(&request_path);

        let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
        loop {
            if tokio::time::Instant::now() >= deadline {
                let _ = std::fs::remove_file(&request_path);
                let _ = std::fs::remove_file(&response_path);
                anyhow::bail!("CONTROL ER recovery decision timed out");
            }
            match read_limited(&response_path, RESPONSE_MAX_BYTES) {
                Ok(Some(raw)) => {
                    let result = verify_response(&self.key, &request_id, &raw);
                    let _ = std::fs::remove_file(&request_path);
                    let _ = std::fs::remove_file(&response_path);
                    return result;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = std::fs::remove_file(&request_path);
                    let _ = std::fs::remove_file(&response_path);
                    return Err(error);
                }
            }
            tokio::time::sleep(CLIENT_POLL).await;
        }
    }
}

impl ServiceRestartAuthority for ErRecoveryClient {
    fn decide<'a>(&'a self, report: ServiceExitReport) -> ServiceRestartAuthorityFuture<'a> {
        Box::pin(async move { self.decide_inner(report).await })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SignedRecoveryRequest {
    version: u8,
    request_id: String,
    created_at_ms: u64,
    report: ServiceExitReport,
    mac: String,
}

#[derive(Serialize)]
struct RequestMacPayload<'a> {
    version: u8,
    request_id: &'a str,
    created_at_ms: u64,
    report: &'a ServiceExitReport,
}

#[derive(Debug, Serialize, Deserialize)]
struct SignedRecoveryResponse {
    version: u8,
    request_id: String,
    created_at_ms: u64,
    directive: ServiceRestartDirective,
    mac: String,
}

#[derive(Serialize)]
struct ResponseMacPayload<'a> {
    version: u8,
    request_id: &'a str,
    created_at_ms: u64,
    directive: &'a ServiceRestartDirective,
}

pub fn process_pending_requests(
    io: &atomic_io::AtomicIo,
    admin_dir: &Path,
    key: &ErControlKey,
    report_signing_key: &str,
) -> anyhow::Result<usize> {
    let root = admin_dir.join("er-recovery");
    ensure_layout(&root)?;
    let requests = root.join("requests");
    let mut processed = 0usize;

    for entry in std::fs::read_dir(&requests)?.take(MAX_REQUESTS_PER_POLL) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(raw) = read_limited(&path, REQUEST_MAX_BYTES)? else {
            continue;
        };
        let request: SignedRecoveryRequest = match serde_json::from_slice(&raw) {
            Ok(request) => request,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let file_id = path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
        if file_id != request.request_id
            || !valid_request_id(&request.request_id)
            || !fresh_timestamp(request.created_at_ms)
            || request.version != PROTOCOL_VERSION
            || !verify_request_mac(key, &request)?
        {
            let _ = std::fs::remove_file(&path);
            continue;
        }

        let directive = compute_directive(&request.report);
        let why = explain_exit(&request.report);
        let how = explain_activity(&request.report);
        let created_at_ms = now_unix_ms();
        let response = SignedRecoveryResponse {
            version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            created_at_ms,
            mac: response_mac(
                key,
                PROTOCOL_VERSION,
                &request.request_id,
                created_at_ms,
                &directive,
            )?,
            directive: directive.clone(),
        };
        let response_path = response_path(&root, &request.request_id);
        let payload = serde_json::to_vec(&response)?;
        if payload.len() <= RESPONSE_MAX_BYTES {
            io.write_atomic(&response_path, &payload)?;
            harden_file(&response_path);
            append_decision_log(
                io,
                &root,
                &request.report,
                &directive,
                &why,
                &how,
                report_signing_key,
            )?;
            processed = processed.saturating_add(1);
        }
        let _ = std::fs::remove_file(&path);
    }

    Ok(processed)
}

fn compute_directive(report: &ServiceExitReport) -> ServiceRestartDirective {
    let why = explain_exit(report);
    if report.expected {
        return ServiceRestartDirective::Stop {
            reason: format!("planned exit: {why}"),
        };
    }
    match report.restart {
        RestartPolicy::Never => ServiceRestartDirective::Stop {
            reason: format!("restart=never; {why}"),
        },
        RestartPolicy::OnFailure if report.exit_success => ServiceRestartDirective::Stop {
            reason: "service exited successfully and restart=on-failure".into(),
        },
        RestartPolicy::Always | RestartPolicy::OnFailure => {
            let crash_loop_floor = if report.previous_restart_attempts >= 6 {
                5_000
            } else {
                0
            };
            ServiceRestartDirective::Restart {
                minimum_backoff_ms: crash_loop_floor,
                reason: format!("CONTROL ER authorized service-local recovery: {why}"),
            }
        }
    }
}

fn explain_exit(report: &ServiceExitReport) -> String {
    if let Some(signal) = report.exit_signal {
        return format!("terminated by signal {signal}");
    }
    if let Some(code) = report.exit_code {
        return format!("exited with code {code}");
    }
    if report.exit_success {
        "exited successfully".into()
    } else {
        "process exited without a portable code or signal".into()
    }
}

fn explain_activity(report: &ServiceExitReport) -> String {
    let operation = report.last_operation.as_deref().unwrap_or("idle-or-unknown");
    format!(
        "phase={} operation={} active_calls={} uptime_ms={} idle_for_ms={}",
        report.phase, report.active_calls, report.active_calls, report.uptime_ms, report.idle_for_ms
    )
    .replace(
        &format!("operation={}", report.active_calls),
        &format!("operation={operation}"),
    )
}

#[derive(Serialize)]
struct DecisionLogPayload<'a> {
    recorded_at_ms: u64,
    why: &'a str,
    how: &'a str,
    what_was_doing: &'a str,
    report: &'a ServiceExitReport,
    directive: &'a ServiceRestartDirective,
}

#[derive(Serialize)]
struct DecisionLogRecord<'a> {
    payload: DecisionLogPayload<'a>,
    signature: DecisionLogSignature,
}

#[derive(Serialize)]
struct DecisionLogSignature {
    algo: &'static str,
    value: String,
}

fn append_decision_log(
    io: &atomic_io::AtomicIo,
    root: &Path,
    report: &ServiceExitReport,
    directive: &ServiceRestartDirective,
    why: &str,
    how: &str,
    report_signing_key: &str,
) -> anyhow::Result<()> {
    let what_was_doing = report.last_operation.as_deref().unwrap_or("idle-or-unknown");
    let payload = DecisionLogPayload {
        recorded_at_ms: now_unix_ms(),
        why,
        how,
        what_was_doing,
        report,
        directive,
    };
    let canonical = serde_json::to_vec(&payload)?;
    let signature = DecisionLogSignature {
        algo: "hmac-sha256-ephemeral",
        value: hmac_hex(report_signing_key.as_bytes(), &canonical)?,
    };
    let record = DecisionLogRecord { payload, signature };
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    let path = root.join("er-process-decisions.log");
    io.append_locked(&path, &line)?;
    harden_file(&path);
    compact_decision_log(io, &path);
    Ok(())
}

fn compact_decision_log(io: &atomic_io::AtomicIo, path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < DECISION_LOG_MAX_BYTES {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let lines = raw.lines().filter(|line| !line.is_empty()).collect::<Vec<_>>();
    if lines.len() <= DECISION_LOG_MAX_LINES {
        return;
    }
    let compacted = lines[lines.len() - DECISION_LOG_MAX_LINES..].join("\n") + "\n";
    let _ = io.write_atomic(path, compacted.as_bytes());
    harden_file(path);
}

fn request_mac(
    key: &ErControlKey,
    version: u8,
    request_id: &str,
    created_at_ms: u64,
    report: &ServiceExitReport,
) -> anyhow::Result<String> {
    let payload = RequestMacPayload {
        version,
        request_id,
        created_at_ms,
        report,
    };
    hmac_serialized(key, &payload)
}

fn response_mac(
    key: &ErControlKey,
    version: u8,
    request_id: &str,
    created_at_ms: u64,
    directive: &ServiceRestartDirective,
) -> anyhow::Result<String> {
    let payload = ResponseMacPayload {
        version,
        request_id,
        created_at_ms,
        directive,
    };
    hmac_serialized(key, &payload)
}

fn verify_request_mac(key: &ErControlKey, request: &SignedRecoveryRequest) -> anyhow::Result<bool> {
    let payload = RequestMacPayload {
        version: request.version,
        request_id: &request.request_id,
        created_at_ms: request.created_at_ms,
        report: &request.report,
    };
    verify_serialized(key, &payload, &request.mac)
}

fn verify_response(
    key: &ErControlKey,
    expected_request_id: &str,
    raw: &[u8],
) -> anyhow::Result<ServiceRestartDirective> {
    let response: SignedRecoveryResponse = serde_json::from_slice(raw)?;
    if response.version != PROTOCOL_VERSION
        || response.request_id != expected_request_id
        || !fresh_timestamp(response.created_at_ms)
    {
        anyhow::bail!("CONTROL ER returned an invalid or stale recovery response");
    }
    let payload = ResponseMacPayload {
        version: response.version,
        request_id: &response.request_id,
        created_at_ms: response.created_at_ms,
        directive: &response.directive,
    };
    if !verify_serialized(key, &payload, &response.mac)? {
        anyhow::bail!("CONTROL ER recovery response authentication failed");
    }
    match &response.directive {
        ServiceRestartDirective::Restart { reason, .. }
        | ServiceRestartDirective::Stop { reason } if reason.len() > 512 => {
            anyhow::bail!("CONTROL ER recovery reason exceeded 512 bytes")
        }
        _ => {}
    }
    Ok(response.directive)
}

fn hmac_serialized<T: Serialize>(key: &ErControlKey, value: &T) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    hmac_hex(key.as_bytes(), &bytes)
}

fn verify_serialized<T: Serialize>(key: &ErControlKey, value: &T, expected: &str) -> anyhow::Result<bool> {
    let bytes = serde_json::to_vec(value)?;
    let decoded = match hex::decode(expected) {
        Ok(decoded) => decoded,
        Err(_) => return Ok(false),
    };
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts 256-bit key");
    mac.update(&bytes);
    Ok(mac.verify_slice(&decoded).is_ok())
}

fn hmac_hex(key: &[u8], bytes: &[u8]) -> anyhow::Result<String> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid HMAC key length"))?;
    mac.update(bytes);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn ensure_layout(root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join("requests"))?;
    std::fs::create_dir_all(root.join("responses"))?;
    harden_dir(root);
    harden_dir(&root.join("requests"));
    harden_dir(&root.join("responses"));
    Ok(())
}

#[cfg(unix)]
fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) {}

#[cfg(unix)]
fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_file(_path: &Path) {}

fn read_limited(path: &Path, max_bytes: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > max_bytes as u64 {
        anyhow::bail!("ER recovery frame exceeded {max_bytes} bytes");
    }
    Ok(Some(std::fs::read(path)?))
}

fn request_path(root: &Path, request_id: &str) -> PathBuf {
    root.join("requests").join(format!("{request_id}.json"))
}

fn response_path(root: &Path, request_id: &str) -> PathBuf {
    root.join("responses").join(format!("{request_id}.json"))
}

fn random_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn valid_request_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fresh_timestamp(created_at_ms: u64) -> bool {
    let now = now_unix_ms();
    let oldest = now.saturating_sub(REQUEST_TTL.as_millis() as u64);
    let newest = now.saturating_add(FUTURE_SKEW.as_millis() as u64);
    created_at_ms >= oldest && created_at_ms <= newest
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use service_runtime::ServiceMode;

    fn report(restart: RestartPolicy, success: bool, attempts: u32) -> ServiceExitReport {
        ServiceExitReport {
            service: "auth".into(),
            title: "Auth".into(),
            service_file: "auth.service".into(),
            pid: 42,
            exit_success: success,
            exit_code: Some(if success { 0 } else { 1 }),
            exit_signal: None,
            restart,
            mode: ServiceMode::Resident,
            previous_restart_attempts: attempts,
            uptime_ms: 800,
            active_calls: 1,
            idle_for_ms: 2,
            last_operation: Some("call:lookup".into()),
            expected: false,
            phase: "runtime-monitor".into(),
        }
    }

    #[test]
    fn decision_respects_service_restart_policy() {
        assert!(matches!(
            compute_directive(&report(RestartPolicy::Never, false, 0)),
            ServiceRestartDirective::Stop { .. }
        ));
        assert!(matches!(
            compute_directive(&report(RestartPolicy::OnFailure, true, 0)),
            ServiceRestartDirective::Stop { .. }
        ));
        assert!(matches!(
            compute_directive(&report(RestartPolicy::OnFailure, false, 0)),
            ServiceRestartDirective::Restart { .. }
        ));
    }

    #[test]
    fn crash_loop_gets_a_control_backoff_floor() {
        match compute_directive(&report(RestartPolicy::Always, false, 6)) {
            ServiceRestartDirective::Restart {
                minimum_backoff_ms,
                ..
            } => assert!(minimum_backoff_ms >= 5_000),
            other => panic!("expected restart directive, got {other:?}"),
        }
    }
}
''',
)

# ---------------------------------------------------------------------------
# ER consumes authenticated recovery requests only in CONTROL mode and records
# its richer process-decision report. BASIC never receives/uses a control key.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/error_reporter_daemon.rs"
text = read(path)
text = text.replace(
    "use error_client::{IssueCategory, IssueLevel, QueueEntry};\n",
    "use error_client::{IssueCategory, IssueLevel, QueueEntry};\n\nuse crate::host_bootstrap::ErControlKey;\n",
    1,
)
old = '''impl ParentBootstrapFrame {\n    pub fn control() -> Self {\n        Self {\n            version: 1,\n            authority: ErAuthority::Control,\n            signing_key_hex: random_key_hex(),\n            control_key_hex: Some(random_key_hex()),\n        }\n    }\n}\n'''
new = '''impl ParentBootstrapFrame {\n    pub fn control(control_key: &ErControlKey) -> Self {\n        Self {\n            version: 1,\n            authority: ErAuthority::Control,\n            signing_key_hex: random_key_hex(),\n            control_key_hex: Some(control_key.to_hex()),\n        }\n    }\n}\n'''
if old not in text:
    raise SystemExit("missing ParentBootstrapFrame::control anchor")
text = text.replace(old, new, 1)
old = '''    pub fn control_key(&self) -> Option<&str> {\n        self.control_key.as_deref()\n    }\n}\n'''
new = '''    pub fn control_key(&self) -> Option<&str> {\n        self.control_key.as_deref()\n    }\n\n    pub fn recovery_key(&self) -> anyhow::Result<Option<ErControlKey>> {\n        self.control_key\n            .as_deref()\n            .map(ErControlKey::from_inherited_hex)\n            .transpose()\n    }\n}\n'''
if old not in text:
    raise SystemExit("missing ErBootstrap control_key anchor")
text = text.replace(old, new, 1)
old = '''    let authority = bootstrap.authority();\n    let restart_control = authority.can_control_restarts();\n    let com_key_in_memory = bootstrap.control_key().is_some();\n    let signing_key = bootstrap.signing_key.as_str();\n'''
new = '''    let authority = bootstrap.authority();\n    let restart_control = authority.can_control_restarts();\n    let recovery_key = bootstrap.recovery_key()?;\n    let com_key_in_memory = recovery_key.is_some();\n    let signing_key = bootstrap.signing_key.as_str();\n'''
if old not in text:
    raise SystemExit("missing ER run bootstrap anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''    let mut poll_interval = tokio::time::interval(Duration::from_millis(poll_interval_ms));\n    let mut status_interval = tokio::time::interval(STATUS_FLUSH_INTERVAL);\n''',
    '''    let mut poll_interval = tokio::time::interval(Duration::from_millis(poll_interval_ms));\n    let mut status_interval = tokio::time::interval(STATUS_FLUSH_INTERVAL);\n    let mut recovery_interval = tokio::time::interval(Duration::from_millis(25));\n''',
    1,
)
select_anchor = '''            _ = status_interval.tick() => {\n                write_status(&io, &status_path, &StatusReport {\n'''
if select_anchor not in text:
    raise SystemExit("missing ER status select anchor")
recovery_arm = '''            _ = recovery_interval.tick(), if recovery_key.is_some() => {\n                if let Some(key) = recovery_key.as_ref() {\n                    if let Err(error) = crate::er_recovery::process_pending_requests(\n                        &io,\n                        &admin_dir,\n                        key,\n                        signing_key,\n                    ) {\n                        last_error_message = Some(format!(\n                            \"CONTROL ER recovery queue failed: {error}\"\n                        ));\n                    }\n                }\n            }\n'''
text = text.replace(select_anchor, recovery_arm + select_anchor, 1)
text = text.replace(
    "        let frame = ParentBootstrapFrame::control();",
    "        let key = ErControlKey::from_inherited_hex(&random_key_hex()).unwrap();\n        let frame = ParentBootstrapFrame::control(&key);",
    1,
)
write(path, text)

# ---------------------------------------------------------------------------
# Service Mother receives the SAME backend-generation CONTROL key over its
# already-authenticated inherited stdin bootstrap stream, then installs the ER
# recovery client in ServiceManager. No key reaches argv/env/disk.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
# Read third bootstrap frame after Runtime ENV and before parent-liveness watcher.
anchor = '''    let runtime_env_frame = if args.iter().any(|arg| arg == "--runtime-env-frame") {\n        service_runtime::read_parent_bootstrap_json_if_configured("Service Mother Runtime ENV")?\n            .ok_or_else(|| {\n                anyhow::anyhow!("Service Mother Runtime ENV frame requires inherited bootstrap")\n            })?\n            .into()\n    } else {\n        None\n    };\n'''
if anchor not in text:
    raise SystemExit("missing Mother Runtime ENV read anchor")
addition = anchor + '''    let er_recovery_key = if args.iter().any(|arg| arg == "--er-recovery-frame") {\n        let value = service_runtime::read_parent_bootstrap_json_if_configured(\n            "Service Mother ER recovery",\n        )?\n        .ok_or_else(|| {\n            anyhow::anyhow!("Service Mother ER recovery frame requires inherited bootstrap")\n        })?;\n        let frame: crate::er_recovery::RecoveryBootstrapFrame = serde_json::from_value(value)?;\n        Some(frame.into_key()?)\n    } else {\n        None\n    };\n'''
text = text.replace(anchor, addition, 1)
old = '''        Some(catalog) => {\n            ServiceManager::prepare_all_with_fabric_and_runtime_env(\n                catalog,\n                server.fabric_endpoint(),\n                runtime_env.clone(),\n            )\n            .await\n        }\n'''
new = '''        Some(catalog) => {\n            let restart_authority = er_recovery_key.clone().map(|key| {\n                Arc::new(crate::er_recovery::ErRecoveryClient::new(key))\n                    as Arc<dyn service_runtime::ServiceRestartAuthority>\n            });\n            ServiceManager::prepare_all_with_fabric_runtime_env_and_restart_authority(\n                catalog,\n                server.fabric_endpoint(),\n                runtime_env.clone(),\n                restart_authority,\n            )\n            .await\n        }\n'''
if old not in text:
    raise SystemExit("missing Mother ServiceManager construction anchor")
text = text.replace(old, new, 1)
# spawn_process carries optional key and marks third frame in argv only by a
# non-secret boolean flag.
old = '''async fn spawn_process(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: &serde_json::Value,\n    existing_manager: Option<&ServiceManager>,\n) -> anyhow::Result<ServiceMotherProcess> {'''
new = '''async fn spawn_process(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: &serde_json::Value,\n    er_control_key: Option<&crate::host_bootstrap::ErControlKey>,\n    existing_manager: Option<&ServiceManager>,\n) -> anyhow::Result<ServiceMotherProcess> {'''
if old not in text:
    raise SystemExit("missing Mother spawn_process signature anchor")
text = text.replace(old, new, 1)
old = '''    let mut child = match command\n        .args(["--service-mother", "--launch-separate"])\n        .arg("--service-catalog-fingerprint")\n'''
new = '''    command.args(["--service-mother", "--launch-separate"]);\n    if er_control_key.is_some() {\n        command.arg("--er-recovery-frame");\n    }\n    let mut child = match command\n        .arg("--service-catalog-fingerprint")\n'''
if old not in text:
    raise SystemExit("missing Mother command spawn anchor")
text = text.replace(old, new, 1)
# Write ER frame after runtime-env frame.
anchor = '''    if let Err(error) =\n        service_runtime::write_parent_bootstrap_json(&mut liveness, runtime_env).await\n    {\n        cleanup_failed_spawn(&mut child).await;\n        return Err(anyhow::anyhow!(\n            "send Service Mother Runtime ENV snapshot: {error}"\n        ));\n    }\n'''
if anchor not in text:
    raise SystemExit("missing Mother Runtime ENV write anchor")
addition = anchor + '''    if let Some(er_control_key) = er_control_key {\n        let frame = serde_json::to_value(crate::er_recovery::RecoveryBootstrapFrame::from_key(\n            er_control_key,\n        ))?;\n        if let Err(error) =\n            service_runtime::write_parent_bootstrap_json(&mut liveness, &frame).await\n        {\n            cleanup_failed_spawn(&mut child).await;\n            return Err(anyhow::anyhow!(\n                "send Service Mother ER recovery capability: {error}"\n            ));\n        }\n    }\n'''
text = text.replace(anchor, addition, 1)
old = '''pub async fn spawn(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: Arc<serde_json::Value>,\n) -> anyhow::Result<ServiceMotherSupervisor> {'''
new = '''pub async fn spawn(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: Arc<serde_json::Value>,\n    er_control_key: Option<crate::host_bootstrap::ErControlKey>,\n) -> anyhow::Result<ServiceMotherSupervisor> {'''
if old not in text:
    raise SystemExit("missing Mother spawn signature anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''        runtime_env.as_ref(),\n        None,\n    )''',
    '''        runtime_env.as_ref(),\n        er_control_key.as_ref(),\n        None,\n    )''',
    1,
)
text = text.replace(
    '''            runtime_env,\n            supervisor_manager,\n''',
    '''            runtime_env,\n            er_control_key,\n            supervisor_manager,\n''',
    1,
)
old = '''    runtime_env: Arc<serde_json::Value>,\n    manager: ServiceManager,\n'''
new = '''    runtime_env: Arc<serde_json::Value>,\n    er_control_key: Option<crate::host_bootstrap::ErControlKey>,\n    manager: ServiceManager,\n'''
if old not in text:
    raise SystemExit("missing Mother supervise signature anchor")
text = text.replace(old, new, 1)
# respawn path
text = text.replace(
    '''                runtime_env.as_ref(),\n                Some(&manager),\n            )''',
    '''                runtime_env.as_ref(),\n                er_control_key.as_ref(),\n                Some(&manager),\n            )''',
    1,
)
write(path, text)

# ---------------------------------------------------------------------------
# Root backend creates exactly one CONTROL key after HostBootstrapReady and
# shares it with ER + Mother. The ER process may rotate its report-signing key
# each refresh while the backend-generation recovery key remains stable.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
if "mod er_recovery;\n" not in text:
    text = text.replace("mod error_reporter_daemon;\n", "mod error_reporter_daemon;\nmod er_recovery;\n", 1)
old = '''fn spawn_error_reporter_daemon_process(\n    maintenance: Arc<MaintenanceMetrics>,\n    refresh_interval: Duration,\n    control_enabled: bool,\n) -> anyhow::Result<tokio::task::JoinHandle<()>> {'''
new = '''fn spawn_error_reporter_daemon_process(\n    maintenance: Arc<MaintenanceMetrics>,\n    refresh_interval: Duration,\n    control_key: Option<host_bootstrap::ErControlKey>,\n) -> anyhow::Result<tokio::task::JoinHandle<()>> {'''
if old not in text:
    raise SystemExit("missing ER spawn function signature")
text = text.replace(old, new, 1)
text = text.replace(
    "            let frame = control_enabled.then(error_reporter_daemon::ParentBootstrapFrame::control);",
    "            let frame = control_key\n                .as_ref()\n                .map(error_reporter_daemon::ParentBootstrapFrame::control);",
    1,
)
text = text.replace(
    '''                authority = if control_enabled { "control" } else { "basic" },\n''',
    '''                authority = if control_key.is_some() { "control" } else { "basic" },\n''',
    1,
)
# Issue capability immediately after hard HostBootstrap entry to boot_and_run,
# before ER/Mother spawning, and keep it RAM-only.
old = '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {\n    boot_trace("start");\n'''
new = '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {\n    let er_control_key = host_ready.issue_er_control_key();\n    boot_trace("start");\n'''
if old not in text:
    raise SystemExit("missing boot_and_run anchor")
text = text.replace(old, new, 1)
text = text.replace(
    '''        refresh_interval,\n        host_ready.er_control_enabled(),\n    )?;''',
    '''        refresh_interval,\n        er_control_key.clone(),\n    )?;''',
    1,
)
old = '''                service_runtime_env.clone(),\n            )\n            .await?,'''
new = '''                service_runtime_env.clone(),\n                er_control_key.clone(),\n            )\n            .await?,'''
if old not in text:
    raise SystemExit("missing main Service Mother spawn anchor")
text = text.replace(old, new, 1)
write(path, text)

# ---------------------------------------------------------------------------
# Docs: CONTROL ER is now in the actual service-local recovery decision loop.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
section = r'''

### CONTROL ER recovery decisions

A verified HostBootstrap now issues one per-backend-generation ER CONTROL key in
RAM. The backend sends that same capability independently to the Error Reporter
and Service Mother through inherited one-shot bootstrap pipes. It never appears
in a command line, environment variable, or key file. Refreshing the ER rotates
its ephemeral report-signing key while retaining the backend-generation recovery
capability, so Mother does not need to be restarted merely because ER refreshes.

For an unexpected `.service` exit, Mother sends CONTROL ER a bounded,
authenticated `ServiceExitReport` containing only process/runtime metadata:
service identity, `.service` filename, PID, exit code or Unix signal, restart
policy, mode, uptime, prior restart attempts, active-call count, idle duration,
and a non-sensitive last-operation label such as `call:lookup_user`. Arguments,
request bodies, event payloads, credentials, Runtime ENV values, and database
rows are never included. CONTROL ER returns a service-local Restart/Stop/Default
directive and records a signed detailed decision report including `why`, `how`,
and `what_was_doing`.

The Service Manager waits only a bounded time for CONTROL ER. Missing, BASIC,
crashed, unauthenticated, stale, or timed-out ER responses fall back to the
existing local `.service` restart policy. ER can request a longer delay but
cannot shorten the manager's exponential backoff or exceed its configured cap.
This path can only decide recovery for the service that exited; it cannot turn
one service crash into a whole-RBE restart. Explicit operator restarts and
planned shutdowns stay outside the unexpected-crash decision path.
'''
if "### CONTROL ER recovery decisions" not in text:
    text += section
write(path, text)
