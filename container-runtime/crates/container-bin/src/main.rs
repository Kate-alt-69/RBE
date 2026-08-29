//! Standalone `container` execution service.
//!
//! backend.exe owns the user-facing dashboard. This process owns execution,
//! Environment → Swamp → Worker scheduling, durable cache/journal state, and a
//! local authenticated control socket. A sibling monitor is supervised and
//! recycled periodically so long-running process-local allocations are bounded.

mod dashboard;

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use container_runtime_core::{EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost};
use execution_engine::{ExecutionLimits, WasmExecutor};
use ipc_protocol::{decode_request, read_frame, write_frame, Request, Response, PROTOCOL_VERSION};
use resource_limits::ResourceLimits;
use sandbox_primitives::{install_restricted_seccomp, set_no_new_privileges, SandboxPolicy};

const DEFAULT_DASHBOARD_ADDRESS: &str = "127.0.0.1:8787";
const MONITOR_REFRESH: Duration = Duration::from_secs(500 * 60 * 60);
const MONITOR_POLL: Duration = Duration::from_millis(250);
const MONITOR_RESTART_DELAY: Duration = Duration::from_secs(2);
const MAX_REFRESH_DRAIN: Duration = Duration::from_secs(5 * 60);
const EVENT_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;
const EVENT_LOG_KEEP_BYTES: u64 = 8 * 1024 * 1024;
const MONITOR_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;
const MONITOR_LOG_KEEP_BYTES: u64 = 4 * 1024 * 1024;
static EVENT_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn event_log_path() -> PathBuf {
    runtime_paths::binary_dir().join("data").join("container-runtime").join("container-events.jsonl")
}
pub(crate) fn monitor_log_path() -> PathBuf {
    runtime_paths::binary_dir().join("data").join("container-runtime").join("container-monitor.log")
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.iter().any(|arg| arg == "--monitor") { return run_monitor(&args); }
    if args.iter().any(|arg| arg == "--worker") { return run_worker(&args); }

    let debug = args.iter().any(|arg| arg == "--debug");
    let listen = value_after(&args, "--listen");
    let dashboard_disabled = args.iter().any(|arg| arg == "--no-dashboard");
    let dashboard_address = value_after(&args, "--dashboard-listen").unwrap_or_else(|| DEFAULT_DASHBOARD_ADDRESS.to_string());
    emit_event("container_start", &format!("pid={}", std::process::id()));

    if let Err(err) = spawn_monitor_supervisor() {
        emit_event("monitor_supervisor_failed", &err.to_string());
        tracing::warn!(error = %err, "container monitor supervisor could not be started");
    }

    let admin_dir = runtime_paths::default_admin_dir();
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();

    let vault_data_dir = runtime_paths::binary_dir().join("data").join("container-admin");
    let vault = match vault::Vault::new(io.clone(), "backend-rs-container", &vault_data_dir) {
        Ok(v) => Arc::new(v),
        Err(e) => {
            emit_event("vault_init_failed", &e.to_string());
            error_client::report_issue(error_client::IssueInput {
                source: "container_bin_boot",
                level: Some(error_client::IssueLevel::Error),
                category: Some(error_client::IssueCategory::OperationFailure),
                message: &format!("container-bin: failed to init vault: {e}"),
                stack: None,
            });
            return Err(anyhow::anyhow!("container-bin: failed to init vault: {e}"));
        }
    };

    let _registry = Arc::new(EnvironmentRegistry::new(io, vault, &vault_data_dir));
    let defaults = RuntimeConfig::default();
    let general_environments = value_after(&args, "--general-environments")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(defaults.general_environments);
    let swamps_per_environment = value_after(&args, "--swamps-per-environment")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(defaults.swamps_per_environment);
    let workers_per_swamp = value_after(&args, "--workers-per-swamp")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(defaults.workers_per_swamp);
    let runtime = Runtime::new(RuntimeConfig {
        general_environments,
        swamps_per_environment,
        workers_per_swamp,
        rebalance_interval_ms: 25,
    });
    let accepting = Arc::new(AtomicBool::new(true));

    emit_event("runtime_config", &format!(
        "general_environments={} swamps_per_environment={} workers_per_swamp={}",
        runtime.config().general_environments,
        runtime.config().swamps_per_environment,
        runtime.config().workers_per_swamp
    ));

    let token = env::var("RBE_CONTAINER_TOKEN").ok();
    if !dashboard_disabled {
        match token.as_ref() {
            Some(token) => {
                if let Err(error) = dashboard::spawn(dashboard_address.clone(), token.clone(), runtime.clone()) {
                    emit_event("dashboard_start_failed", &error.to_string());
                    tracing::warn!(error = %error, address = %dashboard_address, "container standalone dashboard could not be started");
                } else {
                    emit_event("dashboard_listening", &dashboard_address);
                }
            }
            None => {
                emit_event("dashboard_disabled_no_token", "RBE_CONTAINER_TOKEN is not set");
                tracing::warn!("container standalone dashboard disabled because RBE_CONTAINER_TOKEN is not set");
            }
        }
    }

    if debug { run_debug(&args, &runtime)?; }
    if let Some(address) = listen {
        let token = token.ok_or_else(|| anyhow::anyhow!("RBE_CONTAINER_TOKEN must be set when --listen is used"))?;
        run_control_server(&address, token, runtime.clone(), accepting)?;
    } else if !debug {
        println!("container: no control socket requested; exiting after initialization");
    }
    Ok(())
}

