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
# Generalize the authenticated CONTROL ER recovery transport so the same
# per-backend-generation capability can evaluate critical runtime processes as
# well as individual .service workers. Payloads remain metadata-only.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/er_recovery.rs"
text = read(path)
text = replace_once(
    text,
    "use std::path::{Path, PathBuf};\n",
    "use std::collections::BTreeMap;\nuse std::path::{Path, PathBuf};\n",
    "er_recovery BTreeMap import",
)
text = replace_once(
    text,
    "const PROTOCOL_VERSION: u8 = 1;",
    "const PROTOCOL_VERSION: u8 = 2;",
    "ER recovery protocol version",
)

client_marker = "#[derive(Clone)]\npub struct ErRecoveryClient {\n"
insert = r'''#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExitReport {
    pub component: String,
    pub process_image: String,
    pub pid: u32,
    pub exit_success: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub previous_restart_attempts: u32,
    pub uptime_ms: u64,
    pub expected: bool,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_error: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
enum RecoverySubject {
    Service(ServiceExitReport),
    CriticalProcess(ProcessExitReport),
}

fn validate_process_report(report: &ProcessExitReport) -> anyhow::Result<()> {
    fn safe(label: &str, value: &str, max: usize) -> anyhow::Result<()> {
        if value.is_empty()
            || value.len() > max
            || value.chars().any(|character| character.is_control() || character == '\0')
        {
            anyhow::bail!("CONTROL ER {label} is invalid or exceeds {max} bytes");
        }
        Ok(())
    }

    safe("component", &report.component, 80)?;
    safe("process image", &report.process_image, 80)?;
    safe("phase", &report.phase, 96)?;
    if let Some(operation) = report.last_operation.as_deref() {
        safe("last operation", operation, 128)?;
    }
    if let Some(error) = report.observation_error.as_deref() {
        safe("observation error", error, 1024)?;
    }
    if report.context.len() > 16 {
        anyhow::bail!("CONTROL ER process context exceeded 16 fields");
    }
    for (name, value) in &report.context {
        safe("context key", name, 64)?;
        safe("context value", value, 256)?;
    }
    Ok(())
}

'''
if client_marker not in text:
    raise SystemExit("missing ErRecoveryClient marker")
text = text.replace(client_marker, insert + client_marker, 1)

text = replace_between(
    text,
    "impl ErRecoveryClient {\n",
    "#[derive(Debug, Serialize, Deserialize)]\nstruct SignedRecoveryRequest",
    r'''impl ErRecoveryClient {
    pub fn new(key: ErControlKey) -> Self {
        Self {
            root: runtime_paths::default_admin_dir().join("er-recovery"),
            key,
        }
    }

    async fn decide_subject(
        &self,
        report: RecoverySubject,
    ) -> anyhow::Result<ServiceRestartDirective> {
        if let RecoverySubject::CriticalProcess(process) = &report {
            validate_process_report(process)?;
        }
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

    pub async fn decide_process(
        &self,
        report: ProcessExitReport,
    ) -> anyhow::Result<ServiceRestartDirective> {
        self.decide_subject(RecoverySubject::CriticalProcess(report)).await
    }
}

impl ServiceRestartAuthority for ErRecoveryClient {
    fn decide<'a>(&'a self, report: ServiceExitReport) -> ServiceRestartAuthorityFuture<'a> {
        Box::pin(async move { self.decide_subject(RecoverySubject::Service(report)).await })
    }
}

''',
    "ErRecoveryClient generalized transport",
)

text = replace_once(
    text,
    "    report: ServiceExitReport,\n    mac: String,\n}\n\n#[derive(Serialize)]\nstruct RequestMacPayload<'a> {\n    version: u8,\n    request_id: &'a str,\n    created_at_ms: u64,\n    report: &'a ServiceExitReport,\n}",
    "    report: RecoverySubject,\n    mac: String,\n}\n\n#[derive(Serialize)]\nstruct RequestMacPayload<'a> {\n    version: u8,\n    request_id: &'a str,\n    created_at_ms: u64,\n    report: &'a RecoverySubject,\n}",
    "signed recovery request subject type",
)

