from pathlib import Path

main = Path("engine/crates/backend/src/main.rs")
text = main.read_text()
anchor = "mod port_guard;\nmod runtime_image_boot;"
replacement = '''mod port_guard;
#[path = "package_links/approval.rs"]
mod package_approval;
mod runtime_image_boot;'''
if anchor not in text:
    raise SystemExit("main package approval anchor missing")
text = text.replace(anchor, replacement, 1)
anchor = "mod service_mother;\nmod service_native_cutover;"
replacement = "mod service_mother;\nmod service_native_bench;\nmod service_native_cutover;"
if anchor not in text:
    raise SystemExit("main module anchor missing")
text = text.replace(anchor, replacement, 1)
anchor = '    if has("--maintenance-notice") {'
replacement = '''    if has("--native-service-process-bench") {
        match service_native_bench::run(&args).await {
            Ok(()) => return ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("native Service process benchmark failed: {error:#}");
                return ExitCode::FAILURE;
            }
        }
    }

    if has("--maintenance-notice") {'''
if anchor not in text:
    raise SystemExit("main benchmark dispatch anchor missing")
text = text.replace(anchor, replacement, 1)
main.write_text(text)

service_main = Path("engine/crates/backend/src/service_main.rs")
text = service_main.read_text()
anchor = '''#[path = "error_code_book_core.rs"]
mod error_code_book;
mod service_boot;'''
replacement = '''#[path = "error_code_book_core.rs"]
mod error_code_book;
#[path = "package_links/approval.rs"]
mod package_approval;
mod service_boot;'''
if anchor not in text:
    raise SystemExit("service main package approval anchor missing")
text = text.replace(anchor, replacement, 1)
service_main.write_text(text)

package_links = Path("engine/crates/backend/src/package_links.rs")
text = package_links.read_text()
old = '''#[path = "package_links/approval.rs"]
pub(crate) mod approval;'''
new = "pub(crate) use crate::package_approval as approval;"
if old not in text:
    raise SystemExit("package links approval module anchor missing")
text = text.replace(old, new, 1)
package_links.write_text(text)

mother = Path("engine/crates/backend/src/service_mother.rs")
text = mother.read_text()
anchor = '''impl ServiceMotherProcess {
    pub fn manager(&self) -> ServiceManager {'''
replacement = '''impl ServiceMotherProcess {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub fn manager(&self) -> ServiceManager {'''
if anchor not in text:
    raise SystemExit("ServiceMotherProcess impl anchor missing")
text = text.replace(anchor, replacement, 1)
anchor = "async fn spawn_process(\n"
if anchor not in text:
    raise SystemExit("spawn_process anchor missing")
text = text.replace(anchor, "pub(crate) async fn spawn_process(\n", 1)
mother.write_text(text)

bench = Path("engine/crates/backend/src/service_native_bench.rs")
text = bench.read_text()
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
if old not in text:
    raise SystemExit("benchmark Mother spawn anchor missing")
text = text.replace(old, new, 1)
old = "  memoryLimitMb = 64,"
new = "  memoryLimitMb = 256,"
if old not in text:
    raise SystemExit("benchmark Service memory limit anchor missing")
text = text.replace(old, new, 1)
old = '            "defaultMemoryLimitMb": 64,'
new = '            "defaultMemoryLimitMb": 256,'
if old not in text:
    raise SystemExit("benchmark default memory limit anchor missing")
text = text.replace(old, new, 1)
old = "let index = ((sorted.len() - 1) * percent + 99) / 100;"
new = "let index = ((sorted.len() - 1) * percent).div_ceil(100);"
if old not in text:
    raise SystemExit("benchmark percentile anchor missing")
text = text.replace(old, new, 1)
bench.write_text(text)

catalog = Path("engine/crates/backend/src/service_package_catalog.rs")
text = catalog.read_text()
old = "crate::package_links::approval::approved_runtime_capabilities("
if old not in text:
    raise SystemExit("service package approval call anchor missing")
text = text.replace(old, "crate::package_approval::approved_runtime_capabilities(", 1)
catalog.write_text(text)

terminal = Path("engine/crates/logging/src/terminal.rs")
text = terminal.read_text()
old = '''            .json()
            .flatten_event(true)
            .with_filter(suppression_filter);'''
new = '''            .json()
            .flatten_event(true)
            // stdout is reserved for machine-readable child-process protocols
            // such as Service Mother/worker readiness frames. JSON logs must
            // stay on stderr just like pretty logs or they can corrupt IPC.
            .with_writer(std::io::stderr)
            .with_filter(suppression_filter);'''
if old not in text:
    raise SystemExit("JSON logging writer anchor missing")
text = text.replace(old, new, 1)
terminal.write_text(text)

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
    // host image, shared libraries, thread stacks and runtime mappings that are
    // already present before a Service executes. Treat memoryLimitMb as the
    // Service's additional budget instead of charging that inherited baseline
    // against the Service before its first instruction can run.
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
    let statm = std::fs::read_to_string("/proc/self/statm")
        .map_err(|error| anyhow::anyhow!("read current Service virtual memory from /proc/self/statm: {error}"))?;
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
text = text.replace(anchor, replacement, 1)
service_runtime.write_text(text)

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
text = text[:start] + text[end:]
native_host.write_text(text)

docs = Path("docs/service-runtime.md")
text = docs.read_text()
old = "On Unix, the configured memory limit is enforced with `RLIMIT_AS`. On Windows, each service host creates a private Job Object, applies `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, and assigns itself before advertising readiness. A non-zero `memoryLimitMb` therefore fails service startup if the platform cannot install the requested hard limit instead of silently running unbounded."
new = "On Unix, the configured memory limit is enforced with `RLIMIT_AS`. On Linux/Android the already-loaded Service host image is measured before the limit is installed, and `memoryLimitMb` is added as the Service's execution budget so shared libraries, Rust/Tokio runtime mappings, and inherited thread stacks do not consume that budget before Service code starts. On other Unix targets the existing absolute `RLIMIT_AS` behavior remains in place. On Windows, each service host creates a private Job Object, applies `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, and assigns itself before advertising readiness. A non-zero `memoryLimitMb` therefore fails service startup if the platform cannot install the requested hard limit instead of silently running unbounded."
if old not in text:
    raise SystemExit("service runtime memory-limit documentation anchor missing")
text = text.replace(old, new, 1)
docs.write_text(text)
