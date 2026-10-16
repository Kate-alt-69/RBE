//! Phase 6 native Service process benchmark.
//!
//! This mode deliberately runs only from a disposable copied binary root. Native
//! cutover owns OID/compiler cache state relative to the executable directory,
//! so benchmarking a normal installation in place would pollute its cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::Value;

const SCHEMA: &str = "RBE-NATIVE-SERVICE-PROCESS-BENCH/1";
const BENCH_MARKER: &str = ".rbe-native-bench-root";
const SERVICE_NAME: &str = "bench_native";
const DEFAULT_ITERATIONS: usize = 5;
const DEFAULT_WARM_CALLS: usize = 20;

#[derive(Debug)]
struct Args {
    profile: String,
    iterations: usize,
    warm_calls: usize,
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MachineInfo {
    os: String,
    arch: String,
    target: String,
    logical_parallelism: usize,
    cpu_model: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricSummary {
    name: &'static str,
    samples: usize,
    min_ns: u128,
    median_ns: u128,
    p95_ns: u128,
    max_ns: u128,
    mean_ns: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RssObservations {
    mother_post_ready_kib: Vec<u64>,
    worker_post_cold_call_kib: Vec<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessBenchReport {
    schema: &'static str,
    profile: String,
    iterations: usize,
    warm_calls_per_round: usize,
    machine: MachineInfo,
    native_service_count: usize,
    metrics: Vec<MetricSummary>,
    rss_observations: RssObservations,
    unmeasured: Vec<&'static str>,
}

struct Fixture {
    service_dir: PathBuf,
    settings_path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.settings_path);
        let _ = fs::remove_dir_all(&self.service_dir);
    }
}

pub async fn run(raw_args: &[String]) -> anyhow::Result<()> {
    let args = parse_args(raw_args)?;
    let root = runtime_paths::binary_dir();
    require_disposable_root(&root)?;
    let fixture = create_fixture(&root)?;
    let config = config::Config::load(&fixture.settings_path)
        .with_context(|| format!("load native Service benchmark settings {}", fixture.settings_path.display()))?;
    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile_from_root(&config.services, &io, &root)?
        .ok_or_else(|| anyhow::anyhow!("native Service benchmark fixture produced no Service catalog"))?;
    let image = crate::runtime_image_boot::compile(&config, Some(&catalog))?;
    let cutover = crate::service_native_cutover::prepare_native_service_cutover(&image, Some(&catalog))?
        .ok_or_else(|| anyhow::anyhow!("benchmark Service did not enter the native execution subset"))?;
    if cutover.native_service_count() != 1 {
        bail!(
            "benchmark fixture expected exactly one native Service, observed {}",
            cutover.native_service_count()
        );
    }

    let mut runtime_env = image.environment.to_json();
    crate::service_native_cutover::attach_native_service_cutover(&mut runtime_env, Some(&cutover))?;
    let runtime_env = Arc::new(runtime_env);

    let mut mother_start_ns = Vec::with_capacity(args.iterations);
    let mut cold_call_ns = Vec::with_capacity(args.iterations);
    let mut warm_call_ns = Vec::with_capacity(args.iterations.saturating_mul(args.warm_calls));
    let mut mother_rss = Vec::with_capacity(args.iterations);
    let mut worker_rss = Vec::with_capacity(args.iterations);

    for _ in 0..args.iterations {
        let mother_started = Instant::now();
        let mother = crate::service_mother::spawn(
            &fixture.settings_path,
            &catalog.fingerprint(),
            runtime_env.clone(),
            None,
            crate::service_integrity::EXPECTED_SERVICE_SHA256,
        )
        .await?;
        mother_start_ns.push(mother_started.elapsed().as_nanos());
        if let Some(rss) = process_rss_kib(mother.pid()) {
            mother_rss.push(rss);
        }

        let manager = mother.manager();
        let before = manager.snapshot().await;
        let dormant = before
            .iter()
            .find(|snapshot| snapshot.name == SERVICE_NAME)
            .ok_or_else(|| anyhow::anyhow!("benchmark Service missing from Mother snapshot"))?;
        if dormant.pid.is_some() {
            bail!("OnDemand benchmark Service was already running before the cold call");
        }

        let cold_started = Instant::now();
        let cold_value = manager.call(SERVICE_NAME, "ready", Vec::new()).await?;
        cold_call_ns.push(cold_started.elapsed().as_nanos());
        ensure_true(cold_value, "cold native Service call")?;

        let after = manager.snapshot().await;
        let worker_pid = after
            .iter()
            .find(|snapshot| snapshot.name == SERVICE_NAME)
            .and_then(|snapshot| snapshot.pid)
            .ok_or_else(|| anyhow::anyhow!("native Service worker has no PID after cold activation"))?;
        if let Some(rss) = process_rss_kib(worker_pid) {
            worker_rss.push(rss);
        }

        for _ in 0..args.warm_calls {
            let warm_started = Instant::now();
            let value = manager.call(SERVICE_NAME, "ready", Vec::new()).await?;
            warm_call_ns.push(warm_started.elapsed().as_nanos());
            ensure_true(value, "warm native Service call")?;
        }

        mother.shutdown(Duration::from_secs(5)).await;
    }

    let report = ProcessBenchReport {
        schema: SCHEMA,
        profile: args.profile,
        iterations: args.iterations,
        warm_calls_per_round: args.warm_calls,
        machine: machine_info(),
        native_service_count: cutover.native_service_count(),
        metrics: vec![
            summarize("service_mother_startup", &mother_start_ns)?,
            summarize("cold_native_service_call", &cold_call_ns)?,
            summarize("warm_native_service_call", &warm_call_ns)?,
        ],
        rss_observations: RssObservations {
            mother_post_ready_kib: mother_rss,
            worker_post_cold_call_kib: worker_rss,
        },
        unmeasured: vec![
            "backend_process_startup_latency",
            "true_peak_process_rss",
            "oid_index_package_update_latency",
        ],
    };

    let encoded = serde_json::to_string_pretty(&report)?;
    if let Some(output) = args.output {
        if let Some(parent) = output.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("create benchmark output directory {}", parent.display()))?;
        }
        fs::write(&output, format!("{encoded}\n"))
            .with_context(|| format!("write benchmark report {}", output.display()))?;
    }
    println!("{encoded}");
    Ok(())
}

