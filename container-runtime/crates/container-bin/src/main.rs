//! Standalone `container` execution service.
//!
//! The backend launches this binary as a separate process. The service owns
//! Environment → Swamp → Worker scheduling and exposes a local authenticated
//! control socket. `--debug` renders the live topology without requiring the
//! backend process. `--worker` is a disposable child execution mode used by
//! the sandbox boundary and never starts the control plane. `--monitor` is a
//! small sibling process that watches the container supervisor and its event log.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use container_runtime_core::{EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost};
use execution_engine::{ExecutionLimits, WasmExecutor};
use ipc_protocol::{decode_request, read_frame, write_frame, Request, Response, PROTOCOL_VERSION};
use resource_limits::ResourceLimits;
use sandbox_primitives::{install_restricted_seccomp, set_no_new_privileges, SandboxPolicy};

const EVENT_LOG: &str = "./data/container-runtime/container-events.jsonl";
const MONITOR_LOG: &str = "./data/container-runtime/container-monitor.log";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.iter().any(|arg| arg == "--monitor") { return run_monitor(&args); }
    if args.iter().any(|arg| arg == "--worker") { return run_worker(&args); }

    let debug = args.iter().any(|arg| arg == "--debug");
    let listen = value_after(&args, "--listen");
    emit_event("container_start", &format!("pid={}")
        .replace("{}", &std::process::id().to_string()));

    if let Err(err) = spawn_monitor_process() {
        tracing::warn!(error = %err, "container monitor process could not be started");
    }

    let admin_dir = PathBuf::from("./data/admin");
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();

    let vault_data_dir = PathBuf::from("./data/container-admin");
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

    let registry = Arc::new(EnvironmentRegistry::new(io, vault, &vault_data_dir));
    let swamps_per_environment = value_after(&args, "--swamps-per-environment")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| RuntimeConfig::default().swamps_per_environment);
    let workers_per_swamp = value_after(&args, "--workers-per-swamp")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let runtime = Runtime::new(RuntimeConfig {
        swamps_per_environment,
        workers_per_swamp,
        rebalance_interval_ms: 25,
        ..RuntimeConfig::default()
    });

    if debug { run_debug(&args, &runtime)?; }
    if let Some(address) = listen {
        let token = env::var("RBE_CONTAINER_TOKEN").map_err(|_| anyhow::anyhow!("RBE_CONTAINER_TOKEN must be set when --listen is used"))?;
        run_control_server(&address, token, runtime.clone(), registry)?;
    } else if !debug {
        println!("container: no control socket requested; exiting after initialization");
    }
    Ok(())
}

fn spawn_monitor_process() -> anyhow::Result<()> {
    let exe = env::current_exe()?;
    let pid = std::process::id();
    std::process::Command::new(exe)
        .arg("--monitor")
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--events")
        .arg(EVENT_LOG)
        .arg("--log")
        .arg(MONITOR_LOG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(Into::into)
}

fn run_monitor(args: &[String]) -> anyhow::Result<()> {
    let watched_pid = value_after(args, "--pid")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| anyhow::anyhow!("monitor: --pid is required"))?;
    let event_path = PathBuf::from(value_after(args, "--events").unwrap_or_else(|| EVENT_LOG.to_string()));
    let monitor_path = PathBuf::from(value_after(args, "--log").unwrap_or_else(|| MONITOR_LOG.to_string()));
    if let Some(parent) = monitor_path.parent() { fs::create_dir_all(parent)?; }

    let mut last_event_len = 0_u64;
    loop {
        let alive = process_exists(watched_pid);
        if let Ok(metadata) = fs::metadata(&event_path) {
            if metadata.len() < last_event_len { last_event_len = 0; }
            if metadata.len() > last_event_len {
                if let Ok(mut file) = OpenOptions::new().read(true).open(&event_path) {
                    file.seek(SeekFrom::Start(last_event_len))?;
                    let mut appended = String::new();
                    file.read_to_string(&mut appended)?;
                    last_event_len = metadata.len();
                    let line = format!("{} container-event {}", now_ms(), appended.trim_end());
                    append_monitor_log(&monitor_path, &line)?;
                }
            }
        }
        if !alive {
            append_monitor_log(&monitor_path, &format!("{} container_process_exit pid={watched_pid}", now_ms()))?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}")]).output();
        output.map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())).unwrap_or(false)
    }
}

fn append_monitor_log(path: &PathBuf, line: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    use std::io::Write;
    writeln!(file, "{line}")?;
    Ok(())
}

fn emit_event(kind: &str, detail: &str) {
    if let Some(parent) = PathBuf::from(EVENT_LOG).parent() { let _ = fs::create_dir_all(parent); }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(EVENT_LOG) {
        let _ = writeln!(file, "{{\"ts\":{},\"pid\":{},\"kind\":{:?},\"detail\":{:?}}}", now_ms(), std::process::id(), kind, detail);
    }
}

fn now_ms() -> u128 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() }