validation_anchor = '''        if file_id != request.request_id\n            || !valid_request_id(&request.request_id)\n            || !fresh_timestamp(request.created_at_ms)\n            || request.version != PROTOCOL_VERSION\n            || !verify_request_mac(key, &request)?\n        {\n            let _ = std::fs::remove_file(&path);\n            continue;\n        }\n\n        let directive = compute_directive(&request.report);'''
validation_replacement = '''        if file_id != request.request_id\n            || !valid_request_id(&request.request_id)\n            || !fresh_timestamp(request.created_at_ms)\n            || request.version != PROTOCOL_VERSION\n            || !verify_request_mac(key, &request)?\n        {\n            let _ = std::fs::remove_file(&path);\n            continue;\n        }\n        if let RecoverySubject::CriticalProcess(report) = &request.report {\n            if validate_process_report(report).is_err() {\n                let _ = std::fs::remove_file(&path);\n                continue;\n            }\n        }\n\n        let directive = compute_directive(&request.report);'''
text = replace_once(text, validation_anchor, validation_replacement, "server process report validation")

text = replace_between(
    text,
    "fn compute_directive(",
    "fn response_mac(",
    r'''fn compute_directive(report: &RecoverySubject) -> ServiceRestartDirective {
    match report {
        RecoverySubject::Service(report) => compute_service_directive(report),
        RecoverySubject::CriticalProcess(report) => compute_process_directive(report),
    }
}

fn compute_service_directive(report: &ServiceExitReport) -> ServiceRestartDirective {
    let why = explain_status(
        report.exit_success,
        report.exit_code,
        report.exit_signal,
        None,
    );
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

fn compute_process_directive(report: &ProcessExitReport) -> ServiceRestartDirective {
    let why = explain_status(
        report.exit_success,
        report.exit_code,
        report.exit_signal,
        report.observation_error.as_deref(),
    );
    if report.expected {
        return ServiceRestartDirective::Stop {
            reason: format!("planned {} exit: {why}", report.component),
        };
    }
    let crash_loop_floor = if report.previous_restart_attempts >= 6 {
        5_000
    } else {
        0
    };
    ServiceRestartDirective::Restart {
        minimum_backoff_ms: crash_loop_floor,
        reason: format!(
            "CONTROL ER authorized critical-process recovery for {}: {why}",
            report.component
        ),
    }
}

fn explain_status(
    exit_success: bool,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    observation_error: Option<&str>,
) -> String {
    if let Some(error) = observation_error {
        return format!("supervisor could not observe a normal exit status: {error}");
    }
    if let Some(signal) = exit_signal {
        return format!("terminated by signal {signal}");
    }
    if let Some(code) = exit_code {
        return format!("exited with code {code}");
    }
    if exit_success {
        "exited successfully".into()
    } else {
        "process exited without a portable code or signal".into()
    }
}

fn explain_exit(report: &RecoverySubject) -> String {
    match report {
        RecoverySubject::Service(report) => explain_status(
            report.exit_success,
            report.exit_code,
            report.exit_signal,
            None,
        ),
        RecoverySubject::CriticalProcess(report) => explain_status(
            report.exit_success,
            report.exit_code,
            report.exit_signal,
            report.observation_error.as_deref(),
        ),
    }
}

fn operation_label(report: &RecoverySubject) -> &str {
    match report {
        RecoverySubject::Service(report) => report
            .last_operation
            .as_deref()
            .unwrap_or("idle-or-unknown"),
        RecoverySubject::CriticalProcess(report) => report
            .last_operation
            .as_deref()
            .unwrap_or("idle-or-unknown"),
    }
}

fn explain_activity(report: &RecoverySubject) -> String {
    match report {
        RecoverySubject::Service(report) => format!(
            "phase={} operation={} active_calls={} uptime_ms={} idle_for_ms={}",
            report.phase,
            operation_label(&RecoverySubject::Service(report.clone())),
            report.active_calls,
            report.uptime_ms,
            report.idle_for_ms
        ),
        RecoverySubject::CriticalProcess(report) => format!(
            "phase={} operation={} uptime_ms={} previous_restart_attempts={} context_fields={}",
            report.phase,
            report
                .last_operation
                .as_deref()
                .unwrap_or("idle-or-unknown"),
            report.uptime_ms,
            report.previous_restart_attempts,
            report.context.len()
        ),
    }
}

#[derive(Serialize)]
struct DecisionLogPayload<'a> {
    recorded_at_ms: u64,
    why: &'a str,
    how: &'a str,
    what_was_doing: &'a str,
    report: &'a RecoverySubject,
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
    report: &RecoverySubject,
    directive: &ServiceRestartDirective,
    why: &str,
    how: &str,
    report_signing_key: &str,
) -> anyhow::Result<()> {
    let what_was_doing = operation_label(report);
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
    report: &RecoverySubject,
) -> anyhow::Result<String> {
    let payload = RequestMacPayload {
        version,
        request_id,
        created_at_ms,
        report,
    };
    hmac_serialized(key, &payload)
}

''',
    "generic recovery decision and diagnostic block",
)