fn spawn_monitor_supervisor() -> anyhow::Result<()> {
    let exe = env::current_exe()?;
    let watched_pid = std::process::id();
    let events = event_log_path();
    let log = monitor_log_path();
    thread::Builder::new().name("container-monitor-supervisor".into()).spawn(move || {
        loop {
            let mut child = match std::process::Command::new(&exe)
                .arg("--monitor").arg("--pid").arg(watched_pid.to_string())
                .arg("--events").arg(&events).arg("--log").arg(&log)
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    emit_event("monitor_spawn_failed", &error.to_string());
                    thread::sleep(MONITOR_RESTART_DELAY);
                    continue;
                }
            };
            let started = Instant::now();
            emit_event("monitor_started", &format!("pid={}", child.id()));
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        emit_event("monitor_exit", &status.to_string());
                        thread::sleep(MONITOR_RESTART_DELAY);
                        break;
                    }
                    Err(error) => {
                        emit_event("monitor_wait_error", &error.to_string());
                        let _ = child.kill();
                        let _ = child.wait();
                        thread::sleep(MONITOR_RESTART_DELAY);
                        break;
                    }
                    Ok(None) => {}
                }
                if started.elapsed() >= MONITOR_REFRESH {
                    emit_event("monitor_scheduled_refresh", "hours=500");
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
    })?;
    Ok(())
}

fn run_monitor(args: &[String]) -> anyhow::Result<()> {
    let watched_pid = value_after(args, "--pid").and_then(|value| value.parse::<u32>().ok()).ok_or_else(|| anyhow::anyhow!("monitor: --pid is required"))?;
    let event_path = value_after(args, "--events").map(PathBuf::from).unwrap_or_else(event_log_path);
    let monitor_path = value_after(args, "--log").map(PathBuf::from).unwrap_or_else(monitor_log_path);
    if let Some(parent) = monitor_path.parent() { fs::create_dir_all(parent)?; }

    let mut last_event_len = 0_u64;
    loop {
        if let Ok(metadata) = fs::metadata(&event_path) {
            if metadata.len() < last_event_len { last_event_len = 0; }
            if metadata.len() > last_event_len {
                if let Ok(mut file) = OpenOptions::new().read(true).open(&event_path) {
                    file.seek(SeekFrom::Start(last_event_len))?;
                    let mut appended = String::new();
                    file.read_to_string(&mut appended)?;
                    last_event_len = metadata.len();
                    if !appended.trim().is_empty() {
                        append_monitor_log(&monitor_path, &format!("{} container-event {}", now_ms(), appended.trim_end()))?;
                    }
                }
            }
        }
        if !process_exists(watched_pid) {
            append_monitor_log(&monitor_path, &format!("{} container_process_exit pid={watched_pid}", now_ms()))?;
            return Ok(());
        }
        thread::sleep(MONITOR_POLL);
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() { return false; }
        let mut exit_code = 0u32;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
        unsafe { CloseHandle(handle); }
        ok && exit_code == 259
    }
}

