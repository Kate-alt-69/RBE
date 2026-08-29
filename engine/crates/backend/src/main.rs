//! Backend boot sequence. Vault and the container are separate supervised OS processes.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_lib::{AppState, ContainerClient, MaintenanceMetrics};
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod container_process;
mod error_reporter_daemon;
mod maintenance_notice;
mod port_guard;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--maintenance-notice") {
        let value = |flag: &str| args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone());
        let host = value("--maintenance-host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = value("--maintenance-port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8080);
        if let Err(err) = maintenance_notice::run(host, port).await {
            eprintln!("fatal maintenance responder error: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--er") {
        if !has("--launch") { eprintln!("backend.exe --er requires --launch as well"); std::process::exit(2); }
        let separate = has("--separate-process") || has("--saperate-process");
        if let Err(err) = run_error_reporter_daemon(separate).await { eprintln!("fatal error-reporter-daemon error: {err:#}"); std::process::exit(1); }
        return;
    }

    if has("--vault") {
        if !has("--separate-process") && !has("--saperate-process") { eprintln!("backend.exe --vault requires --separate-process"); std::process::exit(2); }
        let value = |flag: &str, default: &str| args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone()).unwrap_or_else(|| default.to_string());
        let service_name = value("--service-name", "backend-rs");
        let data_dir = PathBuf::from(value("--data-dir", &runtime_paths::default_admin_dir().to_string_lossy()));
        let force_dbus = has("--dbus");
        if let Err(err) = vault_process::run_vault_daemon(service_name, data_dir, force_dbus) { eprintln!("fatal Vault daemon error: {err:#}"); std::process::exit(1); }
        return;
    }

    if let Err(err) = boot_and_run().await { eprintln!("fatal boot error: {err:#}"); std::process::exit(1); }
}

async fn run_error_reporter_daemon(separate_process: bool) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(true).init();
    tracing::info!(pid = std::process::id(), separate_process, "backend.exe running in --er (error-reporter-daemon) mode");
    let io = atomic_io::AtomicIo::new();
    let admin_dir = runtime_paths::default_admin_dir();
    error_reporter_daemon::run(io, admin_dir, separate_process).await
}

fn spawn_error_reporter_daemon_process(
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("could not resolve current_exe to spawn the error-reporter daemon: {e}"))?;
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
                Err(err) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::error!(consecutive_failures, error = %err, "failed to spawn error-reporter daemon process");
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
                        Err(err) => tracing::warn!(error = %err, "error watching error-reporter daemon process; restarting"),
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                _ = tokio::time::sleep(refresh_interval) => {
                    tracing::info!(hours = refresh_interval.as_secs() / 3600, "scheduled error-reporter process refresh");
                    if let Err(err) = child.kill().await { tracing::warn!(error = %err, "failed to terminate error-reporter for scheduled refresh"); }
                    let _ = child.wait().await;
                    maintenance.record_error_reporter_refresh();
                }
            }
        }
    }))
}

