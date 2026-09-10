//! Backend boot sequence. Vault, container runtime, and user `.service` files
//! are separate supervised OS processes. Video Manager's lightweight control
//! plane lives in-process; heavy media workers remain lazy/separate.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_lib::{AppState, ContainerClient, MaintenanceMetrics};
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod container_process;
#[allow(dead_code)]
mod er_recovery;
mod error_reporter_daemon;
mod host_bootstrap;
mod maintenance_notice;
mod port_guard;
mod runtime_image_boot;
#[allow(dead_code)]
mod service_boot;
#[allow(dead_code)]
mod service_control;
#[allow(dead_code, clippy::too_many_arguments)]
mod service_mother;
mod vault_recovery;

mod service_integrity {
    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if has("--maintenance-notice") {
        let value = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
        };
        let host = value("--maintenance-host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = value("--maintenance-port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8080);
        if let Err(error) = maintenance_notice::run(host, port).await {
            eprintln!("fatal maintenance responder error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--er") {
        if !has("--launch") {
            eprintln!("backend.exe --er requires --launch as well");
            std::process::exit(2);
        }
        let separate = has("--separate-process") || has("--saperate-process");
        let bootstrap = if has("--er-bootstrap-stdin") {
            error_reporter_daemon::ErBootstrap::from_parent_stdin()
        } else {
            Ok(error_reporter_daemon::ErBootstrap::basic())
        };
        let bootstrap = match bootstrap {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                eprintln!("fatal error-reporter bootstrap error: {error:#}");
                std::process::exit(1);
            }
        };
        if let Err(error) = run_error_reporter_daemon(separate, bootstrap).await {
            eprintln!("fatal error-reporter-daemon error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--vault") {
        if let Err(error) = host_bootstrap::evaluate(&args).await {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            std::process::exit(1);
        }
        if !has("--separate-process") && !has("--saperate-process") {
            eprintln!("backend.exe --vault requires --separate-process");
            std::process::exit(2);
        }
        let value = |flag: &str, default: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| default.to_string())
        };
        let service_name = value("--service-name", "backend-rs");
        let data_dir = PathBuf::from(value(
            "--data-dir",
            &runtime_paths::default_admin_dir().to_string_lossy(),
        ));
        let force_dbus = has("--dbus");
        if let Err(error) = vault_process::run_vault_daemon(service_name, data_dir, force_dbus) {
            eprintln!("fatal Vault daemon error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    println!("Evaluating..");
    let host_ready = match host_bootstrap::evaluate(&args).await {
        Ok(ready) => ready,
        Err(error) => {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            std::process::exit(1);
        }
    };

    if let Err(error) = boot_and_run(host_ready).await {
        eprintln!("fatal boot error: {error:#}");
        std::process::exit(1);
    }
}

async fn run_error_reporter_daemon(
    separate_process: bool,
    bootstrap: error_reporter_daemon::ErBootstrap,
) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
    tracing::info!(
        pid = std::process::id(),
        separate_process,
        "backend.exe running in --er (error-reporter-daemon) mode"
    );
    let io = atomic_io::AtomicIo::new();
    let admin_dir = runtime_paths::default_admin_dir();
    error_reporter_daemon::run(io, admin_dir, separate_process, bootstrap).await
}

fn spawn_error_reporter_daemon_process(
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
                        ErSupervisorFailure {
                            phase: "spawn-failed",
                            pid: None,
                            exit_success: None,
                            exit_code: None,
                            exit_signal: None,
                            uptime: Duration::ZERO,
                            observation_error: Some(error.to_string()),
                        },
                        consecutive_failures,
                        authority,
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
                            ErSupervisorFailure {
                                phase: "bootstrap-serialize-failed",
                                pid,
                                exit_success: None,
                                exit_code: None,
                                exit_signal: None,
                                uptime: started_at.elapsed(),
                                observation_error: Some(error.to_string()),
                            },
                            consecutive_failures,
                            authority,
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
                        ErSupervisorFailure {
                            phase: "bootstrap-pipe-unavailable",
                            pid,
                            exit_success: None,
                            exit_code: None,
                            exit_signal: None,
                            uptime: started_at.elapsed(),
                            observation_error: Some(
                                "CONTROL ER child did not expose inherited bootstrap stdin".into(),
                            ),
                        },
                        consecutive_failures,
                        authority,
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
                        ErSupervisorFailure {
                            phase: "bootstrap-write-failed",
                            pid,
                            exit_success: None,
                            exit_code: None,
                            exit_signal: None,
                            uptime: started_at.elapsed(),
                            observation_error: Some(error.to_string()),
                        },
                        consecutive_failures,
                        authority,
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
                                ErSupervisorFailure {
                                    phase: "unexpected-exit",
                                    pid,
                                    exit_success: Some(status.success()),
                                    exit_code: status.code(),
                                    exit_signal: er_exit_signal(&status),
                                    uptime,
                                    observation_error: None,
                                },
                                consecutive_failures,
                                authority,
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
                                ErSupervisorFailure {
                                    phase: "wait-failed",
                                    pid,
                                    exit_success: None,
                                    exit_code: None,
                                    exit_signal: None,
                                    uptime,
                                    observation_error: Some(error.to_string()),
                                },
                                consecutive_failures,
                                authority,
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

struct ErSupervisorFailure {
    phase: &'static str,
    pid: Option<u32>,
    exit_success: Option<bool>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    uptime: Duration,
    observation_error: Option<String>,
}

fn report_er_supervisor_failure(
    observation: ErSupervisorFailure,
    consecutive_failures: u32,
    authority: &str,
) {
    let ErSupervisorFailure {
        phase,
        pid,
        exit_success,
        exit_code,
        exit_signal,
        uptime,
        observation_error,
    } = observation;
    let why = match observation_error.as_deref() {
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

fn resolve_settings_path() -> String {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(value) = args
        .windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| pair[1].clone())
    {
        return value;
    }

    if args.iter().any(|arg| arg == "--allow-settings-env") {
        if let Ok(value) = std::env::var("SETTINGS_PATH") {
            eprintln!(
                "warning: --allow-settings-env enabled deprecated ambient SETTINGS_PATH support"
            );
            return value;
        }
    } else if std::env::var_os("SETTINGS_PATH").is_some() {
        eprintln!(
            "warning: ignoring ambient SETTINGS_PATH; use --settings <file> (or --allow-settings-env for legacy development compatibility)"
        );
    }

    "settings.json".to_string()
}

async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {
    let er_control_key = host_ready.issue_er_control_key();
    boot_trace("start");
    boot_trace(format!(
        "exe={}",
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    ));
    boot_trace(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    ));

    let settings_path = resolve_settings_path();
    boot_trace(format!("settings path={settings_path}"));
    let mut config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("failed to load {settings_path}: {error}"))?;
    let refresh_interval =
        Duration::from_secs(config.runtime.process_refresh_hours.saturating_mul(3600));
    let maintenance = Arc::new(MaintenanceMetrics::new(
        config.runtime.process_refresh_hours,
    ));
    boot_trace("settings loaded");

    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");

    let mut supervisor = Supervisor::new(RestartPolicy::default());
    let lifecycle = supervisor.lifecycle();
    lifecycle.set(BackendState::Initializing);
    let state_rx = lifecycle.subscribe();
    tokio::spawn(async move {
        supervisor.run().await;
    });
    boot_trace("supervisor spawned");

    let io = atomic_io::AtomicIo::new();
    let admin_dir = runtime_paths::default_admin_dir();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();
    boot_trace("error-client initialized, panic hook installed");
    lifecycle.set(BackendState::ConfigurationLoaded);

    // Compile every executable service and the complete REL Runtime Image
    // before binding even the maintenance responder. A malformed source or
    // invalid ServerPolicy cannot leave the backend half-started.
    let service_catalog = service_boot::compile(&config.services, &io)?;
    let service_interfaces: route_engine::ServiceInterfaces = service_catalog
        .as_ref()
        .map(|catalog| {
            catalog
                .services()
                .iter()
                .map(|service| {
                    (
                        service.name.clone(),
                        service.exports.iter().cloned().collect::<HashSet<_>>(),
                    )
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let runtime_image = runtime_image_boot::compile(&config, service_catalog.as_ref())?;
    runtime_image_boot::apply_server_policy(&mut config, &runtime_image.server_policy)?;
    runtime_image_boot::apply_middleware_plan(&mut config, &runtime_image.middleware_plan)?;
    let runtime_image = Arc::new(route_engine::RuntimeImageSlot::new(runtime_image));
    let config = Arc::new(config);
    boot_trace(format!(
        "effective api bind={}:{}",
        config.api.host, config.api.port
    ));
    tracing::info!(
        path = %settings_path,
        refresh_hours = config.runtime.process_refresh_hours,
        image = %runtime_image.snapshot().image_id,
        "configuration and Runtime Image loaded"
    );

    // Reclaim stale/crashed prior backend listeners BEFORE the temporary
    // responder starts. The responder is this same executable, so running the
    // old image-name based reclaim after it binds would mistake it for stale RBE.
    if config.runtime.reclaim_port {
        port_guard::reclaim_port_if_needed(config.api.port);
    }

    let maintenance_notice =
        maintenance_notice::MaintenanceNoticeProcess::spawn(&config.api.host, config.api.port)
            .await?;
    tracing::info!(
        pid = maintenance_notice.pid(),
        host = %config.api.host,
        port = config.api.port,
        "temporary API maintenance responder ready"
    );
    boot_trace("temporary API maintenance responder ready");
    lifecycle.set(BackendState::ServicesStarting);

    let error_reporter_task = spawn_error_reporter_daemon_process(
        maintenance.clone(),
        refresh_interval,
        er_control_key.clone(),
    )?;

    boot_trace(format!(
        "vault starting as separate process data dir={}",
        admin_dir.display()
    ));
    let vault_recovery_authority: Option<Arc<dyn vault_process::VaultRecoveryAuthority>> =
        er_control_key.as_ref().map(|key| {
            Arc::new(vault_recovery::VaultErRecoveryAuthority::new(key.clone()))
                as Arc<dyn vault_process::VaultRecoveryAuthority>
        });
    let vault_instance = match vault_process::VaultClient::spawn_with_recovery(
        "backend-rs",
        &admin_dir,
        vault_recovery_authority,
    ) {
        Ok(vault) => Arc::new(vault),
        Err(error) => {
            let details = format!("{error:#}");
            error_client::report_issue(error_client::IssueInput {
                source: "backend.vault.startup",
                level: Some(error_client::IssueLevel::Error),
                category: None,
                message: "Vault failed to become ready; backend startup is aborted",
                stack: Some(&details),
            });
            tracing::error!(error = %error, "Vault failed to become ready; backend startup aborted");
            return Err(anyhow::anyhow!("Vault bootstrap failed: {details}"));
        }
    };
    boot_trace("vault process ready");

    let vault_refresh_task = spawn_vault_refresh(
        vault_instance.clone(),
        maintenance.clone(),
        refresh_interval,
    );

    let container_path = container_process::ContainerProcess::packaged_path()?;
    boot_trace(format!(
        "checking required container dependency at {}",
        container_path.display()
    ));
    if !container_path.is_file() {
        anyhow::bail!(
            "required container dependency is missing: {}",
            container_path.display()
        );
    }

    let initial_container =
        container_process::ContainerProcess::spawn(&container_path, &config.containers).await?;
    let (address, token, pid) = initial_container.endpoint();
    let container_client = ContainerClient::new(address, token, pid);
    boot_trace(format!(
        "verified container process ready pid={pid:?} address={address}"
    ));
    let container_process = Arc::new(tokio::sync::Mutex::new(initial_container));
    let container_supervisor_task = spawn_container_supervisor(
        container_path.clone(),
        config.containers.clone(),
        container_process.clone(),
        container_client.clone(),
        maintenance.clone(),
        refresh_interval,
        er_control_key.clone(),
    );

    let service_runtime_env = Arc::new(runtime_image.snapshot().environment.to_json());
    let service_mother = match service_catalog.as_ref() {
        Some(catalog) => Some(
            service_mother::spawn(
                &settings_path,
                &catalog.fingerprint(),
                service_runtime_env.clone(),
                er_control_key.clone(),
                service_integrity::EXPECTED_SERVICE_SHA256,
            )
            .await?,
        ),
        None => None,
    };
    let service_manager = service_mother
        .as_ref()
        .map(|mother| mother.manager())
        .unwrap_or_default();

    let (video_manager, video_worker_task) = if config.video_manager.enabled {
        if config.video_manager.default_database != video_manager::DEFAULT_DATABASE_NAME {
            anyhow::bail!(
                "videoManager.defaultDatabase {:?} is not registered at boot; built-in default is {:?}",
                config.video_manager.default_database,
                video_manager::DEFAULT_DATABASE_NAME
            );
        }
        let data_dir = service_boot::resolve_runtime_path(&config.video_manager.data_dir);
        let database_path = data_dir.join("video-manager.db");
        let manager = Arc::new(video_manager::VideoManager::open_default(
            &database_path,
            config.video_manager.live_idle_secs,
        )?);
        let worker_task = if config.video_manager.download_worker_enabled {
            let ffprobe =
                service_boot::resolve_runtime_path(config.video_manager.ffprobe_executable.trim());
            let ffmpeg =
                service_boot::resolve_runtime_path(config.video_manager.ffmpeg_executable.trim());
            let download = video_manager::DownloadPolicy {
                max_bytes: config.video_manager.download_max_bytes,
                ..Default::default()
            };
            let ffmpeg_policy = video_manager::FfmpegPolicy::new(&ffmpeg);
            let ffmpeg_capabilities =
                video_manager::probe_ffmpeg_capabilities(&ffmpeg_policy).await?;
            let selected_video_encoder = ffmpeg_capabilities.preferred_video_encoder();
            let ffmpeg_policy = ffmpeg_policy.with_video_encoder(selected_video_encoder);
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: ffmpeg_policy,
                recovery_scan: Duration::from_secs(config.video_manager.worker_recovery_scan_secs),
            };
            let task = manager.clone().spawn_download_worker(policy)?;
            tracing::info!(
                ffprobe = %ffprobe.display(),
                ffmpeg = %ffmpeg.display(),
                software_h264 = ffmpeg_capabilities.software_h264,
                aac = ffmpeg_capabilities.aac,
                hardware_h264_encoders = ?ffmpeg_capabilities.hardware_h264_encoders,
                verified_hardware_h264_encoders = ?ffmpeg_capabilities.verified_hardware_h264_encoders,
                selected_video_encoder = ?selected_video_encoder,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
                max_download_bytes = config.video_manager.download_max_bytes,
                "Video Manager lazy download worker ready"
            );
            Some(task)
        } else {
            tracing::info!(
                "Video Manager download worker disabled; queued downloads remain quarantined until a trusted worker is configured"
            );
            None
        };
        tracing::info!(
            database = %database_path.display(),
            live_idle_secs = config.video_manager.live_idle_secs,
            "Video Manager control plane ready; live media workers sleeping"
        );
        (Some(manager), worker_task)
    } else {
        tracing::info!("Video Manager disabled by configuration");
        (None, None)
    };

    let app_state = AppState::new(
        config.clone(),
        state_rx,
        vault_instance,
        container_client,
        service_manager.clone(),
        video_manager,
        maintenance,
    );
    boot_trace("app state created");
    {
        let rate_limiters = app_state.rate_limiters.clone();
        let ip_strikes = app_state.ip_strikes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                rate_limiters.sweep();
                ip_strikes.sweep();
            }
        });
    }

    let api_dir = route_engine::default_api_dir();
    boot_trace(format!("building router api dir={}", api_dir.display()));
    let cache_dir = runtime_paths::binary_dir().join(".cache");
    match route_engine::cache::sync(&io, &api_dir, &cache_dir) {
        Ok(outcomes) => {
            let regenerated = outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome.result,
                        Ok(route_engine::cache::SyncAction::Regenerated)
                    )
                })
                .count();
            let failed: Vec<_> = outcomes
                .iter()
                .filter(|outcome| outcome.result.is_err())
                .collect();
            boot_trace(format!(
                "transpiler cache sync: {} file(s), {regenerated} regenerated, {} failed",
                outcomes.len(),
                failed.len()
            ));
            for outcome in &failed {
                let message = outcome.result.as_ref().unwrap_err();
                tracing::warn!(
                    route = %outcome.route_path.display(),
                    error = %message,
                    "transpiler: failed to generate Rust artifact for this route (interpreted serving is unaffected)"
                );
            }
        }
        Err(error) => tracing::warn!(
            error = %error,
            "transpiler cache sync failed — continuing boot without it"
        ),
    }

    let router = api::build_router(app_state, &api_dir, &service_interfaces, runtime_image)?;
    boot_trace("router built; handing API port to real backend");
    let addr = format!("{}:{}", config.api.host, config.api.port);

    maintenance_notice.stop().await;
    let listener = bind_backend_listener(addr.as_str()).await?;
    lifecycle.set(BackendState::Ready);
    tracing::info!(%addr, "backend ready");
    maybe_open_dashboard(&config);
    lifecycle.set(BackendState::Running);

    let shutdown_lifecycle = lifecycle.clone();
    let result = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown_lifecycle.set(BackendState::ShutdownRequested);
    })
    .await;

    lifecycle.set(BackendState::Stopping);
    let shutdown_budget = Duration::from_millis(config.runtime.graceful_shutdown_timeout_ms.max(1));
    if let Some(mother) = service_mother {
        mother.shutdown(shutdown_budget).await;
    } else {
        service_manager.shutdown_all().await;
    }
    if let Some(worker) = video_worker_task {
        worker
            .shutdown(Duration::from_millis(
                config.runtime.graceful_shutdown_timeout_ms.max(1),
            ))
            .await;
    }
    container_supervisor_task.abort();
    vault_refresh_task.abort();
    error_reporter_task.abort();
    drop(container_process);
    lifecycle.set(BackendState::Stopped);

    result.map_err(|error| anyhow::anyhow!("server error: {error}"))?;
    Ok(())
}

