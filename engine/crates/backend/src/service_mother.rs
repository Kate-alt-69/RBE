use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use service_runtime::{
    new_service_mother_token, ServiceManager, ServiceMotherReady, ServiceMotherServer,
};

const MOTHER_RESTART_BASE_DELAY: Duration = Duration::from_millis(250);
const MOTHER_RESTART_MAX_DELAY: Duration = Duration::from_secs(30);
const MOTHER_STABLE_WINDOW: Duration = Duration::from_secs(60);
const MOTHER_READY_MAX_BYTES: usize = 4 * 1024;
const MOTHER_STDOUT_LINE_MAX_BYTES: usize = 64 * 1024;

pub struct ServiceMotherProcess {
    manager: ServiceManager,
    child: Child,
    _liveness: ChildStdin,
    pid: u32,
    started_at: Instant,
}

pub struct ServiceMotherSupervisor {
    manager: ServiceManager,
    shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    task: tokio::task::JoinHandle<()>,
}

impl ServiceMotherSupervisor {
    pub fn manager(&self) -> ServiceManager {
        self.manager.clone()
    }

    pub async fn shutdown(mut self, timeout: Duration) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(timeout);
        }
        match tokio::time::timeout(timeout, &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "Service Mother supervisor task failed")
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "Service Mother supervisor exceeded shutdown budget; aborting task"
                );
                self.task.abort();
                let _ = (&mut self.task).await;
            }
        }
    }
}

impl ServiceMotherProcess {
    pub fn manager(&self) -> ServiceManager {
        self.manager.clone()
    }

    pub async fn shutdown(mut self, timeout: Duration) {
        let started = tokio::time::Instant::now();
        if tokio::time::timeout(timeout, self.manager.shutdown_all())
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "Service Mother shutdown RPC exceeded graceful shutdown budget"
            );
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
            return;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, self.child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(%status, "Service Mother exited after shutdown");
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed waiting for Service Mother shutdown");
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "Service Mother exceeded graceful shutdown budget; terminating process"
                );
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
    }
}

pub async fn run_child(args: &[String]) -> anyhow::Result<()> {
    let expected_runtime_digest = flag_value(args, "--service-runtime-digest")
        .ok_or_else(|| anyhow::anyhow!("Service Mother requires parent runtime image digest"))?;
    let current_exe = std::env::current_exe().context("resolve Service Mother executable")?;
    let actual_runtime_digest = file_sha256_hex(&current_exe)?;
    if !expected_runtime_digest.eq_ignore_ascii_case(&actual_runtime_digest) {
        anyhow::bail!(
            "Service Mother executable digest mismatch; refusing unverified service runtime"
        );
    }
    let token = service_runtime::read_parent_bootstrap_secret_if_configured("Service Mother")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Service Mother requires inherited parent authentication; command-line tokens are not accepted"
            )
        })?;
    let runtime_env_frame = if args.iter().any(|arg| arg == "--runtime-env-frame") {
        service_runtime::read_parent_bootstrap_json_if_configured("Service Mother Runtime ENV")?
            .ok_or_else(|| {
                anyhow::anyhow!("Service Mother Runtime ENV frame requires inherited bootstrap")
            })?
            .into()
    } else {
        None
    };
    let er_recovery_key = if args.iter().any(|arg| arg == "--er-recovery-frame") {
        let value = service_runtime::read_parent_bootstrap_json_if_configured(
            "Service Mother ER recovery",
        )?
        .ok_or_else(|| {
            anyhow::anyhow!("Service Mother ER recovery frame requires inherited bootstrap")
        })?;
        let frame: crate::er_recovery::RecoveryBootstrapFrame = serde_json::from_value(value)?;
        Some(frame.into_key()?)
    } else {
        None
    };
    if !args
        .iter()
        .any(|arg| arg == "--launch-separate" || arg == "--launch-saperate")
    {
        anyhow::bail!("service --service-mother requires --launch-separate");
    }

    let settings_path = flag_value(args, "--settings").unwrap_or_else(|| "settings.json".into());
    let config = config::Config::load(&settings_path).map_err(|error| {
        anyhow::anyhow!("Service Mother failed to load {settings_path}: {error}")
    })?;
    let runtime_env = Arc::new(match runtime_env_frame {
        Some(value) => value,
        None => serde_json::to_value(&config.runtime_env)?,
    });
    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile(&config.services, &io)?;
    let actual_fingerprint = catalog
        .as_ref()
        .map(|catalog| catalog.fingerprint())
        .unwrap_or_default();
    let expected_fingerprint = flag_value(args, "--service-catalog-fingerprint");
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_some() && expected_fingerprint.is_none() {
        anyhow::bail!("Service Mother requires the parent service catalog fingerprint");
    }
    if let Some(expected) = expected_fingerprint {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("Service Mother parent catalog fingerprint is malformed");
        }
        if !expected.eq_ignore_ascii_case(&actual_fingerprint) {
            anyhow::bail!(
                "Service Mother catalog changed after parent validation (expected {expected}, compiled {actual_fingerprint})"
            );
        }
    }
    let server = ServiceMotherServer::bind(token).await?;
    let ready = server.ready().clone();
    let manager = match catalog.as_ref() {
        Some(catalog) => {
            let restart_authority = er_recovery_key.clone().map(|key| {
                Arc::new(crate::er_recovery::ErRecoveryClient::new(key))
                    as Arc<dyn service_runtime::ServiceRestartAuthority>
            });
            ServiceManager::prepare_all_with_fabric_runtime_env_and_restart_authority(
                catalog,
                server.fabric_endpoint(),
                runtime_env.clone(),
                restart_authority,
            )
            .await
        }
        None => ServiceManager::default(),
    };
    // Serve Fabric RPC before running Service.start() so lifecycle hooks can
    // call dependencies. Parent readiness remains withheld until starts pass.
    let server_manager = manager.clone();
    let mut server_task = tokio::spawn(async move { server.serve(server_manager).await });
    if let Some(catalog) = catalog.as_ref() {
        if let Err(error) = manager.start_prepared(catalog).await {
            server_task.abort();
            let _ = (&mut server_task).await;
            return Err(error);
        }
    }
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    tracing::info!(
        pid = std::process::id(),
        address = %ready.address,
        services = catalog
            .as_ref()
            .map(|catalog| catalog.services().len())
            .unwrap_or(0),
        "Service Mother runtime ready"
    );
    let control_manager = manager.clone();
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
}