async fn boot_and_run() -> anyhow::Result<()> {
    boot_trace("start");
    boot_trace(format!("exe={}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|err| format!("<unavailable: {err}>"))));
    boot_trace(format!("cwd={}", std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|err| format!("<unavailable: {err}>"))));

    let settings_path = std::env::var_os("SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_paths::binary_dir().join("settings.json"));
    boot_trace(format!("settings path={}", settings_path.display()));
    let config = config::Config::load(&settings_path)
        .map_err(|err| anyhow::anyhow!("failed to load {}: {err}", settings_path.display()))?;
    let config = Arc::new(config);
    let refresh_interval = Duration::from_secs(config.runtime.process_refresh_hours.saturating_mul(3600));
    let maintenance = Arc::new(MaintenanceMetrics::new(config.runtime.process_refresh_hours));
    boot_trace("settings loaded");
    boot_trace(format!("effective api bind={}:{}", config.api.host, config.api.port));

    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");

    if config.runtime.reclaim_port {
        port_guard::reclaim_port_if_needed(config.api.port);
    }

    let maintenance_notice = maintenance_notice::MaintenanceNoticeProcess::spawn(
        &config.api.host,
        config.api.port,
    )
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
    tracing::info!(path = %settings_path.display(), refresh_hours = config.runtime.process_refresh_hours, "configuration loaded");

    let (lifecycle_tx, lifecycle_rx) = tokio::sync::watch::channel(BackendState::ConfigurationLoaded);
    let _ = lifecycle_tx.send(BackendState::ServicesStarting);

    let error_reporter_task = spawn_error_reporter_daemon_process(maintenance.clone(), refresh_interval)?;

    boot_trace(format!("vault starting as separate process data dir={}", admin_dir.display()));
    let vault_instance = match vault_process::VaultClient::spawn("backend-rs", &admin_dir) {
        Ok(vault) => Arc::new(vault),
        Err(err) => {
            let details = format!("{err:#}");
            error_client::report_issue(error_client::IssueInput { source: "backend.vault.startup", level: Some(error_client::IssueLevel::Error), category: None, message: "Vault failed to become ready; backend startup is aborted", stack: Some(&details) });
            tracing::error!(error = %err, "Vault failed to become ready; backend startup aborted");
            return Err(anyhow::anyhow!("Vault bootstrap failed: {details}"));
        }
    };
    boot_trace("vault process ready");

    let vault_refresh_task = spawn_vault_refresh(vault_instance.clone(), maintenance.clone(), refresh_interval);

    let container_path = container_process::ContainerProcess::packaged_path()?;
    boot_trace(format!("checking required container dependency at {}", container_path.display()));
    if !container_path.is_file() { anyhow::bail!("required container dependency is missing: {}", container_path.display()); }

    let initial_container = container_process::ContainerProcess::spawn(&container_path, &config.containers).await?;
    let (address, token, pid) = initial_container.endpoint();
    let container_client = ContainerClient::new(address, token, pid);
    boot_trace(format!("verified container process ready pid={pid:?} address={address}"));
    let container_process = Arc::new(tokio::sync::Mutex::new(initial_container));
    let container_refresh_task = spawn_container_refresh(
        container_path.clone(),
        config.containers.clone(),
        container_process.clone(),
        container_client.clone(),
        maintenance.clone(),
        refresh_interval,
    );

    let mut supervisor = Supervisor::new(RestartPolicy::default());
    supervisor.set_state(BackendState::ServicesStarting);
    tokio::spawn(async move { supervisor.run().await; });
    boot_trace("supervisor spawned");

    let app_state = AppState::new(config.clone(), lifecycle_rx, vault_instance, container_client, maintenance);
    boot_trace("app state created");
    {
        let rate_limiters = app_state.rate_limiters.clone();
        let ip_strikes = app_state.ip_strikes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop { interval.tick().await; rate_limiters.sweep(); ip_strikes.sweep(); }
        });
    }

    let api_dir = route_engine::default_api_dir();
    boot_trace(format!("building router api dir={}", api_dir.display()));
    let cache_dir = runtime_paths::binary_dir().join(".cache").join("backend");
    match route_engine::cache::sync(&io, &api_dir, &cache_dir) {
        Ok(outcomes) => {
            let regenerated = outcomes.iter().filter(|o| matches!(o.result, Ok(route_engine::cache::SyncAction::Regenerated))).count();
            let failed: Vec<_> = outcomes.iter().filter(|o| o.result.is_err()).collect();
            boot_trace(format!("transpiler cache sync: {} file(s), {regenerated} regenerated, {} failed", outcomes.len(), failed.len()));
            for outcome in &failed { let message = outcome.result.as_ref().unwrap_err(); tracing::warn!(route = %outcome.route_path.display(), error = %message, "transpiler: failed to generate Rust artifact for this route (interpreted serving is unaffected)"); }
        }
        Err(err) => tracing::warn!(error = %err, "transpiler cache sync failed — continuing boot without it"),
    }

    let router = api::build_router(app_state, &api_dir)?;
    let _ = lifecycle_tx.send(BackendState::Ready);
    boot_trace("router built; handing API port to real backend");
    let addr = format!("{}:{}", config.api.host, config.api.port);

    maintenance_notice.stop().await;
    let listener = bind_backend_listener(addr.as_str()).await?;
    let _ = lifecycle_tx.send(BackendState::Running);
    tracing::info!(%addr, "backend ready");
    maybe_open_dashboard(&config);

    let result = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal(lifecycle_tx.clone()))
        .await;

    let _ = lifecycle_tx.send(BackendState::Stopping);
    container_refresh_task.abort();
    vault_refresh_task.abort();
    error_reporter_task.abort();
    drop(container_process);
    let _ = lifecycle_tx.send(BackendState::Stopped);

    result.map_err(|err| anyhow::anyhow!("server error: {err}"))?;
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
                    tracing::info!(attempt, %addr, "API port acquired after maintenance handoff retry");
                }
                return Ok(listener);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                last_addr_in_use = Some(err);
                tokio::time::sleep(HANDOFF_RETRY).await;
            }
            Err(err) => {
                return Err(anyhow::anyhow!("failed to bind {addr} after maintenance handoff: {err}"));
            }
        }
    }

    Err(anyhow::anyhow!(
        "timed out acquiring {addr} after maintenance handoff: {}",
        last_addr_in_use
            .map(|err| err.to_string())
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
            tracing::info!(hours = refresh_interval.as_secs() / 3600, "starting scheduled rolling container refresh");

            if let Err(err) = client.prepare_refresh(DRAIN_TIMEOUT).await {
                tracing::warn!(error = %err, "container did not complete refresh drain; retaining current process");
                if let Err(resume_err) = client.resume().await {
                    tracing::error!(error = %resume_err, "failed to resume container after refresh drain failure");
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
                    tracing::info!(pid, %address, "replacement container healthy; IPC switched to new process");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    drop(old);
                }
                Err(err) => {
                    tracing::error!(error = %err, "scheduled container replacement failed; resuming existing healthy container");
                    if let Err(resume_err) = resume_container_with_retry(&client).await {
                        tracing::error!(error = %resume_err, "failed to resume existing container after replacement failure");
                    }
                }
            }
        }
    })
}

