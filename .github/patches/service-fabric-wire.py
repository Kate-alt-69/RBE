from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:160]!r}")
    write(path, text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# service-runtime: explicit, non-ambient Service Fabric endpoint.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/service-runtime/src/lib.rs",
    "pub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};\npub use mother::{new_service_mother_token, run_service_mother, ServiceMotherReady};\n",
    '''pub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};
pub use mother::{
    new_service_mother_token, run_service_mother, ServiceMotherReady, ServiceMotherServer,
};

/// Authenticated loopback endpoint handed only to service children by their
/// Service Mother. The authentication token is transferred over the inherited
/// stdin bootstrap pipe, never command-line arguments or environment variables.
#[derive(Clone)]
pub struct ServiceFabricEndpoint {
    address: SocketAddr,
    auth: Arc<str>,
}

impl ServiceFabricEndpoint {
    pub fn new(address: SocketAddr, auth: String) -> anyhow::Result<Self> {
        if !address.ip().is_loopback() {
            anyhow::bail!("Service Fabric endpoint must be loopback");
        }
        validate_parent_bootstrap_secret(&auth)?;
        Ok(Self {
            address,
            auth: Arc::<str>::from(auth),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn auth(&self) -> &str {
        &self.auth
    }
}
''',
)

# ---------------------------------------------------------------------------
# ServiceManager: preserve Fabric endpoint across initial start, on-demand
# activation and supervised restarts. Two-phase preparation lets Mother accept
# calls while resident Service.start() hooks are still booting.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace(
    "    ServiceRequest, ServiceResponse, SERVICE_IPC_REQUEST_MAX_BYTES, SERVICE_IPC_RESPONSE_MAX_BYTES,\n    SERVICE_IPC_TIMEOUT,\n};",
    "    ServiceFabricEndpoint, ServiceRequest, ServiceResponse, SERVICE_IPC_REQUEST_MAX_BYTES,\n    SERVICE_IPC_RESPONSE_MAX_BYTES, SERVICE_IPC_TIMEOUT,\n};",
    1,
)
text = text.replace(
    "    mother: Option<ServiceMotherClient>,\n}",
    "    mother: Option<ServiceMotherClient>,\n    fabric: Option<ServiceFabricEndpoint>,\n}",
    1,
)
old = '''    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::default();
        for file in catalog.services() {
            let managed = match file.mode {
                ServiceMode::OnDemand => Managed::dormant(file.clone()),
                ServiceMode::Resident | ServiceMode::Hybrid => match spawn_process(file).await {
                    Ok(process) => Managed::running(file.clone(), process),
                    Err(error) => {
                        manager.shutdown_all().await;
                        return Err(error);
                    }
                },
            };
            manager
                .services
                .write()
                .await
                .insert(file.name.clone(), Arc::new(Mutex::new(managed)));
        }
        if !catalog.services().is_empty() {
            manager.start_monitor(
                Duration::from_millis(catalog.monitor_interval_ms.max(50)),
                Duration::from_millis(catalog.max_restart_backoff_ms.max(RESTART_BASE_DELAY_MS)),
            );
        }
        Ok(manager)
    }
'''
new = '''    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None).await;
        manager.start_prepared(catalog).await?;
        Ok(manager)
    }

    pub async fn prepare_all_with_fabric(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
    ) -> Self {
        Self::prepare_all(catalog, Some(fabric)).await
    }

    async fn prepare_all(
        catalog: &ServiceCatalog,
        fabric: Option<ServiceFabricEndpoint>,
    ) -> Self {
        let manager = Self {
            fabric,
            ..Self::default()
        };
        let mut services = manager.services.write().await;
        for file in catalog.services() {
            services.insert(
                file.name.clone(),
                Arc::new(Mutex::new(Managed::dormant(file.clone()))),
            );
        }
        drop(services);
        manager
    }

    /// Start resident/hybrid children after every service identity is already
    /// addressable by Mother. Direct service dependencies are started first so
    /// lifecycle hooks can synchronously call an already-running dependency.
    pub async fn start_prepared(&self, catalog: &ServiceCatalog) -> anyhow::Result<()> {
        for file in service_startup_order(catalog)? {
            if file.mode == ServiceMode::OnDemand {
                continue;
            }
            let process = match spawn_process(&file, self.fabric.as_ref()).await {
                Ok(process) => process,
                Err(error) => {
                    self.shutdown_all().await;
                    return Err(error);
                }
            };
            let handle = self
                .services
                .read()
                .await
                .get(&file.name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("prepared service {:?} disappeared", file.name))?;
            let mut managed = handle.lock().await;
            managed.process = Some(process);
            managed.restart_attempts = 0;
            managed.exit_observed = false;
            managed.restarting = false;
            managed.last_activity = Instant::now();
        }
        if !catalog.services().is_empty() {
            self.start_monitor(
                Duration::from_millis(catalog.monitor_interval_ms.max(50)),
                Duration::from_millis(catalog.max_restart_backoff_ms.max(RESTART_BASE_DELAY_MS)),
            );
        }
        Ok(())
    }
'''
if old not in text:
    raise SystemExit("missing ServiceManager::spawn_all anchor")
