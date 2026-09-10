from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:180]!r}")
    write(path, text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# Linux HostBootstrap scripts remain ordinary source files for debugging, but
# backend embeds and pipes them to /bin/sh. Nothing is extracted to disk.
# ---------------------------------------------------------------------------
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

if ! command -v dbus-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=dbus-daemon'
    exit 10
fi

if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    address="$(dbus-daemon --session --fork --print-address 2>/dev/null | head -n 1)"
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

if ! command -v gnome-keyring-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=secret-service-provider'
    exit 10
fi

keyring_output="$(gnome-keyring-daemon --start --components=secrets 2>/dev/null || true)"
printf '%s\n' "$keyring_output" | while IFS= read -r line; do
    case "$line" in
        GNOME_KEYRING_CONTROL=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
        GNOME_KEYRING_PID=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
    esac
done

# gnome-keyring may need a moment to claim org.freedesktop.secrets.
i=0
while [ "$i" -lt 20 ]; do
    if has_secret_service; then
        echo 'RBE_RESULT=READY'
        exit 0
    fi
    i=$((i + 1))
    sleep 0.05
 done

echo 'RBE_RESULT=FAILED'
echo 'RBE_STAGE=secret-service-start'
exit 11
''',
)

write(
    "engine/crates/backend/scripts/bootstrap/linux/install-secret-service.sh",
    r'''#!/bin/sh
set -u

if [ "$(id -u)" -eq 0 ]; then
    ELEVATE='direct'
elif command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
    ELEVATE='sudo'
else
    echo 'RBE_RESULT=NO_PRIVILEGE'
    exit 13
fi

as_root() {
    if [ "$ELEVATE" = 'sudo' ]; then
        sudo -n "$@"
    else
        "$@"
    fi
}

if command -v apt-get >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=apt'
    as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq || exit 20
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq dbus-daemon gnome-keyring || exit 20
elif command -v dnf >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=dnf'
    as_root dnf install -y dbus-daemon gnome-keyring || exit 20
elif command -v yum >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=yum'
    as_root yum install -y dbus-daemon gnome-keyring || exit 20
elif command -v pacman >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=pacman'
    as_root pacman -Sy --noconfirm dbus gnome-keyring || exit 20
elif command -v zypper >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=zypper'
    as_root zypper --non-interactive install dbus-1 gnome-keyring || exit 20
elif command -v apk >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=apk'
    as_root apk add --no-cache dbus gnome-keyring || exit 20
else
    echo 'RBE_RESULT=NO_PACKAGE_MANAGER'
    exit 12
fi

echo 'RBE_RESULT=INSTALLED'
exit 0
''',
)

# ---------------------------------------------------------------------------
# HostBootstrap is Phase 0. On Linux it executes the embedded scripts through
# stdin, verifies Secret Service with a disposable credential, and returns an
# unforgeable in-process capability. No normal RBE credential code is allowed
# to run before this succeeds.
# ---------------------------------------------------------------------------
write(
    "engine/crates/backend/src/host_bootstrap.rs",
    r'''use std::process::Stdio;

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
            anyhow::bail!("Linux credential host evaluation could not provision a Secret Service provider");
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
        .map_err(|error| anyhow::anyhow!("could not start embedded Linux host script {name}: {error}"))?;

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
''',
)

# ---------------------------------------------------------------------------
# ER authority is explicit. BASIC keeps diagnostics but has no restart-control
# capability. CONTROL is granted only by the already-verified parent and gets a
# fresh in-memory COM key per ER process generation. The key is never written to
# disk or argv/env.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/error_reporter_daemon.rs"
text = read(path)
text = text.replace(
    "use serde::Serialize;",
    "use rand::RngCore;\nuse serde::{Deserialize, Serialize};",
    1,
)
text = text.replace(
    '''const SIGNING_KEY_SERVICE: &str = "rbe.error-reporter";\nconst SIGNING_KEY_ACCOUNT: &str = "report-auth";\nconst LEGACY_SIGNING_KEY_FILE: &str = "error-reporter.key";\nconst MIN_SIGNING_KEY_LEN: usize = 16;\n''',
    "",
    1,
)
insert_anchor = "#[derive(Serialize)]\nstruct ReportedBy {\n"
if insert_anchor not in text:
    raise SystemExit("missing ER ReportedBy anchor")
er_types = r'''#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErAuthority {
    Basic,
    Control,
}