fn parse_args(raw: &[String]) -> anyhow::Result<Args> {
    fn value(raw: &[String], flag: &str) -> Option<String> {
        raw.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    }
    let parse_positive = |flag: &str, default: usize| -> anyhow::Result<usize> {
        match value(raw, flag) {
            Some(value) => {
                let parsed = value
                    .parse::<usize>()
                    .with_context(|| format!("{flag} requires a positive integer"))?;
                if parsed == 0 {
                    bail!("{flag} requires a positive integer");
                }
                Ok(parsed)
            }
            None => Ok(default),
        }
    };
    Ok(Args {
        profile: value(raw, "--profile").unwrap_or_else(|| "local".into()),
        iterations: parse_positive("--iterations", DEFAULT_ITERATIONS)?,
        warm_calls: parse_positive("--warm-calls", DEFAULT_WARM_CALLS)?,
        output: value(raw, "--output").map(PathBuf::from),
    })
}

fn require_disposable_root(root: &Path) -> anyhow::Result<()> {
    let marker = root.join(BENCH_MARKER);
    if !marker.is_file() {
        bail!(
            "native Service process benchmark refuses to mutate a normal RBE binary root; copy the packaged backend + dep/service into a disposable directory and create {} beside backend before running the benchmark",
            marker.display()
        );
    }
    Ok(())
}

