use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use route_engine::service_bin::{
    assemble_service_bin, decode_cached_service_bin, encode_cached_service_bin,
    AssemblyRecordKind, RequiredOid, ServiceAssemblyPlan, VerifiedAssemblyOidRecord, DONE_OID,
    SERVICE_PLAN_FORMAT,
};
use route_engine::{
    discover_linked_rel_symbols, optimize_function, BinaryOp, Expr, FunctionDef, ImportTarget,
    LinkedRelSourceUnit, ModuleFile, OidTarget, ServiceProgram, Statement,
};
use serde::Serialize;

const BENCH_SCHEMA: &str = "RBE-REL-NATIVE-BENCH/1";

#[derive(Debug, Clone)]
struct BenchConfig {
    profile: String,
    iterations: u64,
    warmup_iterations: u64,
    pretty: bool,
    output: Option<PathBuf>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            profile: match std::env::consts::ARCH {
                "x86_64" => "baseline-x86_64".into(),
                "aarch64" => "baseline-arm64".into(),
                other => format!("baseline-{other}"),
            },
            iterations: 500,
            warmup_iterations: 50,
            pretty: false,
            output: None,
        }
    }
}

impl BenchConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.profile.trim().is_empty() || self.profile.chars().any(char::is_control) {
            anyhow::bail!("benchmark profile must be a non-empty printable label");
        }
        if self.iterations == 0 {
            anyhow::bail!("benchmark iterations must be greater than zero");
        }
        if self.warmup_iterations > 100_000 || self.iterations > 10_000_000 {
            anyhow::bail!("benchmark iteration count is unreasonably large");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchReport {
    schema: &'static str,
    profile: String,
    host: BenchHost,
    memory: BenchMemory,
    metrics: Vec<BenchMetric>,
    artifacts: BenchArtifacts,
    unmeasured: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchHost {
    os: String,
    arch: String,
    pointer_width: u8,
    target_fingerprint: String,
    logical_parallelism: Option<usize>,
    cpu_model: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchMemory {
    rss_kib_before: Option<u64>,
    rss_kib_after: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchMetric {
    name: &'static str,
    iterations: u64,
    total_ns: u64,
    ns_per_iteration: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchArtifacts {
    optimized_statement_count: usize,
    reachable_symbol_count: usize,
    service_bin_payload_bytes: usize,
    encoded_service_bin_bytes: usize,
}

fn main() -> anyhow::Result<()> {
    let config = parse_args()?;
    config.validate()?;
    let report = run(&config)?;
    let json = if config.pretty {
        serde_json::to_string_pretty(&report)?
    } else {
        serde_json::to_string(&report)?
    };

    if let Some(path) = &config.output {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{json}\n"))?;
    } else {
        println!("{json}");
    }
    Ok(())
}

fn parse_args() -> anyhow::Result<BenchConfig> {
    let mut config = BenchConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                config.profile = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--profile requires a value"))?;
            }
            "--iterations" => {
                config.iterations = parse_u64("--iterations", args.next())?;
            }
            "--warmup" => {
                config.warmup_iterations = parse_u64("--warmup", args.next())?;
            }
            "--output" => {
                config.output = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--output requires a path"))?,
                ));
            }
            "--pretty" => config.pretty = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown rel-native-bench argument {other:?}; use --help"),
        }
    }
    Ok(config)
}

fn parse_u64(flag: &str, value: Option<String>) -> anyhow::Result<u64> {
    let value = value.ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
    value
        .parse::<u64>()
        .map_err(|error| anyhow::anyhow!("invalid {flag} value {value:?}: {error}"))
}

fn print_help() {
    println!(
        "rel-native-bench - Phase 6 REL native-path microbenchmark\n\n\
Usage:\n  cargo run -p route-engine --release --bin rel-native-bench -- [options]\n\n\
Options:\n  --profile <label>      Result profile tag (default: baseline-<arch>)\n  --iterations <count>   Measured iterations per component (default: 500)\n  --warmup <count>       Warmup iterations per component (default: 50)\n  --output <path>        Write JSON report to a file instead of stdout\n  --pretty               Pretty-print JSON\n  -h, --help             Show this help\n\n\
Suggested low-end profile labels:\n  old-x86_64             Older baseline x86-64 CPUs (for example 6th/7th-gen Intel)\n  low-end-arm64          Low-end ARM64 SBC/server cores\n"
    );
}

