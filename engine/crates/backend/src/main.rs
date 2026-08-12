//! Boot sequence — migration-plan §3.2. Each step below is numbered to
//! match that section; steps not yet built (storage, container
//! runtime) are TODOs with the phase they land in, not silently skipped.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use core_lib::AppState;
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod port_guard;

#[tokio::main]
async fn main() {
    // Nothing is logged yet at this point by design — see boot() step 1.
    // If boot() itself fails, print plainly and exit non-zero rather
    // than panicking through main, matching §3.2's "fail fast and loud
    // before the HTTP server starts accepting traffic" principle.
    if let Err(err) = boot_and_run().await {
        eprintln!("fatal boot error: {err:#}");
        std::process::exit(1);
    }
}

async fn boot_and_run() -> anyhow::Result<()> {
    boot_trace("start");
    boot_trace(format!(
        "exe={}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|err| format!("<unavailable: {err}>"))
    ));
    boot_trace(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|err| format!("<unavailable: {err}>"))
    ));

    // --- 1. Load settings.json — no logging yet, so a failure here
    // prints to stderr (via the `?` -> boot_and_run's Err -> main's
    // eprintln) and exits, per §3.2.
    let settings_path =
        std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".to_string());
    boot_trace(format!("settings path={settings_path}"));
    let config = config::Config::load(&settings_path)
        .map_err(|err| anyhow::anyhow!("failed to load {settings_path}: {err}"))?;
    let config = Arc::new(config);
    boot_trace("settings loaded");
    boot_trace(format!(
        "effective api bind={}:{}",
        config.api.host, config.api.port
    ));

    // --- 2. Init tracing/logging (§4 Logging Architecture row).
    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");

    // --- Global error hooks (this turn's ask): the "Terminal" is what
    // `terminal::init` just set up; this is the "global error logger"
    // half — Rust's equivalent of `uncaughtExceptionMonitor`.
    logging::install_panic_hook();

    tracing::info!(path = %settings_path, "configuration loaded");
    boot_trace("configuration log emitted");

    // Global atomic I/O gate — constructed once, shared (cheap Clone,
    // an Arc underneath) by everything in this process that touches
    // disk: the error reporter, the vault, and eventually the
    // container runtime's environments. See atomic-io's doc comment
    // for exactly what "atomic" does and doesn't mean here.
    let io = atomic_io::AtomicIo::new();

    // --- Start the error-reporter task. See the long comment on
    // `logging::spawn_error_reporter` for why this is a plain
    // `tokio::spawn`, not a `Supervisor`-registered service like
    // everything from step 6 onward.
    let admin_dir = PathBuf::from("./data/admin");
    boot_trace(format!("error reporter admin dir={}", admin_dir.display()));
    let error_reporter_task = logging::spawn_error_reporter(io.clone(), admin_dir)?;
    boot_trace("error reporter future created");
    tokio::spawn(async move {
        if let Err(err) = error_reporter_task.await {
            // This log call itself goes through `report_runtime_issue`'s
            // fallback path if the reporter is what just died — see that
            // function's doc comment.
            tracing::error!(error = %err, "error-reporter task exited unexpectedly");
        }
    });
    boot_trace("error reporter spawned");

    // --- 3. Init Vault (§8) — gatekeeper over the OS credential store,
    // with a local encrypted-file fallback if that's unavailable (see
    // vault crate's doc comment). Fallible, and deliberately its own
    // boot step: a vault that fails to initialize should stop boot
    // here, not surface as a confusing failure later when something
    // tries to read a credential.
    let vault_data_dir = PathBuf::from("./data/admin");
    boot_trace(format!(
        "vault initializing data dir={}",
        vault_data_dir.display()
    ));
    let vault_instance = Arc::new(
        vault::Vault::new(io.clone(), "backend-rs", &vault_data_dir)
            .map_err(|err| anyhow::anyhow!("failed to initialize vault: {err}"))?,
    );
    boot_trace("vault initialized");

    // --- 4/5. Config is already loaded + validated (step 1 — validation
    // happens inside `Config::load`, matching §3.2's ordering intent:
    // fail before anything downstream starts).

    // --- 6. Init storage pool — TODO(phase 1): `sqlx` pool per
    // `config.storage.driver`.

    // --- 7. Start async supervisor (§3, §7). Bootstrap services
    // (email, etc.) register here in Phase 3; nothing to register yet
    // in Phase 0.
    let mut supervisor = Supervisor::new(RestartPolicy::default());
    supervisor.set_state(BackendState::Initializing);
    let state_rx = supervisor.subscribe_state();
    boot_trace("supervisor initialized");

    // Drive the supervisor concurrently with the HTTP server below —
    // it runs for the lifetime of the process.
    tokio::spawn(async move {
        supervisor.run().await;
    });
    boot_trace("supervisor spawned");

    // --- 8. Spawn container runtime process + establish IPC channel
    // (§5) — TODO(phase 2). NOT started in-process, per §3's
    // non-negotiable exception — when this lands it'll be a real child
    // process, not a supervisor-registered task.

    // --- 9. Build axum Router from api/ modules (Rust-native routes)
    // plus the .route file engine scanning the same /api/ directory.
    let app_state = AppState::new(config.clone(), state_rx, vault_instance);
    boot_trace("app state created");

    // Periodic cleanup for the rate-limiter/IP-strike maps (§9.1 — this
    // is exactly the kind of small, panic-unlikely housekeeping task
    // that doesn't need the full Supervisor restart machinery; a plain
    // supervised-by-nothing tokio task is the right amount of
    // ceremony here).
    {
        let rate_limiters = app_state.rate_limiters.clone();
        let ip_strikes = app_state.ip_strikes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                rate_limiters.sweep();
                ip_strikes.sweep();
            }
        });
    }

    // Resolved relative to the compiled binary's own directory, never
    // the CWD — see route_engine::paths's doc comment. `/api/` and
    // `/module/` are siblings of the binary, never compiled into it.
    let api_dir = route_engine::default_api_dir();
    boot_trace(format!("building router api dir={}", api_dir.display()));
    let router = api::build_router(app_state, &api_dir)?;
    boot_trace("router built");

    // --- 10. Bind + start HTTP server. Port reclaim (see port_guard's
    // doc comment for why this isn't the obsolete step the migration
    // plan originally assumed) runs immediately before binding.
    let addr = format!("{}:{}", config.api.host, config.api.port);
    if config.runtime.reclaim_port {
        boot_trace("port reclaim starting");
        port_guard::reclaim_port_if_needed(config.api.port);
        boot_trace("port reclaim finished");
    }
    boot_trace(format!("binding listener addr={addr}"));
    let listener = tokio::net::TcpListener::bind(addr.as_str())
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind {addr}: {err}"))?;
    boot_trace("listener bound");

    // --- 11. Backend Ready.
    tracing::info!(%addr, "backend ready");
    boot_trace("backend ready log emitted");
    boot_trace("starting axum server");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|err| anyhow::anyhow!("server error: {err}"))?;
    boot_trace("axum server stopped");

    Ok(())
}

fn boot_trace(message: impl AsRef<str>) {
    if !boot_debug_enabled() {
        return;
    }

    let message = message.as_ref();
    let line = format!("boot: {message}");
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

/// Waits for Ctrl+C or SIGTERM. `RuntimeConfig::graceful_shutdown_timeout_ms`
/// isn't wired to an actual forced-kill timer yet — TODO once there are
/// real in-flight requests/connections worth draining deliberately
/// (Phase 5+); for Phase 0 this just stops accepting new connections.
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
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