# Keep request authentication bound to the generalized tagged report.
text = replace_once(
    text,
    "        report: &request.report,\n",
    "        report: &request.report,\n",
    "request MAC report anchor",
)

# Update the existing tests to wrap service reports in the tagged transport and
# add an explicit critical-process policy test.
test_anchor = '''    #[test]\n    fn decision_respects_service_restart_policy() {\n        assert!(matches!(\n            compute_directive(&report(RestartPolicy::Never, false, 0)),\n            ServiceRestartDirective::Stop { .. }\n        ));\n        assert!(matches!(\n            compute_directive(&report(RestartPolicy::OnFailure, true, 0)),\n            ServiceRestartDirective::Stop { .. }\n        ));\n        assert!(matches!(\n            compute_directive(&report(RestartPolicy::OnFailure, false, 0)),\n            ServiceRestartDirective::Restart { .. }\n        ));\n    }\n\n    #[test]\n    fn crash_loop_gets_a_control_backoff_floor() {\n        match compute_directive(&report(RestartPolicy::Always, false, 6)) {\n            ServiceRestartDirective::Restart {\n                minimum_backoff_ms, ..\n            } => assert!(minimum_backoff_ms >= 5_000),\n            other => panic!(\"expected restart directive, got {other:?}\"),\n        }\n    }'''
test_replacement = '''    fn service_subject(\n        restart: RestartPolicy,\n        success: bool,\n        attempts: u32,\n    ) -> RecoverySubject {\n        RecoverySubject::Service(report(restart, success, attempts))\n    }\n\n    #[test]\n    fn decision_respects_service_restart_policy() {\n        assert!(matches!(\n            compute_directive(&service_subject(RestartPolicy::Never, false, 0)),\n            ServiceRestartDirective::Stop { .. }\n        ));\n        assert!(matches!(\n            compute_directive(&service_subject(RestartPolicy::OnFailure, true, 0)),\n            ServiceRestartDirective::Stop { .. }\n        ));\n        assert!(matches!(\n            compute_directive(&service_subject(RestartPolicy::OnFailure, false, 0)),\n            ServiceRestartDirective::Restart { .. }\n        ));\n    }\n\n    #[test]\n    fn crash_loop_gets_a_control_backoff_floor() {\n        match compute_directive(&service_subject(RestartPolicy::Always, false, 6)) {\n            ServiceRestartDirective::Restart {\n                minimum_backoff_ms, ..\n            } => assert!(minimum_backoff_ms >= 5_000),\n            other => panic!(\"expected restart directive, got {other:?}\"),\n        }\n    }\n\n    #[test]\n    fn unexpected_critical_process_exit_is_always_recoverable() {\n        let process = ProcessExitReport {\n            component: \"service-mother\".into(),\n            process_image: \"service\".into(),\n            pid: 77,\n            exit_success: true,\n            exit_code: Some(0),\n            exit_signal: None,\n            previous_restart_attempts: 6,\n            uptime_ms: 500,\n            expected: false,\n            phase: \"runtime-supervision\".into(),\n            last_operation: Some(\"serve-fabric-and-supervise-services\".into()),\n            observation_error: None,\n            context: BTreeMap::new(),\n        };\n        match compute_directive(&RecoverySubject::CriticalProcess(process)) {\n            ServiceRestartDirective::Restart {\n                minimum_backoff_ms, ..\n            } => assert!(minimum_backoff_ms >= 5_000),\n            other => panic!(\"expected critical-process restart directive, got {other:?}\"),\n        }\n    }'''
text = replace_once(text, test_anchor, test_replacement, "ER recovery tests")
write(path, text)