text = text.replace(old, new, 1)
# All restart/activation process creation must retain the Fabric capability.
text = text.replace("spawn_process(&file).await", "spawn_process(&file, self.fabric.as_ref()).await")
# Function definition itself.
text = text.replace(
    "async fn spawn_process(file: &ServiceFile) -> anyhow::Result<ServiceProcess> {",
    "async fn spawn_process(\n    file: &ServiceFile,\n    fabric: Option<&ServiceFabricEndpoint>,\n) -> anyhow::Result<ServiceProcess> {",
    1,
)
old_cmd = '''    let token = random_token();
    let mut child = match Command::new(&alias)
        .args(["--service-host", "--service-file"])
        .arg(&file.path)
        .current_dir(parent)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
'''
new_cmd = '''    let token = random_token();
    let mut command = Command::new(&alias);
    command
        .args(["--service-host", "--service-file"])
        .arg(&file.path);
    if let Some(fabric) = fabric {
        command
            .arg("--service-mother-address")
            .arg(fabric.address().to_string());
    }
    let mut child = match command
        .current_dir(parent)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
'''
if old_cmd not in text:
    raise SystemExit("missing service child command anchor")
text = text.replace(old_cmd, new_cmd, 1)
old_secret = '''    if let Err(error) = super::write_parent_bootstrap_secret(&mut liveness, &token).await {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send service {:?} parent bootstrap secret: {error}",
            file.name
        ));
    }
'''
new_secret = old_secret + '''    if let Some(fabric) = fabric {
        if let Err(error) =
            super::write_parent_bootstrap_secret(&mut liveness, fabric.auth()).await
        {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(anyhow::anyhow!(
                "send service {:?} Service Fabric bootstrap secret: {error}",
                file.name
            ));
        }
    }
'''
if old_secret not in text:
    raise SystemExit("missing service bootstrap secret anchor")
text = text.replace(old_secret, new_secret, 1)
# Add deterministic dependency-aware resident startup ordering before process IO.
anchor = "async fn stop_process(service_name: &str, process: &mut ServiceProcess) {"
if anchor not in text:
    raise SystemExit("missing stop_process anchor")
startup_helpers = r'''fn direct_service_dependency(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("service:")?;
    let name = rest
        .split(|character: char| character == '.' || character.is_whitespace())
        .next()
        .unwrap_or_default()
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn service_startup_order(catalog: &ServiceCatalog) -> anyhow::Result<Vec<ServiceFile>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Visiting,
        Done,
    }

    fn visit(
        name: &str,
        files: &HashMap<String, ServiceFile>,
        state: &mut HashMap<String, Visit>,
        stack: &mut Vec<String>,
        out: &mut Vec<ServiceFile>,
    ) -> anyhow::Result<()> {
        match state.get(name) {
            Some(Visit::Done) => return Ok(()),
            Some(Visit::Visiting) => {
                let start = stack.iter().position(|item| item == name).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(name.to_string());
                anyhow::bail!(
                    "synchronous Service Fabric dependency cycle would deadlock: {}",
                    cycle.join(" -> ")
                );
            }
            None => {}
        }
        let file = files
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown service dependency {name:?}"))?;
        state.insert(name.to_string(), Visit::Visiting);
        stack.push(name.to_string());
        for dependency in file.imports.iter().filter_map(|raw| direct_service_dependency(raw)) {
            if !files.contains_key(&dependency) {
                anyhow::bail!("service {:?} imports unknown service {dependency:?}", file.name);
            }
            visit(&dependency, files, state, stack, out)?;
        }
        stack.pop();
        state.insert(name.to_string(), Visit::Done);
        out.push(file.clone());
        Ok(())
    }

    let files = catalog
        .services()
        .iter()
        .cloned()
        .map(|file| (file.name.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut names = files.keys().cloned().collect::<Vec<_>>();
    names.sort();
    let mut state = HashMap::new();
    let mut stack = Vec::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        visit(&name, &files, &mut state, &mut stack, &mut out)?;
    }
    Ok(out)
}

'''
text = text.replace(anchor, startup_helpers + anchor, 1)
write(path, text)