fn run(config: &BenchConfig) -> anyhow::Result<BenchReport> {
    let target = OidTarget::current();
    let optimizer_fixture = optimizer_fixture();
    let (module, service) = discovery_fixture();
    let discovery_units = [
        LinkedRelSourceUnit::module("bench", "bench-module-source", &module),
        LinkedRelSourceUnit::service("bench", "bench-service-source", &service),
    ];
    let (plan, records) = assembly_fixture(&target.label());
    let assembled = assemble_service_bin(&plan, &records).context("assemble benchmark Service bin")?;
    let encoded = encode_cached_service_bin(&assembled).context("encode benchmark Service bin")?;

    let rss_kib_before = current_rss_kib();

    warm(config.warmup_iterations, || {
        black_box(optimize_function(black_box(&optimizer_fixture)));
    });
    let optimizer = measure("rel_optimizer", config.iterations, || {
        black_box(optimize_function(black_box(&optimizer_fixture)));
    });

    warm(config.warmup_iterations, || {
        black_box(discover_linked_rel_symbols(black_box(&discovery_units)).unwrap());
    });
    let reachability = measure("linked_rel_reachability", config.iterations, || {
        black_box(discover_linked_rel_symbols(black_box(&discovery_units)).unwrap());
    });

    warm(config.warmup_iterations, || {
        black_box(assemble_service_bin(black_box(&plan), black_box(&records)).unwrap());
    });
    let assembly = measure("service_bin_assembly", config.iterations, || {
        black_box(assemble_service_bin(black_box(&plan), black_box(&records)).unwrap());
    });

    warm(config.warmup_iterations, || {
        black_box(decode_cached_service_bin(black_box(&encoded)).unwrap());
    });
    let cache_read = measure("service_bin_cache_decode", config.iterations, || {
        black_box(decode_cached_service_bin(black_box(&encoded)).unwrap());
    });

    let optimized_statement_count = optimize_function(&optimizer_fixture).0.body.len();
    let reachable_symbol_count = discover_linked_rel_symbols(&discovery_units)
        .context("discover benchmark linked REL symbols")?
        .symbols
        .len();
    let rss_kib_after = current_rss_kib();

    Ok(BenchReport {
        schema: BENCH_SCHEMA,
        profile: config.profile.clone(),
        host: BenchHost {
            os: target.os.clone(),
            arch: target.arch.clone(),
            pointer_width: target.pointer_width,
            target_fingerprint: target.label(),
            logical_parallelism: std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get),
            cpu_model: cpu_model(),
        },
        memory: BenchMemory {
            rss_kib_before,
            rss_kib_after,
        },
        metrics: vec![optimizer, reachability, assembly, cache_read],
        artifacts: BenchArtifacts {
            optimized_statement_count,
            reachable_symbol_count,
            service_bin_payload_bytes: assembled.payload.len(),
            encoded_service_bin_bytes: encoded.len(),
        },
        unmeasured: vec![
            "backend_process_startup_latency",
            "service_child_process_startup_latency",
            "cold_service_ipc_call_latency",
            "warm_service_ipc_call_latency",
            "peak_process_rss",
            "oid_index_package_update_latency",
        ],
    })
}

fn warm(mut iterations: u64, mut operation: impl FnMut()) {
    while iterations != 0 {
        operation();
        iterations -= 1;
    }
}

fn measure(name: &'static str, mut iterations: u64, mut operation: impl FnMut()) -> BenchMetric {
    let original_iterations = iterations;
    let started = Instant::now();
    while iterations != 0 {
        operation();
        iterations -= 1;
    }
    let total_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    BenchMetric {
        name,
        iterations: original_iterations,
        total_ns,
        ns_per_iteration: total_ns as f64 / original_iterations as f64,
    }
}

fn optimizer_fixture() -> FunctionDef {
    FunctionDef {
        name: "benchmark".into(),
        params: Vec::new(),
        body: vec![
            Statement::Const {
                name: "enabled".into(),
                value: Expr::Bool(true),
            },
            Statement::Const {
                name: "threshold".into(),
                value: Expr::Number(4.0),
            },
            Statement::If {
                condition: Expr::Binary {
                    left: Box::new(Expr::Ident("threshold".into())),
                    op: BinaryOp::Greater,
                    right: Box::new(Expr::Number(2.0)),
                },
                then_body: vec![Statement::Return(Expr::Ident("enabled".into()))],
                else_body: vec![Statement::Return(Expr::Bool(false))],
            },
            Statement::Expr(Expr::String("unreachable".into())),
        ],
    }
}

