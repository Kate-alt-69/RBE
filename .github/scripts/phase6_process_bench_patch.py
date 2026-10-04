from pathlib import Path

main = Path("engine/crates/backend/src/main.rs")
text = main.read_text()
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
bench.write_text(text)