async fn bind_backend_listener(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    const HANDOFF_ATTEMPTS: usize = 100;
    const HANDOFF_RETRY: Duration = Duration::from_millis(50);
    let mut last_addr_in_use = None;

    for attempt in 1..=HANDOFF_ATTEMPTS {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if attempt > 1 {
                    tracing::info!(
                        attempt,
                        %addr,
                        "API port acquired after maintenance handoff retry"
                    );
                }
                return Ok(listener);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                last_addr_in_use = Some(error);
                tokio::time::sleep(HANDOFF_RETRY).await;
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "failed to bind {addr} after maintenance handoff: {error}"
                ));
            }
        }
    }

    Err(anyhow::anyhow!(
        "timed out acquiring {addr} after maintenance handoff: {}",
        last_addr_in_use
            .map(|error| error.to_string())
            .unwrap_or_else(|| "address remained unavailable".into())
    ))
}

fn spawn_container_supervisor(
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

fn spawn_vault_refresh(
    vault: Arc<vault_process::VaultClient>,
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
            let vault = vault.clone();
            let result = tokio::task::spawn_blocking(move || vault.refresh_process()).await;
            match result {
                Ok(Ok(())) => {
                    maintenance.record_vault_refresh();
                    tracing::info!("scheduled Vault process refresh completed");
                }
                Ok(Err(error)) => tracing::error!(
                    error = %error,
                    "scheduled Vault process refresh failed; client will retry on demand"
                ),
                Err(error) => {
                    tracing::error!(error = %error, "Vault refresh worker task failed")
                }
            }
        }
    })
}

