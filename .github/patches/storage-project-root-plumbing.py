from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# backend.exe owns ProjectRoot. Capture it exactly once at boot and then carry
# that same value across initial Container launch and all supervisor restarts.
path = Path("engine/crates/backend/src/main.rs")
replace_once(
    path,
    '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {
    let er_control_key = host_ready.issue_er_control_key();
    boot_trace("start");
    boot_trace(format!(
        "exe={}",
''',
    '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {
    let er_control_key = host_ready.issue_er_control_key();
    boot_trace("start");
    let project_root = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("could not capture RBE project root at backend boot: {error}"))?
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("could not canonicalize RBE project root at backend boot: {error}"))?;
    boot_trace(format!(
        "exe={}",
''',
    "backend ProjectRoot capture",
)
replace_once(
    path,
    '''    boot_trace(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    ));
''',
    '''    boot_trace(format!("project_root={}", project_root.display()));
''',
    "backend ProjectRoot trace",
)
replace_once(
    path,
    '''    let initial_container = container_process::ContainerProcess::spawn(
        &container_path,
        &config.containers,
        &host_capability_endpoint,
    )
''',
    '''    let initial_container = container_process::ContainerProcess::spawn(
        &container_path,
        &config.containers,
        &host_capability_endpoint,
        &project_root,
    )
''',
    "initial Container ProjectRoot",
)
replace_once(
    path,
    '''        ContainerSupervisorContext {
            host_capability: host_capability_endpoint.clone(),
            process: container_process.clone(),
''',
    '''        ContainerSupervisorContext {
            host_capability: host_capability_endpoint.clone(),
            project_root: project_root.clone(),
            process: container_process.clone(),
''',
    "supervisor ProjectRoot context",
)
replace_once(
    path,
    '''struct ContainerSupervisorContext {
    host_capability: host_capability::HostCapabilityEndpoint,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
''',
    '''struct ContainerSupervisorContext {
    host_capability: host_capability::HostCapabilityEndpoint,
    project_root: PathBuf,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
''',
    "supervisor ProjectRoot field",
)
replace_once(
    path,
    '''    let ContainerSupervisorContext {
        host_capability,
        process,
''',
    '''    let ContainerSupervisorContext {
        host_capability,
        project_root,
        process,
''',
    "supervisor ProjectRoot destructure",
)
replace_once(
    path,
    '''                        match container_process::ContainerProcess::spawn(
                            &binary,
                            &settings,
                            &host_capability,
                        )
''',
    '''                        match container_process::ContainerProcess::spawn(
                            &binary,
                            &settings,
                            &host_capability,
                            &project_root,
                        )
''',
    "crash replacement ProjectRoot",
)
replace_once(
    path,
    '''                    match container_process::ContainerProcess::spawn(
                        &binary,
                        &settings,
                        &host_capability,
                    )
''',
    '''                    match container_process::ContainerProcess::spawn(
                        &binary,
                        &settings,
                        &host_capability,
                        &project_root,
                    )
''',
    "rolling replacement ProjectRoot",
)


# backend -> Container bootstrap.
path = Path("engine/crates/backend/src/container_process.rs")
replace_once(
    path,
    '''    pub async fn spawn(
        binary: &Path,
        settings: &config::ContainersConfig,
        host_capability: &crate::host_capability::HostCapabilityEndpoint,
    ) -> anyhow::Result<Self> {
''',
    '''    pub async fn spawn(
        binary: &Path,
        settings: &config::ContainersConfig,
        host_capability: &crate::host_capability::HostCapabilityEndpoint,
        project_root: &Path,
    ) -> anyhow::Result<Self> {
        if !project_root.is_absolute() || !project_root.is_dir() {
            anyhow::bail!(
                "frozen RBE project root must be an existing absolute directory: {}",
                project_root.display()
            );
        }
''',
    "ContainerProcess ProjectRoot signature",
)
replace_once(
    path,
    '''                .arg("--parent-liveness-stdin")
                .arg("--general-environments")
                .arg(settings.environments.to_string());
''',
    '''                .arg("--parent-liveness-stdin")
                .arg("--application-root")
                .arg(project_root)
                .arg("--general-environments")
                .arg(settings.environments.to_string())
                .current_dir(project_root);
''',
    "ContainerProcess ProjectRoot command",
)


# Container controller canonicalizes the explicit application root rather than
# deriving it from its own executable/runtime directory.
path = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    path,
    '''    let debug = args.iter().any(|arg| arg == "--debug");
    let listen = value_after(&args, "--listen");
''',
    '''    let debug = args.iter().any(|arg| arg == "--debug");
    let project_root = value_after(&args, "--application-root")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let project_root = project_root
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize Container application root: {error}"))?;
    if !project_root.is_dir() {
        anyhow::bail!(
            "Container application root is not a directory: {}",
            project_root.display()
        );
    }
    let listen = value_after(&args, "--listen");
''',
    "Container application root",
)
replace_once(
    path,
    '''        Arc::clone(&capability_broker),
        capability_dispatcher,
    )?;
''',
    '''        Arc::clone(&capability_broker),
        capability_dispatcher,
        project_root,
    )?;
''',
    "Environment supervisor ProjectRoot argument",
)


# Container -> exact Environment process bootstrap. The Environment owns the
# Storage call, but the authoritative root is the immutable value received from
# backend.exe.
path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
replace_once(
    path,
    '''use std::ops::{Deref, DerefMut};
use std::process::{Child, ChildStdin, Command, Stdio};
''',
    '''use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
''',
    "Environment PathBuf import",
)
replace_once(
    path,
    '''use container_runtime_core::{
    dispatch_storage_capability, Canceller, CapabilityBroker, CapabilityCall, EnvironmentId,
    EnvironmentProfile, EnvironmentStorageManager, ExecutionProvenance, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
''',
    '''use container_runtime_core::{
    dispatch_storage_capability_with_project_root, Canceller, CapabilityBroker, CapabilityCall,
    EnvironmentId, EnvironmentProfile, EnvironmentStorageManager, ExecutionProvenance,
    ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
''',
    "Environment project Storage dispatcher import",
)
replace_once(
    path,
    'const CHILD_PROTOCOL_VERSION: u16 = 4;\n',
    'const CHILD_PROTOCOL_VERSION: u16 = 5;\n',
    "Environment child protocol v5",
)
replace_once(
    path,
    '''    storage_limit_bytes: u64,
    debug: bool,
    session: String,
''',
    '''    storage_limit_bytes: u64,
    project_root: String,
    debug: bool,
    session: String,
''',
    "Environment bootstrap ProjectRoot",
)
replace_once(
    path,
    '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
''',
    '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    project_root: Arc<PathBuf>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
''',
    "Environment child ProjectRoot state",
)
replace_once(
    path,
    '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    artifact_crash_circuit: ArtifactCrashCircuit,
''',
    '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    project_root: Arc<PathBuf>,
    artifact_crash_circuit: ArtifactCrashCircuit,
''',
    "Environment supervisor ProjectRoot field",
)
replace_once(
    path,
    '''        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
    ) -> Result<Arc<Self>> {
        let supervisor = Arc::new(Self {
''',
    '''        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
        project_root: PathBuf,
    ) -> Result<Arc<Self>> {
        let project_root = project_root
            .canonicalize()
            .context("canonicalize Environment supervisor ProjectRoot")?;
        if !project_root.is_dir() {
            bail!(
                "Environment supervisor ProjectRoot is not a directory: {}",
                project_root.display()
            );
        }
        let supervisor = Arc::new(Self {
''',
    "Environment supervisor ProjectRoot start",
)
replace_once(
    path,
    '''            capability_broker,
            capability_dispatcher,
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
''',
    '''            capability_broker,
            capability_dispatcher,
            project_root: Arc::new(project_root),
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
''',
    "Environment supervisor ProjectRoot init",
)
replace_once(
    path,
    '''            generation,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            debug: child_debug,
            session: session.clone(),
''',
    '''            generation,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: self
                .project_root
                .to_str()
                .ok_or_else(|| anyhow!("RBE ProjectRoot is not valid UTF-8"))?
                .to_string(),
            debug: child_debug,
            session: session.clone(),
''',
    "Environment child bootstrap ProjectRoot",
)
replace_once(
    path,
    '''    let environment = parse_environment(&bootstrap.environment)
        .ok_or_else(|| anyhow!("invalid Environment {}", bootstrap.environment))?;
    let storage_root = runtime_paths::binary_dir()
''',
    '''    let environment = parse_environment(&bootstrap.environment)
        .ok_or_else(|| anyhow!("invalid Environment {}", bootstrap.environment))?;
    let project_root = PathBuf::from(&bootstrap.project_root)
        .canonicalize()
        .context("canonicalize Environment ProjectRoot")?;
    if !project_root.is_dir() {
        bail!(
            "Environment ProjectRoot is not a directory: {}",
            project_root.display()
        );
    }
    let storage_root = runtime_paths::binary_dir()
''',
    "Environment child canonical ProjectRoot",
)
replace_once(
    path,
    '''    let state = Arc::new(EnvironmentChildState {
        storage,
        active_executions: Mutex::new(HashMap::new()),
''',
    '''    let state = Arc::new(EnvironmentChildState {
        storage,
        project_root: Arc::new(project_root),
        active_executions: Mutex::new(HashMap::new()),
''',
    "Environment child ProjectRoot state init",
)
replace_once(
    path,
    '''                        let result = match dispatch_storage_capability(
                            &state.storage,
                            &call.target,
''',
    '''                        let result = match dispatch_storage_capability_with_project_root(
                            &state.storage,
                            &state.project_root,
                            &call.target,
''',
    "Environment project Storage dispatch",
)
replace_once(
    path,
    '''    if bootstrap.storage_limit_bytes == 0 {
        bail!("Environment storage limit must be non-zero");
    }
    if bootstrap.session.len() != 64
''',
    '''    if bootstrap.storage_limit_bytes == 0 {
        bail!("Environment storage limit must be non-zero");
    }
    if bootstrap.project_root.is_empty() {
        bail!("Environment ProjectRoot must be non-empty");
    }
    if bootstrap.session.len() != 64
''',
    "Environment ProjectRoot bootstrap validation",
)
replace_once(
    path,
    '''            Arc::new(EnvironmentChildState {
                storage,
                active_executions: Mutex::new(HashMap::new()),
''',
    '''            Arc::new(EnvironmentChildState {
                storage,
                project_root: Arc::new(root.clone()),
                active_executions: Mutex::new(HashMap::new()),
''',
    "Environment test ProjectRoot state",
)
replace_once(
    path,
    '''            generation,
            storage_limit_bytes: 4096,
            debug: false,
            session: "ab".repeat(32),
''',
    '''            generation,
            storage_limit_bytes: 4096,
            project_root: root.to_string_lossy().into_owned(),
            debug: false,
            session: "ab".repeat(32),
''',
    "Environment transport test ProjectRoot",
)
replace_once(
    path,
    '''            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            debug: false,
            session: "ab".repeat(32),
''',
    '''            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            debug: false,
            session: "ab".repeat(32),
''',
    "Environment session test ProjectRoot",
)

print("project-root Storage plumbing patch applied")