fn discovery_fixture() -> (ModuleFile, ServiceProgram) {
    let module = ModuleFile {
        imports: Vec::new(),
        functions: vec![
            FunctionDef {
                name: "live".into(),
                params: Vec::new(),
                body: vec![Statement::Return(Expr::Bool(true))],
            },
            FunctionDef {
                name: "dead".into(),
                params: Vec::new(),
                body: vec![Statement::Return(Expr::Bool(false))],
            },
        ],
        exports: vec!["live".into(), "dead".into()],
    };
    let service = ServiceProgram {
        imports: vec![
            ImportTarget::CustomFunction {
                path: "module/bench.module".into(),
                function: "live".into(),
            },
            ImportTarget::CustomFunction {
                path: "module/bench.module".into(),
                function: "dead".into(),
            },
        ],
        functions: vec![FunctionDef {
            name: "run".into(),
            params: Vec::new(),
            body: vec![Statement::If {
                condition: Expr::Bool(false),
                then_body: vec![Statement::Return(Expr::Call(
                    Box::new(Expr::Ident("dead".into())),
                    Vec::new(),
                ))],
                else_body: vec![Statement::Return(Expr::Call(
                    Box::new(Expr::Ident("live".into())),
                    Vec::new(),
                ))],
            }],
        }],
        exports: vec!["run".into()],
        class_name: None,
        lifecycle: Vec::new(),
        classes: Vec::new(),
    };
    (module, service)
}

fn assembly_fixture(
    target_fingerprint: &str,
) -> (ServiceAssemblyPlan, BTreeMap<u16, VerifiedAssemblyOidRecord>) {
    const OID: u16 = 40_000;
    let record_hash = "33".repeat(32);
    let plan = ServiceAssemblyPlan {
        format: SERVICE_PLAN_FORMAT,
        service_identity: "service:bench".into(),
        service_source_sha256: "11".repeat(32),
        index_identity_sha256: "22".repeat(32),
        target_fingerprint: target_fingerprint.to_string(),
        entry_oids: vec![OID],
        required_oids: vec![RequiredOid {
            oid: OID,
            record_hash: record_hash.clone(),
            kind: AssemblyRecordKind::ServiceExport,
        }],
        placement_order: vec![OID],
        call_graph: BTreeMap::from([(OID, BTreeSet::new())]),
        service_data: vec![0x5a; 128],
        data_alignment: 8,
        dependency_hashes: BTreeMap::new(),
        compile_options: BTreeMap::from([("cpu".into(), "baseline".into())]),
    };
    let record = VerifiedAssemblyOidRecord {
        oid: OID,
        record_hash,
        kind: AssemblyRecordKind::ServiceExport,
        target_fingerprint: target_fingerprint.to_string(),
        alignment: 16,
        entry_offset: 0,
        frame_terminator: Some(DONE_OID),
        diagnostics: Vec::new(),
        machine_code: vec![0x90; 64],
        relocations: Vec::new(),
    };
    (plan, BTreeMap::from([(OID, record)]))
}

fn current_rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn cpu_model() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        for key in ["model name", "Hardware", "Processor"] {
            if let Some(value) = info.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == key).then(|| value.trim().to_string())
            }) {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("PROCESSOR_IDENTIFIER").ok()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microbench_report_has_stable_component_metrics() {
        let config = BenchConfig {
            profile: "test".into(),
            iterations: 2,
            warmup_iterations: 1,
            pretty: false,
            output: None,
        };
        let report = run(&config).unwrap();
        assert_eq!(report.schema, BENCH_SCHEMA);
        assert_eq!(report.profile, "test");
        assert_eq!(
            report
                .metrics
                .iter()
                .map(|metric| metric.name)
                .collect::<Vec<_>>(),
            vec![
                "rel_optimizer",
                "linked_rel_reachability",
                "service_bin_assembly",
                "service_bin_cache_decode",
            ]
        );
        assert_eq!(report.artifacts.optimized_statement_count, 1);
        assert_eq!(report.artifacts.reachable_symbol_count, 2);
        assert!(report.artifacts.service_bin_payload_bytes >= 192);
        assert!(report.artifacts.encoded_service_bin_bytes > report.artifacts.service_bin_payload_bytes);
    }

    #[test]
    fn zero_iterations_are_rejected() {
        let config = BenchConfig {
            profile: "test".into(),
            iterations: 0,
            warmup_iterations: 0,
            pretty: false,
            output: None,
        };
        assert!(config.validate().is_err());
    }
}
