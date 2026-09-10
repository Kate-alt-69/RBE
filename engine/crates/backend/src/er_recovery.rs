use std::path::{Path, PathBuf};
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
            anyhow::bail!("unsupported ER recovery bootstrap version {}", self.version);
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

    async fn decide_inner(
        &self,
        report: ServiceExitReport,
    ) -> anyhow::Result<ServiceRestartDirective> {
        ensure_layout(&self.root)?;
        let request_id = random_request_id();
        let created_at_ms = now_unix_ms();
        let mac = request_mac(
            &self.key,
            PROTOCOL_VERSION,
            &request_id,
            created_at_ms,
            &report,
        )?;
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
        let raw = match read_limited(&path, REQUEST_MAX_BYTES) {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let request: SignedRecoveryRequest = match serde_json::from_slice(&raw) {
            Ok(request) => request,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let file_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
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
    let operation = report
        .last_operation
        .as_deref()
        .unwrap_or("idle-or-unknown");
    format!(
        "phase={} operation={} active_calls={} uptime_ms={} idle_for_ms={}",
        report.phase, operation, report.active_calls, report.uptime_ms, report.idle_for_ms
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
    let what_was_doing = report
        .last_operation
        .as_deref()
        .unwrap_or("idle-or-unknown");
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
    let lines = raw
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
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
        | ServiceRestartDirective::Stop { reason }
            if reason.len() > 512 =>
        {
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

fn verify_serialized<T: Serialize>(
    key: &ErControlKey,
    value: &T,
    expected: &str,
) -> anyhow::Result<bool> {
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
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid HMAC key length"))?;
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
                minimum_backoff_ms, ..
            } => assert!(minimum_backoff_ms >= 5_000),
            other => panic!("expected restart directive, got {other:?}"),
        }
    }
}