fn maybe_open_dashboard(config: &config::Config) {
    if !config.dashboards.enabled
        || !config.dashboards.auto_open
        || std::env::var_os("CI").is_some()
    {
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }

    let prefix = config.dashboards.admin_path_prefix.trim_end_matches('/');
    let url = format!("http://127.0.0.1:{}{prefix}/dashboard", config.api.port);
    tracing::info!(%url, "RBE dashboard ready");

    #[cfg(target_os = "windows")]
    let spawn = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn();
    #[cfg(target_os = "macos")]
    let spawn = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawn = std::process::Command::new("xdg-open").arg(&url).spawn();

    if let Err(error) = spawn {
        tracing::warn!(error = %error, %url, "could not open RBE dashboard automatically");
    }
}

fn boot_trace(message: impl AsRef<str>) {
    if !boot_debug_enabled() {
        return;
    }
    let line = format!("boot: {}", message.as_ref());
    eprintln!("{line}");
    let path = boot_debug_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[pid={}] {line}", std::process::id());
    }
}

fn boot_debug_enabled() -> bool {
    if truthy_env("RBE_BOOT_TRACE") || truthy_env("RBE_DEBUG_BOOT") {
        return true;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    for (index, arg) in args.iter().enumerate() {
        if arg == "--debug-boot" {
            return true;
        }
        if let Some(value) = arg.strip_prefix("--debug-boot=") {
            return truthy(value);
        }
        if let Some(value) = arg.strip_prefix("-debug=") {
            return truthy(value);
        }
        if arg == "-debug" {
            return args
                .get(index + 1)
                .map(|value| truthy(value))
                .unwrap_or(true);
        }
    }
    false
}

fn boot_debug_log_path() -> PathBuf {
    std::env::var_os("RBE_BOOT_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| route_engine::binary_dir().join("boot.log"))
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| truthy(&value))
        .unwrap_or(false)
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod critical_process_supervision_tests {
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

    #[test]
    fn er_self_recovery_backoff_is_exponential_and_capped() {
        assert_eq!(er_restart_delay(1), Duration::from_millis(500));
        assert_eq!(er_restart_delay(2), Duration::from_secs(1));
        assert_eq!(er_restart_delay(3), Duration::from_secs(2));
        assert_eq!(er_restart_delay(30), Duration::from_secs(30));
    }
}