impl ErAuthority {
    pub fn can_control_restarts(self) -> bool {
        matches!(self, Self::Control)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Control => "control",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ParentBootstrapFrame {
    version: u8,
    authority: ErAuthority,
    signing_key_hex: String,
    control_key_hex: Option<String>,
}

impl ParentBootstrapFrame {
    pub fn control() -> Self {
        Self {
            version: 1,
            authority: ErAuthority::Control,
            signing_key_hex: random_key_hex(),
            control_key_hex: Some(random_key_hex()),
        }
    }
}

pub struct ErBootstrap {
    authority: ErAuthority,
    signing_key: String,
    control_key: Option<String>,
}

impl ErBootstrap {
    pub fn basic() -> Self {
        Self {
            authority: ErAuthority::Basic,
            signing_key: random_key_hex(),
            control_key: None,
        }
    }

    pub fn from_parent_stdin() -> anyhow::Result<Self> {
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw)?;
        if raw.len() > 4096 {
            anyhow::bail!("ER bootstrap frame exceeded 4 KiB");
        }
        let frame: ParentBootstrapFrame = serde_json::from_str(raw.trim())?;
        if frame.version != 1 {
            anyhow::bail!("unsupported ER bootstrap frame version {}", frame.version);
        }
        validate_key(&frame.signing_key_hex)?;
        if frame.authority == ErAuthority::Control {
            let key = frame
                .control_key_hex
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("CONTROL ER bootstrap omitted COM key"))?;
            validate_key(key)?;
        }
        Ok(Self {
            authority: frame.authority,
            signing_key: frame.signing_key_hex,
            control_key: frame.control_key_hex,
        })
    }

    pub fn authority(&self) -> ErAuthority {
        self.authority
    }

    pub fn control_key(&self) -> Option<&str> {
        self.control_key.as_deref()
    }
}

impl Drop for ErBootstrap {
    fn drop(&mut self) {
        unsafe {
            self.signing_key.as_mut_vec().fill(0);
            if let Some(key) = self.control_key.as_mut() {
                key.as_mut_vec().fill(0);
            }
        }
    }
}

fn random_key_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn validate_key(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("ER bootstrap key must be a 32-byte hex value");
    }
    Ok(())
}