fn harden_service_mother_environment(command: &mut Command, settings_path: &Path) {
    command.env_clear();
    for name in [
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "RUST_LOG",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("RBE_TRUSTED_SETTINGS_PATH", settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1");
}

fn service_executable_name() -> &'static str {
    if cfg!(windows) {
        "service.exe"
    } else {
        "service"
    }
}

fn file_sha256_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read runtime executable {}", path.display()))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn ensure_canonical_service_executable(backend: &Path, parent: &Path) -> anyhow::Result<PathBuf> {
    let service = parent.join(service_executable_name());
    let expected = file_sha256_hex(backend)?;
    let valid_existing = service.is_file()
        && file_sha256_hex(&service)
            .map(|actual| actual.eq_ignore_ascii_case(&expected))
            .unwrap_or(false);
    if !valid_existing {
        if service.exists() {
            std::fs::remove_file(&service).with_context(|| {
                format!(
                    "replace stale service runtime {}; stop stale service.exe processes first",
                    service.display()
                )
            })?;
        }
        if std::fs::hard_link(backend, &service).is_err() {
            std::fs::copy(backend, &service).with_context(|| {
                format!(
                    "materialize canonical service runtime {}",
                    service.display()
                )
            })?;
        }
    }
    let actual = file_sha256_hex(&service)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        anyhow::bail!(
            "canonical service runtime {} does not match backend executable bytes",
            service.display()
        );
    }
    Ok(service)
}

