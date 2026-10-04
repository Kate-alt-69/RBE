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