# ---------------------------------------------------------------------------
# Service Mother is a critical process: unexpected exit metadata is evaluated
# by CONTROL ER, but local supervision remains the fail-safe and ER cannot turn
# a bad decision into a permanent loss of the service tree.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
text = replace_once(
    text,
    "use std::io::Write;\n",
    "use std::collections::BTreeMap;\nuse std::io::Write;\n",
    "Service Mother BTreeMap import",
)
text = replace_once(
    text,
    "use std::process::Stdio;\n",
    "use std::process::{ExitStatus, Stdio};\n",
    "Service Mother ExitStatus import",
)
text = replace_once(
    text,
    '''pub struct ServiceMotherProcess {\n    manager: ServiceManager,\n    child: Child,\n    _liveness: ChildStdin,\n    started_at: Instant,\n}''',
    '''pub struct ServiceMotherProcess {\n    manager: ServiceManager,\n    child: Child,\n    _liveness: ChildStdin,\n    pid: u32,\n    started_at: Instant,\n}''',
    "ServiceMotherProcess pid",
)
text = replace_once(
    text,
    '''    Ok(ServiceMotherProcess {\n        manager,\n        child,\n        _liveness: liveness,\n        started_at: Instant::now(),\n    })''',
    '''    Ok(ServiceMotherProcess {\n        manager,\n        child,\n        _liveness: liveness,\n        pid: ready.pid,\n        started_at: Instant::now(),\n    })''',
    "ServiceMotherProcess construction",
)

text = replace_between(
    text,
    "async fn supervise(\n",
    "fn mother_restart_delay(",
    r'''async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    expected_catalog_fingerprint: String,
    runtime_env: Arc<serde_json::Value>,
    er_control_key: Option<crate::host_bootstrap::ErControlKey>,
    manager: ServiceManager,
    shutdown_rx: &mut tokio::sync::oneshot::Receiver<Duration>,
) {
    let mut restart_attempts = 0u32;
    loop {
        let (authority_minimum_backoff, authority_reason) = tokio::select! {
            shutdown = &mut *shutdown_rx => {
                let timeout = shutdown.unwrap_or(Duration::from_secs(5));
                process.shutdown(timeout).await;
                return;
            }
            status = process.child.wait() => {
                let uptime = process.started_at.elapsed();
                let pid = process.pid;
                manager.invalidate_remote().await;
                match &status {
                    Ok(status) => tracing::warn!(
                        %status,
                        pid,
                        uptime_ms = uptime.as_millis(),
                        "Service Mother exited; supervising replacement"
                    ),
                    Err(error) => tracing::warn!(
                        error = %error,
                        pid,
                        uptime_ms = uptime.as_millis(),
                        "failed watching Service Mother; supervising replacement"
                    ),
                }

                let report = service_mother_exit_report(
                    pid,
                    &status,
                    uptime,
                    restart_attempts,
                    &expected_catalog_fingerprint,
                    runtime_env.as_ref(),
                );
                let decision = decide_mother_recovery(er_control_key.as_ref(), report).await;
                if uptime >= MOTHER_STABLE_WINDOW {
                    restart_attempts = 0;
                }
                decision
            }
        };

        loop {
            restart_attempts = restart_attempts.saturating_add(1);
            let delay = mother_recovery_backoff(restart_attempts, authority_minimum_backoff);
            tracing::warn!(
                attempt = restart_attempts,
                backoff_ms = delay.as_millis(),
                authority_reason = authority_reason.as_deref().unwrap_or("local critical-process policy"),
                "scheduling Service Mother replacement"
            );
            tokio::select! {
                shutdown = &mut *shutdown_rx => {
                    let _ = shutdown;
                    return;
                }
                _ = tokio::time::sleep(delay) => {}
            }

            match spawn_process(
                &settings_path,
                &expected_catalog_fingerprint,
                runtime_env.as_ref(),
                er_control_key.as_ref(),
                Some(&manager),
            )
            .await
            {
                Ok(replacement) => {
                    tracing::info!(
                        attempt = restart_attempts,
                        authority_reason = authority_reason.as_deref().unwrap_or("local critical-process policy"),
                        "Service Mother replacement ready; shared service endpoint retargeted"
                    );
                    process = replacement;
                    break;
                }
                Err(error) => tracing::error!(
                    attempt = restart_attempts,
                    error = %error,
                    authority_reason = authority_reason.as_deref().unwrap_or("local critical-process policy"),
                    "Service Mother replacement failed"
                ),
            }
        }
    }
}

fn service_mother_exit_report(
    pid: u32,
    status: &std::io::Result<ExitStatus>,
    uptime: Duration,
    previous_restart_attempts: u32,
    expected_catalog_fingerprint: &str,
    runtime_env: &serde_json::Value,
) -> crate::er_recovery::ProcessExitReport {
    let (exit_success, exit_code, exit_signal, observation_error) = match status {
        Ok(status) => (
            status.success(),
            status.code(),
            process_exit_signal(status),
            None,
        ),
        Err(error) => (
            false,
            None,
            None,
            Some(bounded_diagnostic(&error.to_string(), 1024)),
        ),
    };
    let mut context = BTreeMap::new();
    context.insert(
        "catalog_fingerprint".into(),
        expected_catalog_fingerprint.to_string(),
    );
    context.insert(
        "runtime_env_key_count".into(),
        runtime_env
            .as_object()
            .map(|fields| fields.len())
            .unwrap_or(0)
            .to_string(),
    );
    context.insert("supervision_scope".into(), "service-tree-root".into());

    crate::er_recovery::ProcessExitReport {
        component: "service-mother".into(),
        process_image: service_executable_name().into(),
        pid,
        exit_success,
        exit_code,
        exit_signal,
        previous_restart_attempts,
        uptime_ms: uptime.as_millis().min(u128::from(u64::MAX)) as u64,
        expected: false,
        phase: "runtime-supervision".into(),
        last_operation: Some("serve-fabric-and-supervise-services".into()),
        observation_error,
        context,
    }
}

async fn decide_mother_recovery(
    key: Option<&crate::host_bootstrap::ErControlKey>,
    report: crate::er_recovery::ProcessExitReport,
) -> (Duration, Option<String>) {
    let Some(key) = key else {
        return (Duration::ZERO, None);
    };
    let client = crate::er_recovery::ErRecoveryClient::new(key.clone());
    match tokio::time::timeout(Duration::from_millis(700), client.decide_process(report)).await {
        Ok(Ok(service_runtime::ServiceRestartDirective::Restart {
            minimum_backoff_ms,
            reason,
        })) => (
            Duration::from_millis(minimum_backoff_ms).min(MOTHER_RESTART_MAX_DELAY),
            Some(reason),
        ),
        Ok(Ok(service_runtime::ServiceRestartDirective::Default)) => (Duration::ZERO, None),
        Ok(Ok(service_runtime::ServiceRestartDirective::Stop { reason })) => {
            tracing::error!(
                authority_reason = %reason,
                "CONTROL ER requested Stop for unexpected critical Service Mother exit; ignoring unsafe stop directive"
            );
            (
                Duration::ZERO,
                Some(format!("unsafe CONTROL ER stop ignored: {reason}")),
            )
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "CONTROL ER Service Mother decision failed; using local critical-process policy"
            );
            (Duration::ZERO, None)
        }
        Err(_) => {
            tracing::warn!(
                "CONTROL ER Service Mother decision timed out; using local critical-process policy"
            );
            (Duration::ZERO, None)
        }
    }
}

fn mother_recovery_backoff(attempt: u32, authority_minimum: Duration) -> Duration {
    mother_restart_delay(attempt)
        .max(authority_minimum)
        .min(MOTHER_RESTART_MAX_DELAY)
}

fn bounded_diagnostic(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value
            .chars()
            .filter(|character| !character.is_control() && *character != '\0')
            .collect();
    }
    value
        .chars()
        .filter(|character| !character.is_control() && *character != '\0')
        .scan(0usize, |used, character| {
            let next = used.saturating_add(character.len_utf8());
            if next > max_bytes {
                None
            } else {
                *used = next;
                Some(character)
            }
        })
        .collect()
}

#[cfg(unix)]
fn process_exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn process_exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

''',
    "Service Mother CONTROL ER supervision",
)

