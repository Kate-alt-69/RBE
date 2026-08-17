//! Backend boot sequence. Vault is a separate OS process using the same backend executable.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use core_lib::AppState;
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod container_embed;
mod container_process;
mod error_reporter_daemon;
mod port_guard;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--er") {
        if !has("--launch") {
            eprintln!("backend.exe --er requires --launch as well");
            std::process::exit(2);
        }
        let separate = has("--separate-process") || has("--saperate-process");
        if let Err(err) = run_error_reporter_daemon(separate).await {
            eprintln!("fatal error-reporter-daemon error: {err:#}");
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
            args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone()).unwrap_or_else(|| default.to_string())
        };
        let service_name = value("--service-name", "backend-rs");
        let data_dir = PathBuf::from(value("--data-dir", "./data/admin"));
        let force_dbus = has("--dbus");
        if let Err(err) = vault_process::run_vault_daemon(service_name, data_dir, force_dbus) {
            eprintln!("fatal Vault daemon error: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) = boot_and_run().await {
        eprintln!("fatal boot error: {err:#}");
        std::process::exit(1);
    }
}

async fn run_error_reporter_daemon(separate_process: bool) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(true).init();
    tracing::info!(pid = std::process::id(), separate_process, "backend.exe running in --er (error-reporter-daemon) mode");
    let io = atomic_io::AtomicIo::new();
    let admin_dir = PathBuf::from("./data/admin");
    error_reporter_daemon::run(io, admin_dir, separate_process).await
}

fn spawn_error_reporter_daemon_process() -> anyhow::Result<()> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("could not resolve current_exe to spawn the error-reporter daemon: {e}"))?;
    tokio::spawn(async move {
        const MAX_ATTEMPTS: u32 = 5;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);
        for attempt in 1..=MAX_ATTEMPTS {
            let spawn_result = tokio::process::Command::new(&exe).args(["--er", "--separate-process", "--launch"]).kill_on_drop(true).spawn();
            let mut child = match spawn_result {
                Ok(child) => child,
                Err(err) => { tracing::error!(attempt, error = %err, "failed to spawn error-reporter daemon process"); tokio::time::sleep(RETRY_DELAY).await; continue; }
            };
            tracing::info!(pid = child.id(), attempt, "error-reporter daemon process spawned");
            match child.wait().await {
                Ok(status) if status.success() => { tracing::info!("error-reporter daemon process exited cleanly — not restarting"); return; }
                Ok(status) => tracing::warn!(attempt, %status, "error-reporter daemon process exited unexpectedly"),
                Err(err) => tracing::warn!(attempt, error = %err, "error watching error-reporter daemon process"),
            }
            if attempt < MAX_ATTEMPTS { tokio::time::sleep(RETRY_DELAY).await; }
        }
        tracing::error!("error-reporter daemon process failed to stay up after {MAX_ATTEMPTS} attempts — giving up");
    });
    Ok(())
}

async fn boot_and_run() -> anyhow::Result<()> {
    boot_trace("start");
    boot_trace(format!("exe={}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|err| format!("<unavailable: {err}>"))));
    boot_trace(format!("cwd={}", std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|err| format!("<unavailable: {err}>"))));

    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".to_string());
    boot_trace(format!("settings path={settings_path}"));
    let config = config::Config::load(&settings_path).map_err(|err| anyhow::anyhow!("failed to load {settings_path}: {err}"))?;
    let config = Arc::new(config);
    boot_trace("settings loaded");
    boot_trace(format!("effective api bind={}:{}", config.api.host, config.api.port));

    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");

    let io = atomic_io::AtomicIo::new();
    let admin_dir = PathBuf::from("./data/admin");
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();
    boot_trace("error-client initialized, panic hook installed");
    tracing::info!(path = %settings_path, "configuration loaded");

    if let Err(err) = spawn_error_reporter_daemon_process() { tracing::error!(error = %err, "failed to spawn error-reporter daemon process — continuing boot without it"); }

    boot_trace(format!("vault starting as separate process data dir={}", admin_dir.display()));
    let vault_instance = match vault_process::VaultClient::spawn("backend-rs", &admin_dir) {
        Ok(vault) => Arc::new(vault),
        Err(err) => {
            let details = format!("{err:#}");
            error_client::report_issue(error_client::IssueInput { source: "backend.vault.startup", level: Some(error_client::IssueLevel::Error), category: None, message: "Vault failed to become ready after restart attempts; backend is shutting down gracefully", stack: Some(&details) });
            tracing::error!(error = %err, "Vault failed to become ready after restart attempts; backend shutting down gracefully");
            return Ok(());
        }
    };
    boot_trace("vault process ready");

    let container_cache_dir = PathBuf::from("./.cache/service");
    let container_process = match container_embed::extract_if_needed(&io, &container_cache_dir) {
        Ok(Some(path)) => {
            boot_trace(format!("embedded container binary ready at {}", path.display()));
            tracing::info!(path = %path.display(), "embedded container binary extracted and ready");
            Some(container_process::ContainerProcess::spawn(&path).await?)
        }
        Ok(None) => {
            tracing::debug!("no embedded container binary — standalone build; container process not auto-started");
            None
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to extract embedded container binary");
            return Err(err);
        }
    };
    if let Some(process) = &container_process { boot_trace(format!("container process ready pid={:?} address={}", process.pid(), process.address)); }

    let mut supervisor = Supervisor::new(RestartPolicy::default());
    supervisor.set_state(BackendState::Initializing);
    let state_rx = supervisor.subscribe_state();
    tokio::spawn(async move { supervisor.run().await; });
    boot_trace("supervisor spawned");

    let app_state = AppState::new(config.clone(), state_rx, vault_instance);
    boot_trace("app state created");

    {
        let rate_limiters = app_state.rate_limiters.clone();
        let ip_strikes = app_state.ip_strikes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop { interval.tick().await; rate_limiters.sweep(); ip_strikes.sweep(); }
        });
    }

    let api_dir = route_engine::default_api_dir();
    boot_trace(format!("building router api dir={}", api_dir.display()));
    let cache_dir = PathBuf::from("./.cache/backend");
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
    boot_trace("router built");

    let addr = format!("{}:{}", config.api.host, config.api.port);
    if config.runtime.reclaim_port { port_guard::reclaim_port_if_needed(config.api.port); }
    let listener = tokio::net::TcpListener::bind(addr.as_str()).await.map_err(|err| anyhow::anyhow!("failed to bind {addr}: {err}"))?;

    tracing::info!(%addr, "backend ready");
    let result = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await;
    drop(container_process);
    result.map_err(|err| anyhow::anyhow!("server error: {err}"))?;
    Ok(())
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