async fn spawn_process(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    runtime_env: &serde_json::Value,
    er_control_key: Option<&crate::host_bootstrap::ErControlKey>,
    existing_manager: Option<&ServiceManager>,
) -> anyhow::Result<ServiceMotherProcess> {
    if expected_catalog_fingerprint.len() != 64
        || !expected_catalog_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("Service Mother expected catalog fingerprint is malformed");
    }
    let exe = std::env::current_exe().context("resolve backend executable for Service Mother")?;
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backend executable has no parent directory"))?;
    let service_exe = ensure_canonical_service_executable(&exe, parent)?;

    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
        format!(
            "canonicalize Service Mother settings path {}",
            settings_path.as_ref().display()
        )
    })?;
    let token = new_service_mother_token();
    let mut command = Command::new(&service_exe);
    harden_service_mother_environment(&mut command, &settings_path);
    command.args(["--service-mother", "--launch-separate"]);
    if er_control_key.is_some() {
        command.arg("--er-recovery-frame");
    }
    let mut child = match command
        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .arg("--service-runtime-digest")
        .arg(file_sha256_hex(&service_exe)?)
        .arg("--settings")
        .arg(&settings_path)
        .arg("--runtime-env-frame")
        .current_dir(parent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Err(error.into());
        }
    };

    let mut liveness = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            cleanup_failed_spawn(&mut child).await;
            anyhow::bail!("Service Mother parent liveness pipe unavailable");
        }
    };
    if let Err(error) = service_runtime::write_parent_bootstrap_secret(&mut liveness, &token).await
    {
        cleanup_failed_spawn(&mut child).await;
        return Err(anyhow::anyhow!(
            "send Service Mother parent bootstrap secret: {error}"
        ));
    }
    if let Err(error) =
        service_runtime::write_parent_bootstrap_json(&mut liveness, runtime_env).await
    {
        cleanup_failed_spawn(&mut child).await;
        return Err(anyhow::anyhow!(
            "send Service Mother Runtime ENV snapshot: {error}"
        ));
    }
    if let Some(er_control_key) = er_control_key {
        let frame = serde_json::to_value(crate::er_recovery::RecoveryBootstrapFrame::from_key(
            er_control_key,
        ))?;
        if let Err(error) =
            service_runtime::write_parent_bootstrap_json(&mut liveness, &frame).await
        {
            cleanup_failed_spawn(&mut child).await;
            return Err(anyhow::anyhow!(
                "send Service Mother ER recovery capability: {error}"
            ));
        }
    }
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            cleanup_failed_spawn(&mut child).await;
            anyhow::bail!("Service Mother stdout unavailable");
        }
    };
    let mut reader = BufReader::new(stdout);
    let line = match tokio::time::timeout(
        Duration::from_secs(30),
        read_bounded_buffered_line(
            &mut reader,
            MOTHER_READY_MAX_BYTES,
            "Service Mother readiness",
        ),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => {
            cleanup_failed_spawn(&mut child).await;
            anyhow::bail!("Service Mother exited before readiness");
        }
        Ok(Err(error)) => {
            cleanup_failed_spawn(&mut child).await;
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_spawn(&mut child).await;
            anyhow::bail!("Service Mother readiness timed out");
        }
    };
    let ready: ServiceMotherReady = match serde_json::from_str(line.trim()) {
        Ok(ready) => ready,
        Err(error) => {
            cleanup_failed_spawn(&mut child).await;
            return Err(error.into());
        }
    };
    if !ready.address.ip().is_loopback() {
        cleanup_failed_spawn(&mut child).await;
        anyhow::bail!("Service Mother advertised a non-loopback endpoint");
    }
    if child.id() != Some(ready.pid) {
        cleanup_failed_spawn(&mut child).await;
        anyhow::bail!("Service Mother readiness PID does not match child process");
    }

    tokio::spawn(async move {
        loop {
            match read_service_mother_stdout_line(&mut reader, MOTHER_STDOUT_LINE_MAX_BYTES).await {
                Ok(None) => return,
                Ok(Some(ServiceMotherStdoutLine::Line(line))) => {
                    let output = line.trim_end();
                    if !output.is_empty() {
                        tracing::info!(%output, "Service Mother stdout");
                    }
                }
                Ok(Some(ServiceMotherStdoutLine::Oversized)) => tracing::warn!(
                    max_bytes = MOTHER_STDOUT_LINE_MAX_BYTES,
                    "discarded oversized Service Mother stdout line"
                ),
                Ok(Some(ServiceMotherStdoutLine::InvalidUtf8)) => {
                    tracing::warn!("discarded non-UTF-8 Service Mother stdout line")
                }
                Err(error) => {
                    tracing::warn!(error = %error, "failed to drain Service Mother stdout");
                    return;
                }
            }
        }
    });

    let manager_result = match existing_manager {
        Some(manager) => manager
            .replace_remote(ready.address, token)
            .await
            .map(|()| manager.clone()),
        None => ServiceManager::remote(ready.address, token),
    };
    let manager = match manager_result {
        Ok(manager) => manager,
        Err(error) => {
            cleanup_failed_spawn(&mut child).await;
            return Err(error);
        }
    };
    tracing::info!(
        pid = ready.pid,
        address = %ready.address,
        settings = %settings_path.display(),
        "Service Mother process ready"
    );
    Ok(ServiceMotherProcess {
        manager,
        child,
        _liveness: liveness,
        pid: ready.pid,
        started_at: Instant::now(),
    })
}

pub async fn spawn(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    runtime_env: Arc<serde_json::Value>,
    er_control_key: Option<crate::host_bootstrap::ErControlKey>,
) -> anyhow::Result<ServiceMotherSupervisor> {
    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
        format!(
            "canonicalize Service Mother supervisor settings path {}",
            settings_path.as_ref().display()
        )
    })?;
    let expected_catalog_fingerprint = expected_catalog_fingerprint.to_string();
    let initial = spawn_process(
        &settings_path,
        &expected_catalog_fingerprint,
        runtime_env.as_ref(),
        er_control_key.as_ref(),
        None,
    )
    .await?;
    let manager = initial.manager();
    let supervisor_manager = manager.clone();
    let supervisor_settings = settings_path.clone();
    let supervisor_fingerprint = expected_catalog_fingerprint.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<Duration>();
    let task = tokio::spawn(async move {
        supervise(
            initial,
            supervisor_settings,
            supervisor_fingerprint,
            runtime_env,
            er_control_key,
            supervisor_manager,
            &mut shutdown_rx,
        )
        .await;
    });
    Ok(ServiceMotherSupervisor {
        manager,
        shutdown: Some(shutdown_tx),
        task,
    })
}