fn append_monitor_log(path: &Path, line: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    rotate_log(path, MONITOR_LOG_MAX_BYTES, MONITOR_LOG_KEEP_BYTES)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn emit_event(kind: &str, detail: &str) {
    let _guard = EVENT_LOG_LOCK.get_or_init(|| Mutex::new(())).lock().expect("event log lock poisoned");
    let path = event_log_path();
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let _ = rotate_log(&path, EVENT_LOG_MAX_BYTES, EVENT_LOG_KEEP_BYTES);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let event = serde_json::json!({ "ts": now_ms(), "pid": std::process::id(), "kind": kind, "detail": detail });
        let _ = writeln!(file, "{event}");
    }
}

fn rotate_log(path: &Path, max_bytes: u64, keep_bytes: u64) -> anyhow::Result<()> {
    let Ok(metadata) = fs::metadata(path) else { return Ok(()); };
    if metadata.len() <= max_bytes { return Ok(()); }
    let start = metadata.len().saturating_sub(keep_bytes);
    let mut input = OpenOptions::new().read(true).open(path)?;
    input.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    input.read_to_end(&mut tail)?;
    if start > 0 {
        if let Some(index) = tail.iter().position(|byte| *byte == b'\n') { tail.drain(..=index); }
    }
    let mut output = OpenOptions::new().write(true).truncate(true).open(path)?;
    output.write_all(&tail)?;
    output.flush()?;
    Ok(())
}

fn now_ms() -> u128 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() }

fn run_worker(args: &[String]) -> anyhow::Result<()> {
    set_no_new_privileges().map_err(|e| anyhow::anyhow!("worker: failed to set no_new_privs: {e}"))?;
    install_restricted_seccomp().map_err(|e| anyhow::anyhow!("worker: failed to install seccomp: {e}"))?;
    let artifact = value_after(args, "--artifact").ok_or_else(|| anyhow::anyhow!("worker: --artifact is required"))?;
    if artifact.is_empty() || artifact.len() > 128 || !artifact.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') { anyhow::bail!("worker: invalid artifact hash"); }
    let path = runtime_paths::binary_dir().join("data").join("container-runtime").join("artifacts").join(format!("{artifact}.wasm"));
    let wasm = fs::read(path).map_err(|e| anyhow::anyhow!("worker: failed to read artifact: {e}"))?;
    let fuel = value_after(args, "--fuel").and_then(|value| value.parse::<u64>().ok()).unwrap_or(10_000_000);
    let max_memory_bytes = value_after(args, "--memory").and_then(|value| value.parse::<u64>().ok()).unwrap_or(64 * 1024 * 1024);
    let executor = WasmExecutor::new()?;
    let result = executor.execute(&wasm, ExecutionLimits { fuel, max_memory_bytes })?;
    if result.exit_code != 0 { anyhow::bail!("worker: WASM exited with status {}", result.exit_code); }
    Ok(())
}

