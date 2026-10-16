from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    if old not in text:
        raise SystemExit(f"{label} anchor missing")
    file.write_text(text.replace(old, new, 1))


# Expose the process benchmark only from backend.exe.
replace_once(
    "engine/crates/backend/src/main.rs",
    "mod service_mother;\nmod service_native_cutover;",
    "mod service_mother;\nmod service_native_bench;\nmod service_native_cutover;",
    "main benchmark module",
)
replace_once(
    "engine/crates/backend/src/main.rs",
    '    if has("--maintenance-notice") {',
    '''    if has("--native-service-process-bench") {
        match service_native_bench::run(&args).await {
            Ok(()) => return ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("native Service process benchmark failed: {error:#}");
                return ExitCode::FAILURE;
            }
        }
    }

    if has("--maintenance-notice") {''',
    "main benchmark dispatch",
)

# Benchmark needs process PID/RSS observation and a direct Mother-only spawn so
# startup timing does not accidentally include the outer production supervisor.
replace_once(
    "engine/crates/backend/src/service_mother.rs",
    '''impl ServiceMotherProcess {
    pub fn manager(&self) -> ServiceManager {''',
    '''impl ServiceMotherProcess {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub fn manager(&self) -> ServiceManager {''',
    "ServiceMotherProcess pid",
)
replace_once(
    "engine/crates/backend/src/service_mother.rs",
    "async fn spawn_process(\n",
    "pub(crate) async fn spawn_process(\n",
    "Service Mother direct spawn",
)

# Current secured layout exposes the approval reader at each binary crate root;
# the shared package catalog must not depend on backend.exe's full package_links
# module being mounted.
replace_once(
    "engine/crates/backend/src/service_package_catalog.rs",
    "crate::package_links::approval::approved_runtime_capabilities(",
    "crate::package_approval::approved_runtime_capabilities(",
    "Service package approval root",
)

bench_path = Path("engine/crates/backend/src/service_native_bench.rs")
bench = bench_path.read_text()

old = '''struct Fixture {
    service_dir: PathBuf,
    settings_path: PathBuf,
}'''
new = '''struct Fixture {
    service_dir: PathBuf,
    settings_path: PathBuf,
    vault_dir: PathBuf,
}'''
if old not in bench:
    raise SystemExit("benchmark Fixture anchor missing")
bench = bench.replace(old, new, 1)

old = '''impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.settings_path);
        let _ = fs::remove_dir_all(&self.service_dir);
    }
}'''
new = '''impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.settings_path);
        let _ = fs::remove_dir_all(&self.service_dir);
        let _ = fs::remove_dir_all(&self.vault_dir);
    }
}'''
if old not in bench:
    raise SystemExit("benchmark Fixture cleanup anchor missing")
bench = bench.replace(old, new, 1)

old = '''    let image = crate::runtime_image_boot::compile(&config, Some(&catalog))?;
    let cutover = crate::service_native_cutover::prepare_native_service_cutover(&image, Some(&catalog))?
        .ok_or_else(|| anyhow::anyhow!("benchmark Service did not enter the native execution subset"))?;'''
new = '''    let image = crate::runtime_image_boot::compile(&config, Some(&catalog))?;

    // Mirror production OID security instead of injecting a permissive test
    // authority: the disposable backend copy spawns a real isolated Vault child,
    // prepares the core cache, and gives native cutover the same BackendOidVault
    // authority used by normal backend boot.
    let vault_instance = Arc::new(
        vault_process::VaultClient::spawn("phase6-native-bench", &fixture.vault_dir)
            .context("spawn isolated Vault for native Service benchmark")?,
    );
    let oid_vault: Arc<dyn route_engine::OidVaultAuthority> = Arc::new(
        crate::oid_vault::BackendOidVault::new(vault_instance, &root)
            .context("create benchmark OID Vault authority")?,
    );
    crate::runtime_image_boot::prepare_core_oid_cache(&root, oid_vault.clone())
        .context("prepare Vault-attested core OID cache for benchmark")?;
    let cutover = crate::service_native_cutover::prepare_native_service_cutover(
        &image,
        Some(&catalog),
        oid_vault,
    )?
    .ok_or_else(|| anyhow::anyhow!("benchmark Service did not enter the native execution subset"))?;'''
