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

    if debug {
        run_debug(&args)?;
    }

    if let Some(address) = listen {
        let token = env::var("RBE_CONTAINER_TOKEN")
            .map_err(|_| anyhow::anyhow!("RBE_CONTAINER_TOKEN must be set when --listen is used"))?;
        run_control_server(&address, token, registry)?;
    } else if !debug {
        println!("container: no control socket requested; exiting after initialization");
    }

    Ok(())
}

fn run_control_server(address: &str, token: String, _registry: EnvironmentRegistry) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("container: control socket listening on {}", listener.local_addr()?);

    // The socket is intentionally loopback-only in normal backend use. The
    // authentication token is still mandatory because another local process
    // must not gain control merely by discovering the port.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = token.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &token) {
                        tracing::warn!(%err, "container control connection closed with error");
                    }
                });
            }
            Err(err) => tracing::warn!(%err, "failed to accept container control connection"),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, token: &str) -> anyhow::Result<()> {
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
                // Execution dispatch is owned by the runtime process. The
                // actual WASM engine and OS sandbox are deliberately behind
                // the execution-engine/sandbox-primitives contracts.
                Response::Accepted {
                    request_id: request.request_id,
                    execution_id: "pending-runtime-wiring".into(),
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
                        "sandbox": "policy-bound",
                        "wasm": "pending-execution-engine"
                    }),
                }
            }
        }
        Request::Cancel(request) => Response::Error {
            request_id: Some(request.request_id),
            code: "NOT_IMPLEMENTED".into(),
            message: "execution cancellation requires the execution engine wiring".into(),
        },
        Request::Inspect(request) => Response::Error {
            request_id: Some(request.request_id),
            code: "NOT_IMPLEMENTED".into(),
            message: "live inspection requires the runtime handle".into(),
        },
        Request::RestartEnvironment(request) => Response::Error {
            request_id: Some(request.request_id),
            code: "NOT_IMPLEMENTED".into(),
            message: "environment restart requires the supervisor lifecycle wiring".into(),
        },
    };

    write_frame(&mut stream, &response)?;
    Ok(())
}

fn run_debug(args: &[String]) -> anyhow::Result<()> {
    let swamps_per_environment = value_after(args, "--swamps-per-environment")
        .or_else(|| value_after(args, "--swamps"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8));
    let workers = value_after(args, "--workers-per-swamp")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let demo_count = value_after(args, "--demo")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    let runtime = Runtime::new(RuntimeConfig {
        swamps_per_environment,
        workers_per_swamp: workers,
        rebalance_interval_ms: 25,
    });

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
        print_snapshot(&runtime);
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