fn run_control_server(address: &str, token: String, runtime: Arc<Runtime>, accepting: Arc<AtomicBool>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("container: control socket listening on {}", listener.local_addr()?);
    emit_event("control_listening", address);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = token.clone();
                let runtime = Arc::clone(&runtime);
                let accepting = Arc::clone(&accepting);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &token, &runtime, &accepting) {
                        tracing::warn!(%err, "container control connection closed with error");
                    }
                });
            }
            Err(err) => tracing::warn!(%err, "failed to accept container control connection"),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, token: &str, runtime: &Runtime, accepting: &AtomicBool) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = decode_request(&read_frame(&mut reader)?)?;
    let response = match request {
        Request::Hello(hello) => {
            if hello.version != PROTOCOL_VERSION || hello.auth_token != token {
                Response::Error { request_id: None, code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else { Response::HelloAccepted { version: PROTOCOL_VERSION } }
        }
        Request::Execute(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else if !accepting.load(Ordering::Acquire) {
                Response::Error { request_id: Some(request.request_id), code: "REFRESHING".into(), message: "container is draining for a supervised process refresh".into() }
            } else if let Some(environment) = parse_environment(&request.environment).filter(|id| runtime.has_environment(*id)) {
                let cost = WorkCost { cpu: request.declared_cost.cpu, memory: request.declared_cost.memory, io: request.declared_cost.io, network: request.declared_cost.network };
                let execution_id = runtime.submit_with_policy(environment, request.artifact_hash, cost, ResourceLimits::default(), SandboxPolicy::default(), 0, request.payload);
                emit_event("execution_accepted", &execution_id.to_string());
                Response::Accepted { request_id: request.request_id, execution_id: execution_id.to_string() }
            } else {
                Response::Error { request_id: Some(request.request_id), code: "INVALID_ENVIRONMENT".into(), message: format!("container environment is unavailable: {}", request.environment) }
            }
        }
        Request::Health(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else {
                let snapshots = runtime.snapshots();
                let (swamps, workers, busy, completed, failed) = topology_totals(&snapshots);
                Response::Health { request_id: request.request_id, body: serde_json::json!({
                    "protocol": PROTOCOL_VERSION,
                    "process": "container",
                    "pid": std::process::id(),
                    "accepting_executions": accepting.load(Ordering::Acquire),
                    "environments": snapshots.len(),
                    "general_environments": runtime.config().general_environments,
                    "payment_environments": 1,
                    "swamps": swamps,
                    "workers": workers,
                    "workers_busy": busy,
                    "queue": runtime.global_queue_len(),
                    "completed": completed,
                    "failed": failed,
                    "sandbox_policy": "deny-by-default",
                    "wasm_engine": "wasmtime",
                    "artifact_cache": runtime.cache().artifact_count(),
                    "profile_cache": runtime.cache().len()
                }) }
            }
        }
        Request::Cancel(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else if runtime.cancel(&request.execution_id) {
                emit_event("execution_cancel", &request.execution_id);
                Response::Cancelled { request_id: request.request_id }
            } else {
                Response::Error { request_id: Some(request.request_id), code: "NOT_FOUND".into(), message: "execution ID was not found".into() }
            }
        }
        Request::Inspect(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else {
                Response::Inspection { request_id: request.request_id, body: inspection_body(runtime, request.execution_id, accepting.load(Ordering::Acquire)) }
            }
        }
        Request::RestartEnvironment(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else if !accepting.load(Ordering::Acquire) {
                Response::Error { request_id: Some(request.request_id), code: "REFRESHING".into(), message: "container is draining for a supervised process refresh".into() }
            } else if let Some(environment) = parse_environment(&request.environment).filter(|id| runtime.has_environment(*id)) {
                let requeued = runtime.restart_environment(environment);
                emit_event("environment_restart", &format!("environment={environment} requeued={requeued}"));
                Response::Restarted { request_id: request.request_id, environment: format!("{} ({} executions requeued)", environment, requeued) }
            } else {
                Response::Error { request_id: Some(request.request_id), code: "INVALID_ENVIRONMENT".into(), message: format!("container environment is unavailable: {}", request.environment) }
            }
        }
        Request::PrepareRefresh(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else {
                accepting.store(false, Ordering::Release);
                emit_event("refresh_drain_started", &format!("timeout_ms={}", request.drain_timeout_ms));
                let requested = Duration::from_millis(request.drain_timeout_ms.max(1_000));
                let timeout = requested.min(MAX_REFRESH_DRAIN);
                let started = Instant::now();
                while !runtime.is_idle() && started.elapsed() < timeout {
                    thread::sleep(Duration::from_millis(10));
                }
                if runtime.is_idle() {
                    emit_event("refresh_drain_ready", &format!("elapsed_ms={}", started.elapsed().as_millis()));
                    Response::ReadyForRefresh { request_id: request.request_id }
                } else {
                    accepting.store(true, Ordering::Release);
                    emit_event("refresh_drain_timeout", &format!("elapsed_ms={}", started.elapsed().as_millis()));
                    Response::Error { request_id: Some(request.request_id), code: "DRAIN_TIMEOUT".into(), message: "container could not drain all executions before the refresh deadline; normal execution resumed".into() }
                }
            }
        }
        Request::Resume(request) => {
            if request.auth_token != token {
                Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() }
            } else {
                accepting.store(true, Ordering::Release);
                emit_event("refresh_resume", "backend retained current container");
                Response::Resumed { request_id: request.request_id }
            }
        }
    };
    write_frame(&mut stream, &response)?;
    Ok(())
}

