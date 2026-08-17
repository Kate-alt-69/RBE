//! Container runtime entrypoint.
//!
//! Normal mode keeps the existing health/registry wiring. `--debug` starts
//! the testable Swamp runtime and renders live Environment/Swamp/Worker/cache
//! state. This is scheduler/debug infrastructure only; kernel sandboxing,
//! real WASM execution, and authenticated IPC remain future layers.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use container_runtime_core::{EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = env::args().skip(1).collect::<Vec<_>>();
    let debug = args.iter().any(|arg| arg == "--debug");

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

    println!("container-bin: environment health snapshot:");
    for (id, status) in registry.health_snapshot() {
        println!("  {id:<10} {status:?}");
    }

    if debug {
        return run_debug(&args);
    }

    println!("container-bin: normal mode — Swamp debug scheduler disabled; no OS sandbox/IPC yet.");
    Ok(())
}

fn run_debug(args: &[String]) -> anyhow::Result<()> {
    let swamps = value_after(args, "--swamps")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8));
    let workers = value_after(args, "--workers-per-swamp")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let demo_count = value_after(args, "--demo")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    let runtime = Runtime::new(RuntimeConfig {
        swamps,
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
        let id = runtime.submit(format!("demo-artifact-{}", index % 3), cost, work_ms);
        println!("queued {id}");
    }

    runtime.rebalance_once();
    println!("\nRBE CONTAINER RUNTIME — DEBUG");
    println!("swamps={} workers_per_swamp={} global_queue={} cache_profiles={}",
        runtime.config().swamps,
        runtime.config().workers_per_swamp,
        runtime.global_queue_len(),
        runtime.cache().len());

    for _ in 0..10 {
        print_snapshot(&runtime);
        if demo_count == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(250));
        runtime.rebalance_once();
    }

    Ok(())
}

fn print_snapshot(runtime: &Runtime) {
    println!("\nENVIRONMENT  debug-environment");
    for swamp in runtime.snapshots() {
        println!(
            "  SWAMP {:03} queue={:<5} cost={:<6} throughput={:>8.1}/s completed={:<5}",
            swamp.id, swamp.queued, swamp.queued_cost, swamp.throughput_per_sec, swamp.completed
        );
        for worker in swamp.workers {
            let execution = worker.current.map(|id| id.to_string()).unwrap_or_else(|| "-".to_string());
            let avg_ms = if worker.completed == 0 {
                0.0
            } else {
                worker.total_ms as f64 / worker.completed as f64
            };
            println!(
                "    worker-{:<3} {:<7} current={:<36} completed={} avg_ms={:.1}",
                worker.id,
                format!("{:?}", worker.state),
                execution,
                worker.completed,
                avg_ms
            );
        }
    }
    println!("  CACHE profiles={}", runtime.cache().len());
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone())
}
