from pathlib import Path

path = Path("crates/backend/src/service_mother.rs")
source = path.read_text()
old = '''    pub async fn shutdown(mut self, timeout: Duration) {
        self.manager.shutdown_all().await;
        match tokio::time::timeout(timeout, self.child.wait()).await {'''
new = '''    pub async fn shutdown(mut self, timeout: Duration) {
        let started = tokio::time::Instant::now();
        if tokio::time::timeout(timeout, self.manager.shutdown_all())
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "Service Mother shutdown RPC exceeded graceful shutdown budget"
            );
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
            let _ = std::fs::remove_file(&self.alias);
            return;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, self.child.wait()).await {'''
if old not in source:
    raise SystemExit("Service Mother shutdown anchor missing")
source = source.replace(old, new, 1)
source = source.replace(
    "    let manager = ServiceManager::remote(ready.address, token);",
    "    let manager = ServiceManager::remote(ready.address, token)?;",
    1,
)
path.write_text(source)

path = Path("crates/backend/src/main.rs")
source = path.read_text()
if "mod service_mother;" not in source:
    source = source.replace("mod service_boot;", "mod service_boot;\nmod service_mother;", 1)
anchor = '''    // This must branch before normal backend boot. A user .service process is
    // the same binary in a restricted host mode, not a second mother backend.
    if has("--service-host") {'''
replacement = '''    if has("--service-mother") {
        if let Err(error) = service_mother::run_child(&args).await {
            eprintln!("fatal service-mother error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    // This must branch before normal backend boot. A user .service process is
    // the same binary in a restricted host mode, not a second mother backend.
    if has("--service-host") {'''
if anchor not in source:
    raise SystemExit("backend service-host entry anchor missing")
source = source.replace(anchor, replacement, 1)
old = "    let service_manager = service_boot::start(service_catalog.as_ref()).await?;"
new = '''    let service_mother = match service_catalog.as_ref() {
        Some(_) => Some(service_mother::spawn(&settings_path).await?),
        None => None,
    };
    let service_manager = service_mother
        .as_ref()
        .map(|mother| mother.manager())
        .unwrap_or_default();'''
if old not in source:
    raise SystemExit("backend local service start anchor missing")
source = source.replace(old, new, 1)
old = '''    lifecycle.set(BackendState::Stopping);
    service_manager.shutdown_all().await;
    if let Some(worker) = video_worker_task {'''
new = '''    lifecycle.set(BackendState::Stopping);
    let shutdown_budget =
        Duration::from_millis(config.runtime.graceful_shutdown_timeout_ms.max(1));
    if let Some(mother) = service_mother {
        mother.shutdown(shutdown_budget).await;
    } else {
        service_manager.shutdown_all().await;
    }
    if let Some(worker) = video_worker_task {'''
if old not in source:
    raise SystemExit("backend service shutdown anchor missing")
source = source.replace(old, new, 1)
path.write_text(source)