fn create_fixture(root: &Path) -> anyhow::Result<Fixture> {
    let nonce = format!("{}-{}", std::process::id(), now_nanos());
    let service_leaf = format!(".bench-native-service-{nonce}");
    let service_dir = root.join(&service_leaf);
    fs::create_dir_all(&service_dir)
        .with_context(|| format!("create benchmark Service directory {}", service_dir.display()))?;
    let source = r#":service[
  name = bench_native,
  mode = on-demand,
  restart = never,
  memoryLimitMb = 64,
  startupTimeoutMs = 10000,
  idleTimeoutMs = 60000,
  instances = 1
]
export function ready() { return true; }
"#;
    fs::write(service_dir.join("bench_native.service"), source)?;

    let settings_path = root.join(format!(".bench-native-settings-{nonce}.json"));
    let settings = serde_json::json!({
        "api": {"host": "127.0.0.1", "port": 65534},
        "services": {
            "enabled": true,
            "directory": service_leaf,
            "defaultMemoryLimitMb": 64,
            "startupTimeoutMs": 10000,
            "defaultIdleTimeoutMs": 60000,
            "monitorIntervalMs": 50,
            "maxRestartBackoffMs": 1000
        },
        "videoManager": {"enabled": false},
        "dashboards": {"enabled": false, "autoOpen": false, "adminPathPrefix": "/admin"}
    });
    fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(Fixture {
        service_dir,
        settings_path,
    })
}

fn ensure_true(value: Value, label: &str) -> anyhow::Result<()> {
    if value == Value::Bool(true) {
        Ok(())
    } else {
        bail!("{label} returned {value:?}; expected true from the native Boolean fixture")
    }
}

fn summarize(name: &'static str, values: &[u128]) -> anyhow::Result<MetricSummary> {
    if values.is_empty() {
        bail!("benchmark metric {name} has no samples");
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let percentile = |percent: usize| {
        let index = ((sorted.len() - 1) * percent + 99) / 100;
        sorted[index.min(sorted.len() - 1)]
    };
    let total = sorted.iter().fold(0u128, |total, value| total.saturating_add(*value));
    Ok(MetricSummary {
        name,
        samples: sorted.len(),
        min_ns: sorted[0],
        median_ns: percentile(50),
        p95_ns: percentile(95),
        max_ns: *sorted.last().expect("non-empty benchmark samples"),
        mean_ns: total as f64 / sorted.len() as f64,
    })
}

fn machine_info() -> MachineInfo {
    MachineInfo {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        target: route_engine::OidTarget::current().label(),
        logical_parallelism: std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1),
        cpu_model: cpu_model(),
    }
}

fn cpu_model() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let content = fs::read_to_string("/proc/cpuinfo").ok()?;
        return content.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if matches!(key.trim(), "model name" | "Processor" | "Hardware") {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_string())
            } else {
                None
            }
        });
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var("PROCESSOR_IDENTIFIER").ok();
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (!value.is_empty()).then_some(value);
    }
    #[allow(unreachable_code)]
    None
}

fn process_rss_kib(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        return status.lines().find_map(|line| {
            let rest = line.strip_prefix("VmRSS:")?;
            rest.split_whitespace().next()?.parse().ok()
        });
    }
    #[cfg(target_os = "windows")]
    {
        let script = format!("(Get-Process -Id {pid}).WorkingSet64");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        let bytes = String::from_utf8_lossy(&output.stdout).trim().parse::<u64>().ok()?;
        return Some(bytes / 1024);
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        return String::from_utf8_lossy(&output.stdout).trim().parse().ok();
    }
    #[allow(unreachable_code)]
    None
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_stable_and_order_independent() {
        let summary = summarize("x", &[9, 1, 5, 3, 7]).unwrap();
        assert_eq!(summary.samples, 5);
        assert_eq!(summary.min_ns, 1);
        assert_eq!(summary.median_ns, 5);
        assert_eq!(summary.p95_ns, 9);
        assert_eq!(summary.max_ns, 9);
        assert_eq!(summary.mean_ns, 5.0);
    }

    #[test]
    fn benchmark_requires_a_disposable_binary_root_marker() {
        let root = std::env::temp_dir().join(format!("rbe-native-process-bench-marker-{}", now_nanos()));
        fs::create_dir_all(&root).unwrap();
        assert!(require_disposable_root(&root).is_err());
        fs::write(root.join(BENCH_MARKER), b"disposable\n").unwrap();
        assert!(require_disposable_root(&root).is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