fn run_worker(args: &[String]) -> anyhow::Result<()> {
    set_no_new_privileges().map_err(|e| anyhow::anyhow!("worker: failed to set no_new_privs: {e}"))?;
    install_restricted_seccomp().map_err(|e| anyhow::anyhow!("worker: failed to install seccomp: {e}"))?;
    let artifact = value_after(args, "--artifact").ok_or_else(|| anyhow::anyhow!("worker: --artifact is required"))?;
    if artifact.is_empty() || artifact.len() > 128 || !artifact.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') { anyhow::bail!("worker: invalid artifact hash"); }
    let path = PathBuf::from("./data/container-runtime/artifacts").join(format!("{artifact}.wasm"));
    let wasm = fs::read(path).map_err(|e| anyhow::anyhow!("worker: failed to read artifact: {e}"))?;
    let fuel = value_after(args, "--fuel").and_then(|value| value.parse::<u64>().ok()).unwrap_or(10_000_000);
    let max_memory_bytes = value_after(args, "--memory").and_then(|value| value.parse::<u64>().ok()).unwrap_or(64 * 1024 * 1024);
    let executor = WasmExecutor::new()?;
    let result = executor.execute(&wasm, ExecutionLimits { fuel, max_memory_bytes })?;
    if result.exit_code != 0 { anyhow::bail!("worker: WASM exited with status {}", result.exit_code); }
    Ok(())
}

fn run_control_server(address: &str, token: String, runtime: Arc<Runtime>, registry: Arc<EnvironmentRegistry>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("container: control socket listening on {}", listener.local_addr()?);
    emit_event("control_listening", &address.to_string());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = token.clone();
                let runtime = Arc::clone(&runtime);
                let registry = Arc::clone(&registry);
                thread::spawn(move || { if let Err(err) = handle_connection(stream, &token, &runtime, &registry) { tracing::warn!(%err, "container control connection closed with error"); } });
            }
            Err(err) => tracing::warn!(%err, "failed to accept container control connection"),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, token: &str, runtime: &Runtime, registry: &EnvironmentRegistry) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = decode_request(&read_frame(&mut reader)?)?;
    let response = match request {
        Request::Hello(hello) => if hello.version != PROTOCOL_VERSION || hello.auth_token != token { Response::Error { request_id: None, code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } } else { Response::HelloAccepted { version: PROTOCOL_VERSION } },
        Request::Execute(request) => {
            if request.auth_token != token { Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } }
            else if let Some(environment) = parse_environment(&request.environment) {
                let cost = WorkCost { cpu: request.declared_cost.cpu, memory: request.declared_cost.memory, io: request.declared_cost.io, network: request.declared_cost.network };
                let execution_id = runtime.submit_with_policy(environment, request.artifact_hash, cost, ResourceLimits::default(), SandboxPolicy::default(), 0, request.payload);
                emit_event("execution_accepted", &execution_id.to_string());
                Response::Accepted { request_id: request.request_id, execution_id: execution_id.to_string() }
            } else { Response::Error { request_id: Some(request.request_id), code: "INVALID_ENVIRONMENT".into(), message: format!("unknown container environment: {}", request.environment) } }
        }
        Request::Health(request) => if request.auth_token != token { Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } } else { Response::Health { request_id: request.request_id, body: serde_json::json!({ "protocol": PROTOCOL_VERSION, "process": "container", "environments": registry.health_snapshot().len(), "queue": runtime.global_queue_len(), "sandbox_policy": "deny-by-default", "wasm_engine": "wasmtime", "artifact_cache": runtime.cache().artifact_count() }) } },
        Request::Cancel(request) => {
            if request.auth_token != token { Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } }
            else if runtime.cancel(&request.execution_id) { emit_event("execution_cancel", &request.execution_id); Response::Cancelled { request_id: request.request_id } }
            else { Response::Error { request_id: Some(request.request_id), code: "NOT_FOUND".into(), message: "execution ID was not found".into() } }
        }
        Request::Inspect(request) => {
            if request.auth_token != token { Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } }
            else {
                let environments = runtime.snapshots();
                let body = environments.iter().map(|environment| serde_json::json!({ "id": environment.id.to_string(), "generation": environment.generation, "queued": environment.queued, "queued_cost": environment.queued_cost, "swamps": environment.swamps.len(), "workers": environment.worker_count, "storage_limit_bytes": environment.storage_limit_bytes, "storage_ephemeral": environment.storage_ephemeral, "storage_path": environment.storage_path })).collect::<Vec<_>>();
                Response::Inspection { request_id: request.request_id, body: serde_json::json!({ "execution_id": request.execution_id, "environments": body }) }
            }
        }
        Request::RestartEnvironment(request) => {
            if request.auth_token != token { Response::Error { request_id: Some(request.request_id), code: "AUTH_FAILED".into(), message: "container control authentication failed".into() } }
            else if let Some(environment) = parse_environment(&request.environment) {
                let requeued = runtime.restart_environment(environment);
                emit_event("environment_restart", &format!("environment={environment} requeued={requeued}"));
                Response::Restarted { request_id: request.request_id, environment: format!("{} ({} executions requeued)", environment, requeued) }
            } else { Response::Error { request_id: Some(request.request_id), code: "INVALID_ENVIRONMENT".into(), message: format!("unknown container environment: {}", request.environment) } }
        }
    };
    write_frame(&mut stream, &response)?;
    Ok(())
}

fn parse_environment(value: &str) -> Option<EnvironmentId> { match value { "general-1" => Some(EnvironmentId::General1), "general-2" => Some(EnvironmentId::General2), "general-3" => Some(EnvironmentId::General3), "general-4" => Some(EnvironmentId::General4), "general-5" => Some(EnvironmentId::General5), "payment" => Some(EnvironmentId::Payment), _ => None } }

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

fn value_after(args: &[String], flag: &str) -> Option<String> { args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone()) }