fn inspection_body(runtime: &Runtime, execution_id: Option<String>, accepting: bool) -> serde_json::Value {
    let snapshots = runtime.snapshots();
    let (swamps_total, workers_total, workers_busy, completed, failed) = topology_totals(&snapshots);
    let environments = snapshots.into_iter().map(|environment| {
        let swamps = environment.swamps.into_iter().map(|swamp| {
            let workers = swamp.workers.into_iter().map(|worker| serde_json::json!({
                "id": worker.id,
                "state": format!("{:?}", worker.state),
                "current_execution": worker.current.map(|id| id.to_string()),
                "completed": worker.completed,
                "failed": worker.failed,
                "total_ms": worker.total_ms,
                "average_ms": if worker.completed == 0 { 0.0 } else { worker.total_ms as f64 / worker.completed as f64 }
            })).collect::<Vec<_>>();
            serde_json::json!({
                "id": swamp.id,
                "queued": swamp.queued,
                "queued_cost": swamp.queued_cost,
                "throughput_per_sec": swamp.throughput_per_sec,
                "completed": swamp.completed,
                "failed": swamp.failed,
                "workers": workers
            })
        }).collect::<Vec<_>>();
        serde_json::json!({
            "id": environment.id.to_string(),
            "generation": environment.generation,
            "queued": environment.queued,
            "queued_cost": environment.queued_cost,
            "worker_count": environment.worker_count,
            "storage_limit_bytes": environment.storage_limit_bytes,
            "storage_ephemeral": environment.storage_ephemeral,
            "storage_path": environment.storage_path,
            "swamps": swamps
        })
    }).collect::<Vec<_>>();

    let cache_profiles = runtime.cache().profiles().into_iter().map(|(hash, profile)| serde_json::json!({
        "artifact_hash": hash,
        "samples": profile.samples,
        "total_ms": profile.total_ms,
        "last_ms": profile.last_ms,
        "max_ms": profile.max_ms,
        "average_ms": profile.average_ms(),
        "declared_cost": {
            "cpu": profile.declared_cost.cpu,
            "memory": profile.declared_cost.memory,
            "io": profile.declared_cost.io,
            "network": profile.declared_cost.network
        }
    })).collect::<Vec<_>>();

    serde_json::json!({
        "execution_id": execution_id,
        "pid": std::process::id(),
        "accepting_executions": accepting,
        "idle": runtime.is_idle(),
        "config": {
            "general_environments": runtime.config().general_environments,
            "payment_environments": 1,
            "total_environments": environments.len(),
            "swamps_per_environment": runtime.config().swamps_per_environment,
            "workers_per_swamp": runtime.config().workers_per_swamp,
            "rebalance_interval_ms": runtime.config().rebalance_interval_ms
        },
        "totals": {
            "swamps": swamps_total,
            "workers": workers_total,
            "workers_busy": workers_busy,
            "workers_idle": workers_total.saturating_sub(workers_busy),
            "global_queue": runtime.global_queue_len(),
            "completed": completed,
            "failed": failed
        },
        "cache": {
            "artifact_count": runtime.cache().artifact_count(),
            "profile_count": runtime.cache().len(),
            "profiles": cache_profiles,
            "durable_profiles": true,
            "durable_artifacts": true
        },
        "security": {
            "policy": "deny-by-default",
            "wasm": "wasmtime",
            "linux": "namespaces + no_new_privs + seccomp + cgroup-v2 + timeout"
        },
        "environments": environments
    })
}