mother_test_anchor = '''    #[test]\n    fn mother_restart_backoff_is_exponential_and_capped() {\n        assert_eq!(mother_restart_delay(1), Duration::from_millis(250));\n        assert_eq!(mother_restart_delay(2), Duration::from_millis(500));\n        assert_eq!(mother_restart_delay(3), Duration::from_millis(1000));\n        assert_eq!(mother_restart_delay(30), MOTHER_RESTART_MAX_DELAY);\n    }'''
mother_test_replacement = '''    #[test]\n    fn mother_restart_backoff_is_exponential_and_capped() {\n        assert_eq!(mother_restart_delay(1), Duration::from_millis(250));\n        assert_eq!(mother_restart_delay(2), Duration::from_millis(500));\n        assert_eq!(mother_restart_delay(3), Duration::from_millis(1000));\n        assert_eq!(mother_restart_delay(30), MOTHER_RESTART_MAX_DELAY);\n    }\n\n    #[test]\n    fn control_er_floor_can_only_delay_mother_recovery() {\n        assert_eq!(\n            mother_recovery_backoff(1, Duration::from_secs(5)),\n            Duration::from_secs(5)\n        );\n        assert_eq!(\n            mother_recovery_backoff(6, Duration::from_millis(1)),\n            mother_restart_delay(6)\n        );\n        assert_eq!(\n            mother_recovery_backoff(1, Duration::from_secs(300)),\n            MOTHER_RESTART_MAX_DELAY\n        );\n    }'''
text = replace_once(text, mother_test_anchor, mother_test_replacement, "Service Mother recovery tests")
write(path, text)