async fn supervise(
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
                authority_reason = authority_reason
                    .as_deref()
                    .unwrap_or("local critical-process policy"),
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
                        authority_reason = authority_reason
                            .as_deref()
                            .unwrap_or("local critical-process policy"),
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

fn mother_restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = MOTHER_RESTART_BASE_DELAY
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(
        millis
            .saturating_mul(factor)
            .min(MOTHER_RESTART_MAX_DELAY.as_millis() as u64),
    )
}

async fn read_bounded_buffered_line<R>(
    reader: &mut R,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(1024));
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("{label} is not newline terminated");
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if bytes.len().saturating_add(consumed) > max_bytes {
            reader.consume(consumed);
            anyhow::bail!("{label} exceeded {max_bytes} bytes");
        }
        bytes.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|error| anyhow::anyhow!("{label} is not valid UTF-8: {error}"));
        }
    }
}

enum ServiceMotherStdoutLine {
    Line(String),
    Oversized,
    InvalidUtf8,
}

async fn read_service_mother_stdout_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<ServiceMotherStdoutLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(1024));
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if bytes.is_empty() && !oversized {
                return Ok(None);
            }
            if oversized {
                return Ok(Some(ServiceMotherStdoutLine::Oversized));
            }
            return Ok(Some(match String::from_utf8(bytes) {
                Ok(line) => ServiceMotherStdoutLine::Line(line),
                Err(_) => ServiceMotherStdoutLine::InvalidUtf8,
            }));
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if !oversized {
            if bytes.len().saturating_add(consumed) > max_bytes {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..consumed]);
            }
        }
        reader.consume(consumed);

        if newline.is_some() {
            if oversized {
                return Ok(Some(ServiceMotherStdoutLine::Oversized));
            }
            return Ok(Some(match String::from_utf8(bytes) {
                Ok(line) => ServiceMotherStdoutLine::Line(line),
                Err(_) => ServiceMotherStdoutLine::InvalidUtf8,
            }));
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

async fn cleanup_failed_spawn(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mother_readiness_reader_is_bounded_and_preserves_following_output() {
        let payload = b"{\"pid\":1}\nfirst-log-line\n";
        let mut reader = BufReader::new(&payload[..]);
        let readiness = read_bounded_buffered_line(&mut reader, 64, "readiness")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(readiness, "{\"pid\":1}\n");

        let mut next = String::new();
        reader.read_line(&mut next).await.unwrap();
        assert_eq!(next, "first-log-line\n");

        let oversized = format!("{}\n", "x".repeat(65));
        let mut oversized_reader = BufReader::new(oversized.as_bytes());
        let error = read_bounded_buffered_line(&mut oversized_reader, 64, "readiness")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded 64 bytes"));
    }

    #[tokio::test]
    async fn mother_stdout_reader_discards_oversized_lines_and_keeps_draining() {
        let payload = format!("{}\nnext-line\n", "x".repeat(65));
        let mut reader = BufReader::new(payload.as_bytes());
        assert!(matches!(
            read_service_mother_stdout_line(&mut reader, 64)
                .await
                .unwrap(),
            Some(ServiceMotherStdoutLine::Oversized)
        ));
        match read_service_mother_stdout_line(&mut reader, 64)
            .await
            .unwrap()
        {
            Some(ServiceMotherStdoutLine::Line(line)) => assert_eq!(line, "next-line\n"),
            _ => panic!("stdout reader should continue with the next bounded line"),
        }
    }

    #[test]
    fn catalog_fingerprint_argument_is_fixed_size_hex() {
        let valid = "ab".repeat(32);
        assert_eq!(valid.len(), 64);
        assert!(valid.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!("not-a-fingerprint"
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit()));
    }

    #[test]
    fn mother_restart_backoff_is_exponential_and_capped() {
        assert_eq!(mother_restart_delay(1), Duration::from_millis(250));
        assert_eq!(mother_restart_delay(2), Duration::from_millis(500));
        assert_eq!(mother_restart_delay(3), Duration::from_millis(1000));
        assert_eq!(mother_restart_delay(30), MOTHER_RESTART_MAX_DELAY);
    }

    #[test]
    fn control_er_floor_can_only_delay_mother_recovery() {
        assert_eq!(
            mother_recovery_backoff(1, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            mother_recovery_backoff(6, Duration::from_millis(1)),
            mother_restart_delay(6)
        );
        assert_eq!(
            mother_recovery_backoff(1, Duration::from_secs(300)),
            MOTHER_RESTART_MAX_DELAY
        );
    }
}