# ---------------------------------------------------------------------------
# Service Mother: bind first, serve independently, advertise only after child
# startup succeeds. run_service_mother remains a compatibility convenience.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/mother.rs"
text = read(path)
text = text.replace(
    "use crate::{ServiceCallError, ServiceManager, ServiceSnapshot};",
    "use crate::{ServiceCallError, ServiceFabricEndpoint, ServiceManager, ServiceSnapshot};",
    1,
)
start = text.index("pub async fn run_service_mother(")
end = text.index("\nasync fn handle_connection", start)
new_server = r'''pub struct ServiceMotherServer {
    listener: TcpListener,
    ready: ServiceMotherReady,
    token: Arc<str>,
}

impl ServiceMotherServer {
    pub async fn bind(token: String) -> anyhow::Result<Self> {
        validate_mother_endpoint(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            &token,
        )?;
        let listener =
            TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
        let ready = ServiceMotherReady {
            pid: std::process::id(),
            address: listener.local_addr()?,
        };
        Ok(Self {
            listener,
            ready,
            token: Arc::<str>::from(token),
        })
    }

    pub fn ready(&self) -> &ServiceMotherReady {
        &self.ready
    }

    pub fn fabric_endpoint(&self) -> ServiceFabricEndpoint {
        ServiceFabricEndpoint::new(self.ready.address, self.token.to_string())
            .expect("validated Service Mother endpoint must be a valid Fabric endpoint")
    }

    pub async fn serve(self, manager: ServiceManager) -> anyhow::Result<()> {
        let Self {
            listener,
            ready: _,
            token,
        } = self;
        let connections = Arc::new(tokio::sync::Semaphore::new(MAX_MOTHER_CONNECTIONS));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let mut accept_failures = 0u32;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => {
                            accept_failures = 0;
                            accepted
                        }
                        Err(error) => {
                            accept_failures = accept_failures.saturating_add(1);
                            if accept_failures >= MOTHER_ACCEPT_FAILURE_LIMIT {
                                return Err(anyhow::anyhow!(
                                    "Service Mother listener failed {accept_failures} consecutive accepts: {error}"
                                ));
                            }
                            tracing::warn!(
                                error = %error,
                                accept_failures,
                                retry_ms = MOTHER_ACCEPT_RETRY_DELAY.as_millis() as u64,
                                "Service Mother listener accept failed; retrying without tearing down services"
                            );
                            tokio::time::sleep(MOTHER_ACCEPT_RETRY_DELAY).await;
                            continue;
                        }
                    };
                    if !peer.ip().is_loopback() {
                        tracing::warn!(%peer, "Service Mother rejected non-loopback peer");
                        continue;
                    }
                    let permit = match connections.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(%peer, limit = MAX_MOTHER_CONNECTIONS, "Service Mother connection limit reached");
                            drop(stream);
                            continue;
                        }
                    };
                    let manager = manager.clone();
                    let token = token.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(error) = handle_connection(stream, manager, token, shutdown_tx).await {
                            tracing::warn!(error = %error, "Service Mother request failed");
                        }
                    });
                }
            }
        }
        manager.shutdown_all().await;
        Ok(())
    }
}

pub async fn run_service_mother(manager: ServiceManager, token: String) -> anyhow::Result<()> {
    let server = ServiceMotherServer::bind(token).await?;
    println!("{}", serde_json::to_string(server.ready())?);
    std::io::stdout().flush()?;
    server.serve(manager).await
}
'''
text = text[:start] + new_server + text[end:]
write(path, text)

# ---------------------------------------------------------------------------
# route-engine VM: Service REL gets both service caller and process-local host
# capabilities. Grammar restriction is removed; capability control stays in
# RELC/runtime validation.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''        if imports.iter().any(import_contains_service) {
            return Err(
                self.error_here("service-to-service imports are not supported in .service files")
            );
        }

''',
    "",
)

replace_once(
    "engine/crates/route-engine/src/module_eval.rs",
    '''    pub(crate) fn with_host_capabilities_and_classes(
        program: &'a ModuleProgram,
        host_capabilities: Arc<dyn HostCapabilityCaller>,
        classes: Arc<HashMap<String, ServiceClassDef>>,
    ) -> Self {
        Self {
            program,
            services: None,
            host_capabilities: Some(host_capabilities),
            classes,
        }
    }
