//! Backend boot sequence. Vault, container runtime, and user `.service` files
//! are separate supervised OS processes. Video Manager's lightweight control
//! plane lives in-process; heavy media workers remain lazy/separate.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_lib::{AppState, ContainerClient, MaintenanceMetrics};
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod container_process;
mod error_reporter_daemon;
mod maintenance_notice;
mod port_guard;
mod service_boot;
mod service_mother;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if has("--service-mother") {
        if let Err(error) = service_mother::run_child(&args).await {
            eprintln!("fatal service-mother error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    // This must branch before normal backend boot. A user .service process is
    // the same binary in a restricted host mode, not a second mother backend.
    if has("--service-host") {
        if let Err(error) = service_boot::run_host(&args).await {
            eprintln!("fatal service-host error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

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
        if let Err(error) = run_error_reporter_daemon(separate).await {
            eprintln!("fatal error-reporter-daemon error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--vault") {
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

    if let Err(error) = boot_and_run().await {
        eprintln!("fatal boot error: {error:#}");
        std::process::exit(1);
    }
}

async fn run_error_reporter_daemon(separate_process: bool) -> anyhow::Result<()> {
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
    error_reporter_daemon::run(io, admin_dir, separate_process).await
}

fn spawn_error_reporter_daemon_process(
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let exe = std::env::current_exe().map_err(|error| {
        anyhow::anyhow!("could not resolve current_exe to spawn the error-reporter daemon: {error}")
    })?;
    Ok(tokio::spawn(async move {
        const RETRY_DELAY: Duration = Duration::from_secs(3);
        let mut consecutive_failures = 0u32;
        loop {
            let spawn_result = tokio::process::Command::new(&exe)
                .args(["--er", "--separate-process", "--launch"])
                .kill_on_drop(true)
                .spawn();
            let mut child = match spawn_result {
                Ok(child) => child,
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::error!(
                        consecutive_failures,
                        error = %error,
                        "failed to spawn error-reporter daemon process"
                    );
                    tokio::time::sleep(RETRY_DELAY.min(Duration::from_secs(30))).await;
                    continue;
                }
            };
            consecutive_failures = 0;
            tracing::info!(pid = child.id(), "error-reporter daemon process spawned");

            tokio::select! {
                status = child.wait() => {
                    match status {
                        Ok(status) if status.success() => tracing::warn!(%status, "error-reporter daemon exited; restarting supervisor child"),
                        Ok(status) => tracing::warn!(%status, "error-reporter daemon exited unexpectedly; restarting"),
                        Err(error) => tracing::warn!(error = %error, "error watching error-reporter daemon process; restarting"),
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                _ = tokio::time::sleep(refresh_interval) => {
                    tracing::info!(
                        hours = refresh_interval.as_secs() / 3600,
                        "scheduled error-reporter process refresh"
                    );
                    if let Err(error) = child.kill().await {
                        tracing::warn!(error = %error, "failed to terminate error-reporter for scheduled refresh");
                    }
                    let _ = child.wait().await;
                    maintenance.record_error_reporter_refresh();
                }
            }
        }
    }))
}

async fn boot_and_run() -> anyhow::Result<()> {
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

    let settings_path =
        std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".to_string());
    boot_trace(format!("settings path={settings_path}"));
    let config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("failed to load {settings_path}: {error}"))?;
    let config = Arc::new(config);
    let refresh_interval =
        Duration::from_secs(config.runtime.process_refresh_hours.saturating_mul(3600));
    let maintenance = Arc::new(MaintenanceMetrics::new(
        config.runtime.process_refresh_hours,
    ));
    boot_trace("settings loaded");
    boot_trace(format!(
        "effective api bind={}:{}",
        config.api.host, config.api.port
    ));

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

    let io = atomic_io::AtomicIo::new();
    let admin_dir = runtime_paths::default_admin_dir();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();
    boot_trace("error-client initialized, panic hook installed");
    tracing::info!(
        path = %settings_path,
        refresh_hours = config.runtime.process_refresh_hours,
        "configuration loaded"
    );
    lifecycle.set(BackendState::ConfigurationLoaded);

    // Parse the complete user service catalog before expensive infrastructure
    // startup. One malformed service fails the whole boot with SVC diagnostics
    // rather than leaving a partially-started backend.
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
    lifecycle.set(BackendState::ServicesStarting);

    let error_reporter_task =
        spawn_error_reporter_daemon_process(maintenance.clone(), refresh_interval)?;

    boot_trace(format!(
        "vault starting as separate process data dir={}",
        admin_dir.display()
    ));
    let vault_instance = match vault_process::VaultClient::spawn("backend-rs", &admin_dir) {
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
    let container_refresh_task = spawn_container_refresh(
        container_path.clone(),
        config.containers.clone(),
        container_process.clone(),
        container_client.clone(),
        maintenance.clone(),
        refresh_interval,
    );

    let service_mother = match service_catalog.as_ref() {
        Some(catalog) => Some(service_mother::spawn(&settings_path, &catalog.fingerprint()).await?),
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
    let cache_dir = runtime_paths::binary_dir().join(".cache").join("backend");
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

    let router = api::build_router(app_state, &api_dir, &service_interfaces)?;
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
    container_refresh_task.abort();
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

fn spawn_container_refresh(
    binary: PathBuf,
    settings: config::ContainersConfig,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
    client: ContainerClient,
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        const DRAIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
        let mut interval = tokio::time::interval(refresh_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
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
    })
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
