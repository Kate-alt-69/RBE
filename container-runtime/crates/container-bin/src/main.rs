//! Standalone `container` execution service.
//!
//! The backend launches this binary as a separate process. The service owns
//! Environment → Swamp → Worker scheduling and exposes a local authenticated
//! control socket. `--debug` renders the live topology without requiring the
//! backend process.

use std::env;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use container_runtime_core::{EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost};
use ipc_protocol::{decode_request, read_frame, write_frame, Request, Response, PROTOCOL_VERSION};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = env::args().skip(1).collect::<Vec<_>>();
    let debug = args.iter().any(|arg| arg == "--debug");
    let listen = value_after(&args, "--listen");

    let admin_dir = PathBuf::from("./data/admin");
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();

    let vault_data_dir = PathBuf::from("./data/container-admin");
    let vault = match vault::Vault::new(io.clone(), "backend-rs-container", &vault_data_dir) {
        Ok(v) => Arc::new(v),
        Err(e) => {
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

    let registry = EnvironmentRegistry::new(io, vault, &vault_data_dir);
    let swamps_per_environment = value_after(&args, "--swamps-per-environment")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8));
    let workers_per_swamp = value_after(&args, "--workers-per-swamp")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let runtime = Runtime::new(RuntimeConfig {
        swamps_per_environment,
        workers_per_swamp,
        rebalance_interval_ms: 25,
    });

    if debug {
        run_debug(&args, &runtime)?;
    }

    if let Some(address) = listen {
        let token = env::var("RBE_CONTAINER_TOKEN")
            .map_err(|_| anyhow::anyhow!("RBE_CONTAINER_TOKEN must be set when --listen is used"))?;
        run_control_server(&address, token, runtime.clone(), registry)?;
    } else if !debug {
        println!("container: no control socket requested; exiting after initialization");
    }

    Ok(())
}

fn run_control_server(
    address: &str,
    token: String,
    runtime: Arc<Runtime>,
    registry: EnvironmentRegistry,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("container: control socket listening on {}", listener.local_addr()?);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = token.clone();
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &token, &runtime, &registry) {
                        tracing::warn!(%err, "container control connection closed with error");
                    }
                });
            }
            Err(err) => tracing::warn!(%err, "failed to accept container control connection"),
        }
    }
    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    token: &str,
    runtime: &Runtime,
    registry: &EnvironmentRegistry,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let bytes = read_frame(&mut reader)?;
    let request = decode_request(&bytes)?;

    let response = match request {
        Request::Hello(hello) => {
            if hello.version != PROTOCOL_VERSION || hello.auth_token != token {
                Response::Error {
                    request_id: None,
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                Response::HelloAccepted { version: PROTOCOL_VERSION }
            }
        }
        Request::Execute(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                let Some(environment) = parse_environment(&request.environment) else {
                    Response::Error {
                        request_id: Some(request.request_id),
                        code: "INVALID_ENVIRONMENT".into(),
                        message: format!("unknown container environment: {}", request.environment),
                    }
                }
            }
        }
        Request::Health(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                Response::Health {
                    request_id: request.request_id,
                    body: serde_json::json!({
                        "protocol": PROTOCOL_VERSION,
                        "process": "container",
                        "environments": registry.health_snapshot().len(),
                        "queue": runtime.global_queue_len(),
                        "runtime": runtime.snapshots().len(),
                        "sandbox_policy": "deny-by-default",
                        "wasm_engine": "pending-execution-engine"
                    }),
                }
            }
        }
        Request::Cancel(request) => Response::Error {
            request_id: Some(request.request_id),
            code: "NOT_IMPLEMENTED".into(),
            message: "execution cancellation requires the execution control table".into(),
        },
        Request::Inspect(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                let environments = runtime.snapshots();
                Response::Inspection {
                    request_id: request.request_id,
                    body: serde_json::json!({
                        "execution_id": request.execution_id,
                        "environments": environments.len(),
                        "state": environments.iter().map(|env| serde_json::json!({
                            "id": env.id.to_string(),
                            "queued": env.queued,
                            "queued_cost": env.queued_cost,
                            "swamps": env.swamps.len()
                        })).collect::<Vec<_>>()
                    }),
                }
            }
        }
        Request::RestartEnvironment(request) => Response::Error {
            request_id: Some(request.request_id),
            code: "NOT_IMPLEMENTED".into(),
            message: "environment restart requires the supervisor lifecycle layer".into(),
        },
    };

    write_frame(&mut stream, &response)?;
    Ok(())
}

fn parse_environment(value: &str) -> Option<EnvironmentId> {
    match value {
        "general-1" => Some(EnvironmentId::General1),
        "general-2" => Some(EnvironmentId::General2),
        "general-3" => Some(EnvironmentId::General3),
        "general-4" => Some(EnvironmentId::General4),
        "general-5" => Some(EnvironmentId::General5),
        "payment" => Some(EnvironmentId::Payment),
        _ => None,
    }
}

fn run_debug(args: &[String], runtime: &Runtime) -> anyhow::Result<()> {
    let demo_count = value_after(args, "--demo")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    for index in 0..demo_count {
        let cost = if index % 5 == 0 {
            WorkCost { cpu: 100, memory: 20, io: 5, network: 0 }
        } else {
            WorkCost { cpu: 10, memory: 2, io: 1, network: 0 }
        };
        let work_ms = if index % 5 == 0 { 40 } else { 5 };
        let environment = if index % 11 == 0 { EnvironmentId::Payment } else { EnvironmentId::General1 };
        let id = runtime.submit(environment, format!("demo-artifact-{}", index % 3), cost, work_ms);
        println!("queued {id} -> {environment}");
    }

    runtime.rebalance_once();
    println!("\nRBE CONTAINER RUNTIME — DEBUG");
    println!("environments={} swamps_per_environment={} workers_per_swamp={} global_queue={} cache_profiles={}",
        runtime.snapshots().len(), runtime.config().swamps_per_environment,
        runtime.config().workers_per_swamp, runtime.global_queue_len(), runtime.cache().len());

    for _ in 0..10 {
        print_snapshot(runtime);
        if demo_count == 0 { break; }
        thread::sleep(Duration::from_millis(250));
        runtime.rebalance_once();
    }
    Ok(())
}

fn print_snapshot(runtime: &Runtime) {
    for environment in runtime.snapshots() {
        println!("\nENVIRONMENT {:<9} queue={:<5} cost={:<6} swamps={}", environment.id, environment.queued, environment.queued_cost, environment.swamps.len());
        for swamp in environment.swamps {
            println!("  SWAMP {:03} queue={:<5} cost={:<6} throughput={:>8.1}/s completed={:<5}", swamp.id, swamp.queued, swamp.queued_cost, swamp.throughput_per_sec, swamp.completed);
            for worker in swamp.workers {
                let execution = worker.current.map(|id| id.to_string()).unwrap_or_else(|| "-".to_string());
                let avg_ms = if worker.completed == 0 { 0.0 } else { worker.total_ms as f64 / worker.completed as f64 };
                println!("    worker-{:<3} {:<7} current={:<36} completed={} avg_ms={:.1}", worker.id, format!("{:?}", worker.state), execution, worker.completed, avg_ms);
            }
        }
    }
    println!("CACHE profiles={}", runtime.cache().len());
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone())
}