async fn resume_container_with_retry(client: &ContainerClient) -> anyhow::Result<()> {
    let mut last_error = None;
    for attempt in 1..=5 {
        match client.resume().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(attempt, error = %error, "container resume attempt failed");
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("container resume failed without an error")))
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
                Ok(Ok(())) => { maintenance.record_vault_refresh(); tracing::info!("scheduled Vault process refresh completed"); }
                Ok(Err(err)) => tracing::error!(error = %err, "scheduled Vault process refresh failed; client will retry on demand"),
                Err(err) => tracing::error!(error = %err, "Vault refresh worker task failed"),
            }
        }
    })
}

fn maybe_open_dashboard(config: &config::Config) {
    if !config.dashboards.enabled || !config.dashboards.auto_open || std::env::var_os("CI").is_some() { return; }
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() { return; }

    let prefix = config.dashboards.admin_path_prefix.trim_end_matches('/');
    let url = format!("http://127.0.0.1:{}{prefix}/dashboard", config.api.port);
    tracing::info!(%url, "RBE dashboard ready");

    #[cfg(target_os = "windows")]
    let spawn = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
    #[cfg(target_os = "macos")]
    let spawn = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawn = std::process::Command::new("xdg-open").arg(&url).spawn();

    if let Err(err) = spawn { tracing::warn!(error = %err, %url, "could not open RBE dashboard automatically"); }
}

fn boot_trace(message: impl AsRef<str>) {
    if !boot_debug_enabled() { return; }
    let line = format!("boot: {}", message.as_ref());
    eprintln!("{line}");
    let path = boot_debug_log_path();
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) { let _ = writeln!(file, "[pid={}] {line}", std::process::id()); }
}

fn boot_debug_enabled() -> bool {
    if truthy_env("RBE_BOOT_TRACE") || truthy_env("RBE_DEBUG_BOOT") { return true; }
    let args: Vec<String> = std::env::args().skip(1).collect();
    for (index, arg) in args.iter().enumerate() {
        if arg == "--debug-boot" { return true; }
        if let Some(value) = arg.strip_prefix("--debug-boot=") { return truthy(value); }
        if let Some(value) = arg.strip_prefix("-debug=") { return truthy(value); }
        if arg == "-debug" { return args.get(index + 1).map(|value| truthy(value)).unwrap_or(true); }
    }
    false
}

fn boot_debug_log_path() -> PathBuf { std::env::var_os("RBE_BOOT_LOG").map(PathBuf::from).unwrap_or_else(|| route_engine::binary_dir().join("boot.log")) }
fn truthy_env(name: &str) -> bool { std::env::var(name).map(|value| truthy(&value)).unwrap_or(false) }
fn truthy(value: &str) -> bool { matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on") }

async fn shutdown_signal(lifecycle_tx: tokio::sync::watch::Sender<BackendState>) {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler"); };
    #[cfg(unix)]
    let terminate = async { tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to install SIGTERM handler").recv().await; };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    let _ = lifecycle_tx.send(BackendState::ShutdownRequested);
    tracing::info!("shutdown signal received");
}
