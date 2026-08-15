//! Boot sequence — migration-plan §3.2. Each step below is numbered to
//! match that section; steps not yet built (storage, container
//! runtime) are TODOs with the phase they land in, not silently skipped.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use core_lib::AppState;
use supervisor::{BackendState, RestartPolicy, Supervisor};

mod container_embed;
mod error_reporter_daemon;
mod port_guard;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    // `--er` selects error-reporter-daemon mode instead of the normal
    // engine boot — see error_reporter_daemon's doc comment for what
    // that mode actually does. `--launch` is deliberately a SEPARATE,
    // required flag rather than `--er` alone being enough: a safety
    // gate against accidentally landing in daemon mode from a typo or
    // a stray flag, since daemon mode does something meaningfully
    // different from what running `backend.exe` normally does.
    // `--separate-process` (or the literal `--saperate-process`) is
    // informational — it's what the normal boot path passes when IT
    // spawns this as a child (see `spawn_error_reporter_daemon_process`
    // below) so the daemon process can log that context; it has no
    // effect on the daemon's actual behavior.
    if has("--er") {
        if !has("--launch") {
            eprintln!(
                "backend.exe --er requires --launch as well (e.g. `backend.exe --er --launch`) — \
                 this is a deliberate safety gate, not a missing feature."
            );
            std::process::exit(2);
        }
        let separate = has("--separate-process") || has("--saperate-process");
        if let Err(err) = run_error_reporter_daemon(separate).await {
            eprintln!("fatal error-reporter-daemon error: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    // Nothing is logged yet at this point by design — see boot() step 1.
    // If boot() itself fails, print plainly and exit non-zero rather
    // than panicking through main, matching §3.2's "fail fast and loud
    // before the HTTP server starts accepting traffic" principle.
    if let Err(err) = boot_and_run().await {
        eprintln!("fatal boot error: {err:#}");
        std::process::exit(1);
    }
}

/// Runs ONLY the error-reporter daemon — no HTTP server, no routes, no
/// vault, nothing else from the normal boot sequence. Kept deliberately
/// minimal: just enough setup (atomic I/O, logging) for
/// `error_reporter_daemon::run` to do its job.
async fn run_error_reporter_daemon(separate_process: bool) -> anyhow::Result<()> {
    // A bare-bones tracing setup — the daemon doesn't have (and
    // shouldn't need) the full engine's `settings.json`-driven config
    // just to decide its own log format. `RUST_LOG` env var still
    // works for anyone who wants to adjust verbosity.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(true).init();

    tracing::info!(
        pid = std::process::id(),
        separate_process,
        "backend.exe running in --er (error-reporter-daemon) mode"
    );

    let io = atomic_io::AtomicIo::new();
    let admin_dir = PathBuf::from("./data/admin");
    error_reporter_daemon::run(io, admin_dir, separate_process).await
}

/// Spawns `backend.exe --er --separate-process --launch` as a genuine
/// child OS process (not a `tokio::spawn`ed in-process task) — see
/// `error_client`'s crate doc comment for why this changed from an
/// earlier in-process version. A crash in the error-reporter no longer
/// takes down the engine, and vice versa; that's the whole point of
/// the split.
///
/// Includes basic crash monitoring: if the child exits unexpectedly,
/// logs it and retries with a fixed backoff, up to a bounded number of
/// attempts — simpler than `supervisor::Supervisor`'s exponential
/// backoff (that's designed for in-process async tasks, not child
/// processes, and reusing it here for something this different would
/// be forcing an abstraction where a much smaller one does the job).
/// Giving up after repeated failures (rather than restarting forever)
/// is deliberate: a daemon that can't start at all (e.g. permission
/// error on `data/admin`) shouldn't spin retrying indefinitely in the
/// background — see the loop's own comment for the exact bound.
fn spawn_error_reporter_daemon_process() -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not resolve current_exe to spawn the error-reporter daemon: {e}"))?;

    tokio::spawn(async move {
        const MAX_ATTEMPTS: u32 = 5;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

        for attempt in 1..=MAX_ATTEMPTS {
            let spawn_result = tokio::process::Command::new(&exe)
                .args(["--er", "--separate-process", "--launch"])
                .kill_on_drop(false) // outlive this task/handle deliberately — it's a daemon, not a subtask
                .spawn();

            let mut child = match spawn_result {
                Ok(child) => child,
                Err(err) => {
                    tracing::error!(attempt, error = %err, "failed to spawn error-reporter daemon process");
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
            };

            tracing::info!(pid = child.id(), attempt, "error-reporter daemon process spawned");

            match child.wait().await {
                Ok(status) if status.success() => {
                    // Clean exit (e.g. someone sent it a graceful
                    // shutdown signal directly) — don't restart, that
                    // would fight an intentional stop.
                    tracing::info!("error-reporter daemon process exited cleanly — not restarting");
                    return;
                }
                Ok(status) => {
                    tracing::warn!(attempt, %status, "error-reporter daemon process exited unexpectedly");
                }
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "error watching error-reporter daemon process");
                }
            }

            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }

        tracing::error!(
            "error-reporter daemon process failed to stay up after {MAX_ATTEMPTS} attempts — giving up. \
             The engine will keep running; issues will just accumulate unsigned in the queue file until \
             the daemon is started again (manually, or by restarting the engine)."
        );
    });

    Ok(())
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

    // Global atomic I/O gate — constructed once, shared (cheap Clone,
    // an Arc underneath) by everything in this process that touches
    // disk: the error-reporter client, the vault, and eventually the
    // container runtime's environments. See atomic-io's doc comment
    // for exactly what "atomic" does and doesn't mean here.
    let io = atomic_io::AtomicIo::new();

    // --- Global error hooks. `error_client::init` sets up the
    // process-wide state `report_issue`/the panic hook write through —
    // must happen BEFORE `install_panic_hook`, so the hook can safely
    // report immediately if something goes wrong right after. See
    // `error_client`'s crate doc comment for the full two-process
    // design (this process only ever WRITES to the queue file; a
    // separate `backend.exe --er --launch` process, spawned below,
    // reads/signs/processes it).
    let admin_dir = PathBuf::from("./data/admin");
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();
    boot_trace("error-client initialized, panic hook installed");

    tracing::info!(path = %settings_path, "configuration loaded");
    boot_trace("configuration log emitted");

    // --- Spawn the error-reporter daemon as a genuinely separate OS
    // process (not an in-process task) — see
    // `spawn_error_reporter_daemon_process`'s doc comment for exactly
    // what that buys and how crash monitoring works. If this fails to
    // even START (e.g. can't resolve current_exe), that's worth
    // logging loudly but NOT worth failing boot over — the engine can
    // run without it, issues just accumulate unsigned in the queue
    // file until it's available.
    if let Err(err) = spawn_error_reporter_daemon_process() {
        tracing::error!(error = %err, "failed to spawn error-reporter daemon process — continuing boot without it");
    }
    boot_trace("error-reporter daemon process spawn requested");

    // --- 3. Init Vault (§8) — gatekeeper over the OS credential store,
    // with a local encrypted-file fallback if that's unavailable (see
    // vault crate's doc comment). Fallible, and deliberately its own
    // boot step: a vault that fails to initialize should stop boot
    // here, not surface as a confusing failure later when something
    // tries to read a credential. Same `admin_dir` the error-reporter
    // client/daemon use above — one shared admin directory, not a
    // separate one per subsystem.
    boot_trace(format!("vault initializing data dir={}", admin_dir.display()));
    let vault_instance = Arc::new(
        vault::Vault::new(io.clone(), "backend-rs", &admin_dir)
            .map_err(|err| anyhow::anyhow!("failed to initialize vault: {err}"))?,
    );
    boot_trace("vault initialized");

    // --- Extract the embedded container binary, if `build.ps1` built
    // one in (see `container_embed`'s doc comment). Only extraction —
    // nothing spawns it yet; that needs the IPC protocol, still a
    // stub. A plain dev build with no embedded container is expected
    // and fine, logged at debug level, not a warning.
    let container_cache_dir = PathBuf::from("./.cache/service");
    match container_embed::extract_if_needed(&io, &container_cache_dir) {
        Ok(Some(path)) => {
            boot_trace(format!("embedded container binary ready at {}", path.display()));
            tracing::info!(path = %path.display(), "container binary extracted and ready");
        }
        Ok(None) => {
            boot_trace("no embedded container binary (standalone build)");
            tracing::debug!("no embedded container binary — this backend was built without build.ps1's combined mode");
        }
        Err(err) => {
            // Not fatal to boot — the engine can run without the
            // container binary available; whatever eventually spawns
            // it (once ipc-protocol exists) will surface a clearer
            // error at that point if it's actually needed.
            boot_trace(format!("container binary extraction failed: {err:#}"));
            tracing::warn!(error = %err, "failed to extract embedded container binary");
        }
    }

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

    // --- RBE Upgrade Plan §2 (Phase 1): best-effort AOT transpiler
    // sync — mirrors each .route file into a generated Rust artifact
    // under .cache/backend/artifact/. Deliberately NOT allowed to fail
    // boot: this is inspectable tooling ahead of the real WASM
    // pipeline (later phases), not on the request-serving critical
    // path yet — routes are still served by the interpreter via
    // build_router below, unchanged. A transpile failure for one route
    // is logged and skipped; it doesn't affect that route's actual
    // (interpreted) serving at all.
    let cache_dir = PathBuf::from("./.cache/backend");
    match route_engine::cache::sync(&io, &api_dir, &cache_dir) {
        Ok(outcomes) => {
            let regenerated = outcomes
                .iter()
                .filter(|o| matches!(o.result, Ok(route_engine::cache::SyncAction::Regenerated)))
                .count();
            let failed: Vec<_> = outcomes.iter().filter(|o| o.result.is_err()).collect();
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
        Err(err) => {
            // Couldn't even list api_dir, or similar — still not fatal
            // to boot, but worth a real warning since it means NO
            // artifacts got a chance to sync this run.
            boot_trace(format!("transpiler cache sync failed entirely: {err:#}"));
            tracing::warn!(error = %err, "transpiler cache sync failed — continuing boot without it");
        }
    }

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