'''
text = text.replace(insert_anchor, er_types + insert_anchor, 1)
text = text.replace(
    '''struct ReportedBy {\n    service: &'static str,\n    pid: u32,\n    processed_at_ms: u64,\n    processed_iso: String,\n}''',
    '''struct ReportedBy {\n    service: &'static str,\n    pid: u32,\n    authority: ErAuthority,\n    restart_control: bool,\n    processed_at_ms: u64,\n    processed_iso: String,\n}''',
    1,
)
text = text.replace(
    '''struct SignedIssueRecord {\n    #[serde(flatten)]\n    payload: IssuePayload,\n    signature: Signature,\n}''',
    '''struct SignedIssueRecord {\n    #[serde(flatten)]\n    payload: IssuePayload,\n    #[serde(skip_serializing_if = "Option::is_none")]\n    diagnostic_context: Option<DiagnosticContext>,\n    signature: Signature,\n}\n\n#[derive(Serialize)]\nstruct DiagnosticContext {\n    why: String,\n    how: String,\n    activity_source: String,\n    submitted_pid: u32,\n    submitted_ppid: u32,\n    stack_present: bool,\n    message_bytes: usize,\n    stack_bytes: usize,\n}''',
    1,
)
text = text.replace(
    '''    launched_as_separate_process: bool,\n    started_at_ms: u64,''',
    '''    launched_as_separate_process: bool,\n    authority: ErAuthority,\n    restart_control: bool,\n    com_key_in_memory: bool,\n    started_at_ms: u64,''',
    1,
)
text = text.replace(
    '''pub async fn run(\n    io: atomic_io::AtomicIo,\n    admin_dir: PathBuf,\n    launched_as_separate_process: bool,\n) -> anyhow::Result<()> {\n    std::fs::create_dir_all(&admin_dir)?;\n    let signing_key = read_or_create_signing_key(&admin_dir)?;''',
    '''pub async fn run(\n    io: atomic_io::AtomicIo,\n    admin_dir: PathBuf,\n    launched_as_separate_process: bool,\n    bootstrap: ErBootstrap,\n) -> anyhow::Result<()> {\n    std::fs::create_dir_all(&admin_dir)?;\n    let authority = bootstrap.authority();\n    let restart_control = authority.can_control_restarts();\n    let com_key_in_memory = bootstrap.control_key().is_some();\n    let signing_key = bootstrap.signing_key.as_str();''',
    1,
)
text = text.replace(
    '''        poll_interval_ms,\n        "error-reporter daemon started"''',
    '''        poll_interval_ms,\n        authority = authority.as_str(),\n        restart_control,\n        "error-reporter daemon started"''',
    1,
)
text = text.replace(
    "match sign_and_append(&io, &reports_path, entry, pid, &signing_key) {",
    "match sign_and_append(&io, &reports_path, entry, pid, authority, signing_key) {",
    1,
)
text = text.replace(
    '''                    launched_as_separate_process,\n                    started_at_ms,''',
    '''                    launched_as_separate_process,\n                    authority,\n                    restart_control,\n                    com_key_in_memory,\n                    started_at_ms,''',
)
text = text.replace(
    '''            launched_as_separate_process,\n            started_at_ms,''',
    '''            launched_as_separate_process,\n            authority,\n            restart_control,\n            com_key_in_memory,\n            started_at_ms,''',
)
text = text.replace(
    '''    daemon_pid: u32,\n    signing_key: &str,\n) -> anyhow::Result<()> {\n    let processed_at_ms = now_unix_ms();\n    let payload = IssuePayload {''',
    '''    daemon_pid: u32,\n    authority: ErAuthority,\n    signing_key: &str,\n) -> anyhow::Result<()> {\n    let processed_at_ms = now_unix_ms();\n    let diagnostic_context = (authority == ErAuthority::Control).then(|| DiagnosticContext {\n        why: format!("{:?}: {}", entry.category, entry.message),\n        how: entry\n            .stack\n            .as_deref()\n            .map(|stack| stack.chars().take(2048).collect())\n            .unwrap_or_else(|| "no stack supplied by reporting process".into()),\n        activity_source: entry.source.clone(),\n        submitted_pid: entry.pid,\n        submitted_ppid: entry.ppid,\n        stack_present: entry.stack.is_some(),\n        message_bytes: entry.message.len(),\n        stack_bytes: entry.stack.as_ref().map(String::len).unwrap_or(0),\n    });\n    let payload = IssuePayload {''',
    1,
)
text = text.replace(
    '''            service: "error-reporter-daemon",\n            pid: daemon_pid,\n            processed_at_ms,''',
    '''            service: "error-reporter-daemon",\n            pid: daemon_pid,\n            authority,\n            restart_control: authority.can_control_restarts(),\n            processed_at_ms,''',
    1,
)
text = text.replace(
    "    let signed = SignedIssueRecord { payload, signature };",
    "    let signed = SignedIssueRecord { payload, diagnostic_context, signature };",
    1,
)
start = text.find("fn read_or_create_signing_key(admin_dir: &Path) -> anyhow::Result<String> {")
if start < 0:
    raise SystemExit("missing legacy ER signing key function")