if old not in bench:
    raise SystemExit("benchmark secured cutover anchor missing")
bench = bench.replace(old, new, 1)

old = '''        let mother = crate::service_mother::spawn(
            &fixture.settings_path,
            &catalog.fingerprint(),
            runtime_env.clone(),
            None,
            crate::service_integrity::EXPECTED_SERVICE_SHA256,
        )
        .await?;'''
new = '''        let mother = crate::service_mother::spawn_process(
            &fixture.settings_path,
            &catalog.fingerprint(),
            crate::service_integrity::EXPECTED_SERVICE_SHA256,
            runtime_env.as_ref(),
            None,
            None,
        )
        .await?;'''
if old not in bench:
    raise SystemExit("benchmark Mother spawn anchor missing")
bench = bench.replace(old, new, 1)

if "  memoryLimitMb = 64," not in bench:
    raise SystemExit("benchmark Service memory limit anchor missing")
bench = bench.replace("  memoryLimitMb = 64,", "  memoryLimitMb = 256,", 1)
if '            "defaultMemoryLimitMb": 64,' not in bench:
    raise SystemExit("benchmark default memory limit anchor missing")
bench = bench.replace(
    '            "defaultMemoryLimitMb": 64,',
    '            "defaultMemoryLimitMb": 256,',
    1,
)

old = '''    fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(Fixture {
        service_dir,
        settings_path,
    })'''
new = '''    fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)?;
    let vault_dir = root.join(format!(".bench-native-vault-{nonce}"));
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create benchmark Vault directory {}", vault_dir.display()))?;
    Ok(Fixture {
        service_dir,
        settings_path,
        vault_dir,
    })'''
if old not in bench:
    raise SystemExit("benchmark Vault fixture directory anchor missing")
bench = bench.replace(old, new, 1)

old = "let index = ((sorted.len() - 1) * percent + 99) / 100;"
new = "let index = ((sorted.len() - 1) * percent).div_ceil(100);"
if old not in bench:
    raise SystemExit("benchmark percentile anchor missing")
bench = bench.replace(old, new, 1)
bench_path.write_text(bench)

# Child stdout carries readiness/IPC frames. JSON tracing must use stderr just
# like the pretty formatter or logs can be mistaken for protocol frames.
replace_once(
    "engine/crates/logging/src/terminal.rs",
    '''            .json()
            .flatten_event(true)
            .with_filter(suppression_filter);''',
    '''            .json()
            .flatten_event(true)
            // stdout is reserved for machine-readable child-process protocols
            // such as Service Mother/worker readiness frames. JSON logs must
            // stay on stderr just like pretty logs or they can corrupt IPC.
            .with_writer(std::io::stderr)
            .with_filter(suppression_filter);''',
    "JSON logging writer",
)

# Use one memory limiter for evaluator and native workers. RLIMIT_AS is a whole
# address-space limit, so on Linux/Android add memoryLimitMb as the Service budget
# above the already-loaded Rust/shared-library/thread-stack baseline.
service_runtime = Path("engine/crates/service-runtime/src/lib.rs")
text = service_runtime.read_text()
old = "    apply_memory_limit(file.memory_limit_mb)?;"
new = "    apply_service_memory_limit(file.memory_limit_mb)?;"
if old not in text:
    raise SystemExit("service-runtime memory limit call anchor missing")
text = text.replace(old, new, 1)