# ---------------------------------------------------------------------------
# Expose how many authenticated CONTROL recovery decisions ER has processed in
# its own status snapshot. This is diagnostic metadata only.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/error_reporter_daemon.rs"
text = read(path)
text = replace_once(
    text,
    '''    duplicate_count: u64,\n    dropped_count: u64,\n    last_error_message: Option<String>,''',
    '''    duplicate_count: u64,\n    dropped_count: u64,\n    recovery_decision_count: u64,\n    last_error_message: Option<String>,''',
    "ER status recovery counter field",
)
text = replace_once(
    text,
    '''    let mut duplicate_count = 0u64;\n    let mut dropped_count = 0u64;\n    let mut last_error_message: Option<String> = None;''',
    '''    let mut duplicate_count = 0u64;\n    let mut dropped_count = 0u64;\n    let mut recovery_decision_count = 0u64;\n    let mut last_error_message: Option<String> = None;''',
    "ER recovery counter initialization",
)
old_recovery = '''                    if let Err(error) = crate::er_recovery::process_pending_requests(\n                        &io,\n                        &admin_dir,\n                        key,\n                        signing_key,\n                    ) {\n                        last_error_message = Some(format!(\n                            \"CONTROL ER recovery queue failed: {error}\"\n                        ));\n                    }'''
new_recovery = '''                    match crate::er_recovery::process_pending_requests(\n                        &io,\n                        &admin_dir,\n                        key,\n                        signing_key,\n                    ) {\n                        Ok(processed) => {\n                            recovery_decision_count = recovery_decision_count\n                                .saturating_add(processed as u64);\n                        }\n                        Err(error) => {\n                            last_error_message = Some(format!(\n                                \"CONTROL ER recovery queue failed: {error}\"\n                            ));\n                        }\n                    }'''
text = replace_once(text, old_recovery, new_recovery, "ER recovery counter update")
text = text.replace(
    '''                    dropped_count,\n                    last_error_message: last_error_message.clone(),''',
    '''                    dropped_count,\n                    recovery_decision_count,\n                    last_error_message: last_error_message.clone(),''',
)
text = text.replace(
    '''            dropped_count,\n            last_error_message,''',
    '''            dropped_count,\n            recovery_decision_count,\n            last_error_message,''',
)
write(path, text)


# ---------------------------------------------------------------------------
# Document the critical-process boundary and its fail-safe semantics.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
append = r'''

### CONTROL ER critical-process supervision

CONTROL ER recovery protocol v2 also accepts a bounded `ProcessExitReport` for
critical runtime supervisors. Service Mother is the first critical process wired
to this path. On an unexpected Mother exit the backend records the stable process
identity, PID, portable exit code or Unix signal, supervisor observation error
when one exists, uptime, previous replacement attempts, runtime phase, a fixed
non-sensitive activity label, the service-catalog fingerprint, Runtime ENV key
count, and supervision scope. Runtime ENV values, service call arguments,
credentials, request bodies, database rows, and arbitrary child memory are not
included.

The report is authenticated with the same per-backend-generation CONTROL key as
`.service` recovery requests. ER records `why`, `how`, `what_was_doing`, the full
bounded metadata report, and its signed Restart/Stop/Default decision in
`er-process-decisions.log`; `error-reporter-status.json` exposes a cumulative
`recoveryDecisionCount` for the current ER process generation.

Service Mother remains a critical root. An unexpected Mother exit is locally
restartable even if it returned exit code 0. CONTROL ER may raise the minimum
replacement backoff (for example during a crash loop), but its delay is capped by
the backend's Mother maximum. Missing/BASIC/dead/timed-out ER falls back to the
local Mother supervisor. A `Stop` response for an unexpected critical Mother is
treated as unsafe and ignored, so an ER bug cannot permanently remove the
`.service` tree. Planned backend shutdown bypasses this crash-decision path.
'''
if "### CONTROL ER critical-process supervision" not in text:
    text = text.rstrip() + append + "\n"
write(path, text)