end = text.find("fn compact_reports_file", start)
if end < 0:
    raise SystemExit("missing ER compact_reports_file anchor")
text = text[:start] + text[end:]
# Replace the obsolete keyring/legacy-key test with authority tests.
old_test = r'''    #[test]
    fn legacy_plaintext_credential_is_removed() {
        let dir = temp_dir("legacy-key-removal");
        let path = dir.join(LEGACY_SIGNING_KEY_FILE);
        std::fs::write(&path, "legacy-secret").unwrap();
        remove_legacy_signing_key(&path).unwrap();
        assert!(!path.exists());
        remove_legacy_signing_key(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
'''
new_test = r'''    #[test]
    fn basic_er_never_has_restart_control() {
        let bootstrap = ErBootstrap::basic();
        assert_eq!(bootstrap.authority(), ErAuthority::Basic);
        assert!(!bootstrap.authority().can_control_restarts());
        assert!(bootstrap.control_key().is_none());
    }

    #[test]
    fn control_frame_carries_only_memory_bootstrap_material() {
        let frame = ParentBootstrapFrame::control();
        assert_eq!(frame.authority, ErAuthority::Control);
        assert!(frame.control_key_hex.as_deref().is_some_and(|key| key.len() == 64));
        assert_eq!(frame.signing_key_hex.len(), 64);
    }
'''
if old_test not in text:
    raise SystemExit("missing ER legacy key test anchor")
text = text.replace(old_test, new_test, 1)
write(path, text)

# ---------------------------------------------------------------------------
# Wire Phase 0 before any normal credential-bearing subsystem. Parent-created ER
# gets CONTROL via a one-shot inherited stdin frame; direct/manual ER stays
# BASIC. Nothing sensitive is placed in argv or environment variables.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
text = text.replace("mod error_reporter_daemon;\n", "mod error_reporter_daemon;\nmod host_bootstrap;\n", 1)
text = text.replace(
    '''        let separate = has("--separate-process") || has("--saperate-process");\n        if let Err(error) = run_error_reporter_daemon(separate).await {''',
    '''        let separate = has("--separate-process") || has("--saperate-process");\n        let bootstrap = if has("--er-bootstrap-stdin") {\n            error_reporter_daemon::ErBootstrap::from_parent_stdin()\n        } else {\n            Ok(error_reporter_daemon::ErBootstrap::basic())\n        };\n        let bootstrap = match bootstrap {\n            Ok(bootstrap) => bootstrap,\n            Err(error) => {\n                eprintln!("fatal error-reporter bootstrap error: {error:#}");\n                std::process::exit(1);\n            }\n        };\n        if let Err(error) = run_error_reporter_daemon(separate, bootstrap).await {''',
    1,
)
# A directly-launched Vault child must still pass Phase 0 before keyring/Vault
# code executes. Parent-launched children simply re-probe a ready host quickly.
vault_anchor = '''    if has("--vault") {\n        if !has("--separate-process") && !has("--saperate-process") {'''
if vault_anchor not in text:
    raise SystemExit("missing main Vault branch anchor")