old = '''#[cfg(unix)]
fn apply_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        return Ok(());
    }
    let bytes = memory_limit_mb.saturating_mul(1024 * 1024) as libc::rlim_t;
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}'''
new = '''#[cfg(unix)]
pub fn apply_service_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        return Ok(());
    }

    // RLIMIT_AS measures the whole virtual address space, including the Rust
    // host image, shared libraries, thread stacks and runtime mappings already
    // present before a Service executes. memoryLimitMb is the Service execution
    // budget, not a tax on that inherited process baseline.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let baseline_bytes = current_process_virtual_memory_bytes()?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let baseline_bytes = 0;

    let bytes = unix_memory_limit_total_bytes(memory_limit_mb, baseline_bytes)?;
    let bytes = libc::rlim_t::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("service memoryLimitMb exceeds addressable memory"))?;
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(unix)]
fn unix_memory_limit_total_bytes(memory_limit_mb: u64, baseline_bytes: u64) -> anyhow::Result<u64> {
    let budget_bytes = memory_limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("service memoryLimitMb is too large"))?;
    baseline_bytes
        .checked_add(budget_bytes)
        .ok_or_else(|| anyhow::anyhow!("service memoryLimitMb plus runtime baseline is too large"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_process_virtual_memory_bytes() -> anyhow::Result<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").map_err(|error| {
        anyhow::anyhow!("read current Service virtual memory from /proc/self/statm: {error}")
    })?;
    let pages = statm
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("/proc/self/statm did not contain a virtual-memory page count"))?
        .parse::<u64>()
        .map_err(|error| anyhow::anyhow!("parse /proc/self/statm virtual-memory page count: {error}"))?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        anyhow::bail!("query Service process page size before installing RLIMIT_AS failed");
    }
    pages
        .checked_mul(page_size as u64)
        .ok_or_else(|| anyhow::anyhow!("current Service virtual-memory size overflowed u64"))
}'''
if old not in text:
    raise SystemExit("service-runtime Unix memory limiter anchor missing")
text = text.replace(old, new, 1)

remaining = text.count("fn apply_memory_limit(memory_limit_mb: u64)")
if remaining != 2:
    raise SystemExit(f"expected two non-Unix memory limiter definitions, observed {remaining}")
text = text.replace(
    "fn apply_memory_limit(memory_limit_mb: u64)",
    "pub fn apply_service_memory_limit(memory_limit_mb: u64)",
)

anchor = '''mod tests {
    #[test]
    fn malformed_ipc_classifier_identifies_http_probes() {'''
replacement = '''mod tests {
    #[cfg(unix)]
    #[test]
    fn service_memory_limit_budget_adds_to_runtime_baseline() {
        let baseline = 384 * 1024 * 1024;
        let total = super::unix_memory_limit_total_bytes(64, baseline).unwrap();
        assert_eq!(total, baseline + 64 * 1024 * 1024);
    }

    #[cfg(unix)]
    #[test]
    fn service_memory_limit_budget_rejects_overflow() {
        assert!(super::unix_memory_limit_total_bytes(u64::MAX, 1).is_err());
    }

    #[test]
    fn malformed_ipc_classifier_identifies_http_probes() {'''
if anchor not in text:
    raise SystemExit("service-runtime test anchor missing")
service_runtime.write_text(text.replace(anchor, replacement, 1))

native_host = Path("engine/crates/backend/src/service_native_host.rs")
text = native_host.read_text()
old = "    apply_native_memory_limit(frame.memory_limit_mb)?;"
new = "    service_runtime::apply_service_memory_limit(frame.memory_limit_mb)?;"
if old not in text:
    raise SystemExit("native Service memory limit call anchor missing")
text = text.replace(old, new, 1)
start_marker = '#[cfg(unix)]\nfn apply_native_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {'
end_marker = '\n#[cfg(test)]\nmod tests {'
start = text.find(start_marker)
end = text.find(end_marker, start)
if start < 0 or end < 0:
    raise SystemExit("native Service duplicate memory limiter block anchor missing")
native_host.write_text(text[:start] + text[end:])

replace_once(
    "docs/service-runtime.md",
    "On Unix, the configured memory limit is enforced with `RLIMIT_AS`. On Windows, each service host creates a private Job Object, applies `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, and assigns itself before advertising readiness. A non-zero `memoryLimitMb` therefore fails service startup if the platform cannot install the requested hard limit instead of silently running unbounded.",
    "On Unix, the configured memory limit is enforced with `RLIMIT_AS`. On Linux/Android the already-loaded Service host image is measured before the limit is installed, and `memoryLimitMb` is added as the Service's execution budget so shared libraries, Rust/Tokio runtime mappings, and inherited thread stacks do not consume that budget before Service code starts. On other Unix targets the existing absolute `RLIMIT_AS` behavior remains in place. On Windows, each service host creates a private Job Object, applies `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, and assigns itself before advertising readiness. A non-zero `memoryLimitMb` therefore fails service startup if the platform cannot install the requested hard limit instead of silently running unbounded.",
    "service runtime memory-limit documentation",
)