''',
    '''    pub(crate) fn with_host_capabilities_and_classes(
        program: &'a ModuleProgram,
        host_capabilities: Arc<dyn HostCapabilityCaller>,
        classes: Arc<HashMap<String, ServiceClassDef>>,
    ) -> Self {
        Self {
            program,
            services: None,
            host_capabilities: Some(host_capabilities),
            classes,
        }
    }

    pub(crate) fn with_services_host_capabilities_and_classes(
        program: &'a ModuleProgram,
        services: ServiceManager,
        host_capabilities: Arc<dyn HostCapabilityCaller>,
        classes: Arc<HashMap<String, ServiceClassDef>>,
    ) -> Self {
        Self {
            program,
            services: Some(Arc::new(services)),
            host_capabilities: Some(host_capabilities),
            classes,
        }
    }
''',
)

path = "engine/crates/route-engine/src/service_eval.rs"
text = read(path)
text = text.replace(
    "use service_runtime::{\n    ServiceExecutionError, ServiceExecutionFuture, ServiceExecutor, ServiceLifecycle,\n    ServiceLifecycleFuture, ServiceMemory,\n};",
    "use service_runtime::{\n    ServiceExecutionError, ServiceExecutionFuture, ServiceExecutor, ServiceLifecycle,\n    ServiceLifecycleFuture, ServiceManager, ServiceMemory,\n};",
    1,
)
text = text.replace(
    "    host_capabilities: Arc<ServiceHostCapabilities>,\n}",
    "    host_capabilities: Arc<ServiceHostCapabilities>,\n    services: Option<ServiceManager>,\n}",
    1,
)
old_ctor = '''    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        let ServiceProgram {
            imports,
            functions,
            exports,
            lifecycle,
            classes,
            ..
        } = program;
'''
new_ctor = '''    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        Self::build(program, modules, memory, None)
    }

    pub fn with_services(
        program: ServiceProgram,
        modules: ModuleProgram,
        memory: ServiceMemory,
        services: ServiceManager,
    ) -> Self {
        Self::build(program, modules, memory, Some(services))
    }

    fn build(
        program: ServiceProgram,
        modules: ModuleProgram,
        memory: ServiceMemory,
        services: Option<ServiceManager>,
    ) -> Self {
        let ServiceProgram {
            imports,
            functions,
            exports,
            lifecycle,
            classes,
            ..
        } = program;
'''
if old_ctor not in text:
    raise SystemExit("missing ServiceProgramExecutor constructor anchor")
text = text.replace(old_ctor, new_ctor, 1)
text = text.replace(
    '''            classes,
            host_capabilities,
        }
    }

    fn lifecycle_method''',
    '''            classes,
            host_capabilities,
            services,
        }
    }

    fn lifecycle_method''',
    1,
)
old_exec = '''    fn module_executor(&self) -> ModuleExecutor<'_> {
        ModuleExecutor::with_host_capabilities_and_classes(
            &self.modules,
            self.host_capabilities.clone(),
            self.classes.clone(),
        )
    }
'''
new_exec = '''    fn module_executor(&self) -> ModuleExecutor<'_> {
        match &self.services {
            Some(services) => ModuleExecutor::with_services_host_capabilities_and_classes(
                &self.modules,
                services.clone(),
                self.host_capabilities.clone(),
                self.classes.clone(),
            ),
            None => ModuleExecutor::with_host_capabilities_and_classes(
                &self.modules,
                self.host_capabilities.clone(),
                self.classes.clone(),
            ),
        }
    }
'''
if old_exec not in text:
    raise SystemExit("missing service module_executor anchor")
text = text.replace(old_exec, new_exec, 1)
write(path, text)

# ---------------------------------------------------------------------------
# backend service host: receive Mother address openly but Mother auth only as a
# second bounded stdin bootstrap frame. Mother child boot serves before starts,
# then advertises readiness only after configured services are healthy.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
text = text.replace("use std::path::{Path, PathBuf};", "use std::net::SocketAddr;\nuse std::path::{Path, PathBuf};", 1)
anchor = '''    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("service host")? {
        Some(token) => token,
        None => value("--service-token")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("backend --service-host requires parent authentication")
            })?,
    };