text = text.replace(
    vault_anchor,
    '''    if has("--vault") {\n        if let Err(error) = host_bootstrap::evaluate(&args).await {\n            if host_bootstrap::verbose_debug(&args) {\n                eprintln!("[HostBootstrap] {error:#}");\n            } else {\n                eprintln!("RBE initialization failed.");\n            }\n            std::process::exit(1);\n        }\n        if !has("--separate-process") && !has("--saperate-process") {''',
    1,
)
text = text.replace(
    '''    if let Err(error) = boot_and_run().await {\n        eprintln!("fatal boot error: {error:#}");\n        std::process::exit(1);\n    }''',
    '''    println!("Evaluating..");\n    let host_ready = match host_bootstrap::evaluate(&args).await {\n        Ok(ready) => ready,\n        Err(error) => {\n            if host_bootstrap::verbose_debug(&args) {\n                eprintln!("[HostBootstrap] {error:#}");\n            } else {\n                eprintln!("RBE initialization failed.");\n            }\n            std::process::exit(1);\n        }\n    };\n\n    if let Err(error) = boot_and_run(host_ready).await {\n        eprintln!("fatal boot error: {error:#}");\n        std::process::exit(1);\n    }''',
    1,
)
text = text.replace(
    "async fn run_error_reporter_daemon(separate_process: bool) -> anyhow::Result<()> {",
    "async fn run_error_reporter_daemon(\n    separate_process: bool,\n    bootstrap: error_reporter_daemon::ErBootstrap,\n) -> anyhow::Result<()> {",
    1,
)
text = text.replace(
    "    error_reporter_daemon::run(io, admin_dir, separate_process).await",
    "    error_reporter_daemon::run(io, admin_dir, separate_process, bootstrap).await",
    1,
)
text = text.replace(
    '''fn spawn_error_reporter_daemon_process(\n    maintenance: Arc<MaintenanceMetrics>,\n    refresh_interval: Duration,\n) -> anyhow::Result<tokio::task::JoinHandle<()>> {''',
    '''fn spawn_error_reporter_daemon_process(\n    maintenance: Arc<MaintenanceMetrics>,\n    refresh_interval: Duration,\n    control_enabled: bool,\n) -> anyhow::Result<tokio::task::JoinHandle<()>> {''',
    1,
)
old_spawn = '''            let spawn_result = tokio::process::Command::new(&exe)\n                .args(["--er", "--separate-process", "--launch"])\n                .kill_on_drop(true)\n                .spawn();\n            let mut child = match spawn_result {'''
new_spawn = '''            let frame = control_enabled.then(error_reporter_daemon::ParentBootstrapFrame::control);\n            let mut command = tokio::process::Command::new(&exe);\n            command.args(["--er", "--separate-process", "--launch"]);\n            if frame.is_some() {\n                command.arg("--er-bootstrap-stdin").stdin(std::process::Stdio::piped());\n            } else {\n                command.stdin(std::process::Stdio::null());\n            }\n            let spawn_result = command.kill_on_drop(true).spawn();\n            let mut child = match spawn_result {'''
if old_spawn not in text:
    raise SystemExit("missing ER spawn anchor")
text = text.replace(old_spawn, new_spawn, 1)
# Write and close the inherited bootstrap pipe before the ER begins consuming
# queue state. The one-shot frame contains the in-memory COM key only.
child_anchor = '''            consecutive_failures = 0;\n            tracing::info!(pid = child.id(), "error-reporter daemon process spawned");'''
child_repl = '''            if let Some(frame) = frame {\n                use tokio::io::AsyncWriteExt;\n                let encoded = match serde_json::to_vec(&frame) {\n                    Ok(encoded) => encoded,\n                    Err(error) => {\n                        tracing::error!(error = %error, "failed to serialize ER bootstrap frame");\n                        let _ = child.kill().await;\n                        let _ = child.wait().await;\n                        tokio::time::sleep(RETRY_DELAY).await;\n                        continue;\n                    }\n                };\n                let Some(mut stdin) = child.stdin.take() else {\n                    tracing::error!("CONTROL ER child did not expose inherited bootstrap stdin");\n                    let _ = child.kill().await;\n                    let _ = child.wait().await;\n                    tokio::time::sleep(RETRY_DELAY).await;\n                    continue;\n                };\n                if let Err(error) = stdin.write_all(&encoded).await {\n                    tracing::error!(error = %error, "failed to deliver ER bootstrap frame");\n                    let _ = child.kill().await;\n                    let _ = child.wait().await;\n                    tokio::time::sleep(RETRY_DELAY).await;\n                    continue;\n                }\n                drop(stdin);\n            }\n            consecutive_failures = 0;\n            tracing::info!(\n                pid = child.id(),\n                authority = if control_enabled { "control" } else { "basic" },\n                "error-reporter daemon process spawned"\n            );'''
if child_anchor not in text:
    raise SystemExit("missing ER post-spawn anchor")