fn topology_totals(snapshots: &[container_runtime_core::EnvironmentSnapshot]) -> (usize, usize, usize, u64, u64) {
    let mut swamps = 0usize;
    let mut workers = 0usize;
    let mut busy = 0usize;
    let mut completed = 0u64;
    let mut failed = 0u64;
    for environment in snapshots {
        swamps += environment.swamps.len();
        for swamp in &environment.swamps {
            workers += swamp.workers.len();
            busy += swamp.workers.iter().filter(|worker| worker.current.is_some()).count();
            completed = completed.saturating_add(swamp.completed);
            failed = failed.saturating_add(swamp.failed);
        }
    }
    (swamps, workers, busy, completed, failed)
}

fn parse_environment(value: &str) -> Option<EnvironmentId> {
    match value {
        "general-1" => Some(EnvironmentId::General1), "general-2" => Some(EnvironmentId::General2), "general-3" => Some(EnvironmentId::General3),
        "general-4" => Some(EnvironmentId::General4), "general-5" => Some(EnvironmentId::General5), "payment" => Some(EnvironmentId::Payment), _ => None,
    }
}

fn run_debug(args: &[String], runtime: &Runtime) -> anyhow::Result<()> {
    let demo_count = value_after(args, "--demo").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    for index in 0..demo_count {
        let cost = if index % 5 == 0 { WorkCost { cpu: 100, memory: 20, io: 5, network: 0 } } else { WorkCost { cpu: 10, memory: 2, io: 1, network: 0 } };
        let work_ms = if index % 5 == 0 { 40 } else { 5 };
        let environment = if index % 11 == 0 { EnvironmentId::Payment } else { EnvironmentId::General1 };
        let id = runtime.submit(environment, format!("demo-artifact-{}", index % 3), cost, work_ms);
        println!("queued {id} -> {environment}");
    }
    runtime.rebalance_once();
    println!("\nRBE CONTAINER RUNTIME — DEBUG");
    println!("environments={} swamps_per_environment={} workers_per_swamp={} global_queue={} cache_profiles={} artifacts={}", runtime.snapshots().len(), runtime.config().swamps_per_environment, runtime.config().workers_per_swamp, runtime.global_queue_len(), runtime.cache().len(), runtime.cache().artifact_count());
    for _ in 0..10 { print_snapshot(runtime); if demo_count == 0 { break; } thread::sleep(Duration::from_millis(250)); runtime.rebalance_once(); }
    Ok(())
}

fn print_snapshot(runtime: &Runtime) {
    for environment in runtime.snapshots() {
        println!("\nENVIRONMENT {:<9} gen={} queue={:<5} cost={:<6} swamps={} workers={} storage={}MB ephemeral={}", environment.id, environment.generation, environment.queued, environment.queued_cost, environment.swamps.len(), environment.worker_count, environment.storage_limit_bytes / (1024 * 1024), environment.storage_ephemeral);
        for swamp in environment.swamps {
            println!("  SWAMP {:03} queue={:<5} cost={:<6} throughput={:>8.1}/s completed={:<5} failed={:<5}", swamp.id, swamp.queued, swamp.queued_cost, swamp.throughput_per_sec, swamp.completed, swamp.failed);
            for worker in swamp.workers {
                let execution = worker.current.map(|id| id.to_string()).unwrap_or_else(|| "-".to_string());
                let avg_ms = if worker.completed == 0 { 0.0 } else { worker.total_ms as f64 / worker.completed as f64 };
                println!("    worker-{:<3} {:<7} current={:<40} completed={} failed={} avg_ms={:.1}", worker.id, format!("{:?}", worker.state), execution, worker.completed, worker.failed, avg_ms);
            }
        }
    }
    println!("CACHE profiles={} artifacts={}", runtime.cache().len(), runtime.cache().artifact_count());
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone())
}