'''
addition = anchor + '''    let service_manager = match value("--service-mother-address") {
        Some(raw_address) => {
            let address = raw_address.parse::<SocketAddr>().map_err(|error| {
                anyhow::anyhow!("service host received invalid Service Mother address: {error}")
            })?;
            if !address.ip().is_loopback() {
                anyhow::bail!("service host Service Mother address must be loopback");
            }
            let auth = service_runtime::read_parent_bootstrap_secret_if_configured(
                "Service Fabric",
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "service host received a Service Mother address without inherited Fabric authentication"
                )
            })?;
            service_runtime::ServiceManager::remote(address, auth)?
        }
        None => service_runtime::ServiceManager::default(),
    };
'''
if anchor not in text:
    raise SystemExit("missing service host token anchor")
text = text.replace(anchor, addition, 1)
text = text.replace(
    "let executor = route_engine::ServiceProgramExecutor::new(program, modules, memory.clone());",
    "let executor = route_engine::ServiceProgramExecutor::with_services(\n        program,\n        modules,\n        memory.clone(),\n        service_manager,\n    );",
    1,
)
write(path, text)

path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
text = text.replace("use std::path::{Path, PathBuf};", "use std::io::Write;\nuse std::path::{Path, PathBuf};", 1)
text = text.replace(
    '''use service_runtime::{
    new_service_mother_token, run_service_mother, ServiceManager, ServiceMotherReady,
};''',
    '''use service_runtime::{
    new_service_mother_token, ServiceManager, ServiceMotherReady, ServiceMotherServer,
};''',
    1,
)
old_boot = '''    let manager = match catalog.as_ref() {
        Some(catalog) => ServiceManager::spawn_all(catalog).await?,
        None => ServiceManager::default(),
    };
    tracing::info!(
        pid = std::process::id(),
        services = catalog
            .as_ref()
            .map(|catalog| catalog.services().len())
            .unwrap_or(0),
        "Service Mother runtime ready"
    );
    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    match parent_liveness.as_mut() {
        Some(parent_liveness) => {
            tokio::select! {
                result = run_service_mother(manager.clone(), token) => result,
                _ = parent_liveness => {
                    tracing::warn!(
                        "Service Mother parent liveness pipe closed; shutting down managed services"
                    );
                    manager.shutdown_all().await;
                    Ok(())
                }
            }
        }
        None => run_service_mother(manager, token).await,
    }
'''
new_boot = '''    let server = ServiceMotherServer::bind(token).await?;
    let ready = server.ready().clone();
    let manager = match catalog.as_ref() {
        Some(catalog) => {
            ServiceManager::prepare_all_with_fabric(catalog, server.fabric_endpoint()).await
        }
        None => ServiceManager::default(),
    };
    // Serve Fabric RPC before running Service.start() so lifecycle hooks can
    // call dependencies. Parent readiness remains withheld until starts pass.
    let server_manager = manager.clone();
    let mut server_task = tokio::spawn(async move { server.serve(server_manager).await });
    if let Some(catalog) = catalog.as_ref() {
        if let Err(error) = manager.start_prepared(catalog).await {
            server_task.abort();
            let _ = (&mut server_task).await;
            return Err(error);
        }
    }
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    tracing::info!(
        pid = std::process::id(),
        address = %ready.address,
        services = catalog
            .as_ref()
            .map(|catalog| catalog.services().len())
            .unwrap_or(0),
        "Service Mother runtime ready"
    );
    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    match parent_liveness.as_mut() {
        Some(parent_liveness) => {
            tokio::select! {
                result = &mut server_task => result??,
                _ = parent_liveness => {
                    tracing::warn!(
                        "Service Mother parent liveness pipe closed; shutting down managed services"
                    );
                    manager.shutdown_all().await;
                    server_task.abort();
                    let _ = (&mut server_task).await;
                }
            }
            Ok(())
        }
        None => server_task.await??,
    }
    Ok(())
'''
if old_boot not in text:
    raise SystemExit("missing Service Mother child boot anchor")
text = text.replace(old_boot, new_boot, 1)
write(path, text)

# Update focused runtime documentation in the same feature work.
path = "docs/service-runtime.md"
text = read(path)
text = text.replace(
    "Service-to-service imports are rejected in `.service` files. Service calls are intended to flow through the module/mother runtime rather than allowing a service mesh with ambient cross-process authority.",
    "Service-to-service imports are supported through the authenticated loopback Service Mother Fabric. Child services receive the Mother address as non-secret process metadata and receive the Mother authentication token only through the inherited stdin bootstrap pipe. Direct synchronous service dependency cycles are rejected because they would deadlock single-request service workers; normal in-process REL recursion remains a separate concept.",
)
write(path, text)