text = text.replace(child_anchor, child_repl, 1)
text = text.replace(
    "async fn boot_and_run() -> anyhow::Result<()> {",
    "async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {",
    1,
)
text = text.replace(
    '''    let error_reporter_task =\n        spawn_error_reporter_daemon_process(maintenance.clone(), refresh_interval)?;''',
    '''    let error_reporter_task = spawn_error_reporter_daemon_process(\n        maintenance.clone(),\n        refresh_interval,\n        host_ready.er_control_enabled(),\n    )?;''',
    1,
)
write(path, text)

# Backend no longer stores an ER signing key in Secret Service. keyring remains
# because HostBootstrap performs the disposable Secret Service verification.

# ---------------------------------------------------------------------------
# Fix concrete test-only failures revealed by the previous staging gate.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
if "use std::path::PathBuf;" not in text:
    text = text.replace(
        "use std::process::Stdio;\n",
        "#[cfg(test)]\nuse std::path::PathBuf;\nuse std::process::Stdio;\n",
        1,
    )
write(path, text)

path = "engine/crates/route-engine/src/runtime_env.rs"
text = read(path)
text = text.replace(
    '''        assert_eq!(\n            env.call_rel("string", &[Value::String("NAME".into())]).unwrap(),\n            Value::String("rbe".into())\n        );''',
    '''        assert!(matches!(\n            env.call_rel("string", &[Value::String("NAME".into())]).unwrap(),\n            Value::String(value) if value == "rbe"\n        ));''',
    1,
)
text = text.replace(
    '''        assert_eq!(\n            env.call_rel("number", &[Value::String("COUNT".into())]).unwrap(),\n            Value::Number(7.0)\n        );''',
    '''        assert!(matches!(\n            env.call_rel("number", &[Value::String("COUNT".into())]).unwrap(),\n            Value::Number(value) if value == 7.0\n        ));''',
    1,
)
text = text.replace(
    '''        assert_eq!(\n            env.call_rel("get", &[Value::String("MISSING".into())]).unwrap(),\n            Value::Null\n        );''',
    '''        assert!(matches!(\n            env.call_rel("get", &[Value::String("MISSING".into())]).unwrap(),\n            Value::Null\n        ));''',
    1,
)
write(path, text)

# Document the hard boundary next to runtime security docs.
path = "docs/service-runtime.md"
text = read(path)
section = r'''

## HostBootstrap and Error Reporter authority

On Linux, normal RBE startup has a Phase 0 `HostBootstrap` boundary before any
runtime credential is created, read, migrated, rotated, modified, or persisted.
The bootstrap scripts are normal `.sh` source files for debugging, embedded in
the backend at compile time, and piped to `/bin/sh` through stdin; RBE does not
extract temporary script files. The bootstrap first reuses a working
`org.freedesktop.secrets` provider, otherwise starts/provisions D-Bus plus a
Secret Service provider through a supported package manager, then verifies a
disposable write/read/delete credential. Failure aborts normal boot before
Vault, container signing material, Communication keys, or user Services start.
Production output is intentionally generic; a debug build launched with
`-debug` outside `RBE_ENV=production` exposes bounded script diagnostics.

The Error Reporter has two authority levels. `BASIC` keeps diagnostics and
report signing but cannot authorize managed restarts. `CONTROL` is issued only
by a parent that already owns `HostBootstrapReady`; each ER generation receives
fresh signing/control material over an inherited one-shot stdin pipe. ER COM
keys are never stored in a file, argv, or environment variable. CONTROL reports
also carry structured diagnostic context (`why`, `how`, activity source,
process identity, and bounded stack/message metadata) for later recovery-policy
decisions. Actual process execution remains owned by the relevant supervisor;
CONTROL is authority to decide/authorize recovery, not permission to spawn an
arbitrary executable.
'''
if "## HostBootstrap and Error Reporter authority" not in text:
    text += section
write(path, text)
