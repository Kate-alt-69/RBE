from pathlib import Path

path = Path("engine/crates/backend/src/main.rs")
text = path.read_text(encoding="utf-8")

old_call = '''    let container_supervisor_task = spawn_container_supervisor(
        container_path.clone(),
        config.containers.clone(),
        host_capability_endpoint.clone(),
        container_process.clone(),
        container_client.clone(),
        maintenance.clone(),
        refresh_interval,
        er_control_key.clone(),
    );'''
new_call = '''    let container_supervisor_task = spawn_container_supervisor(
        container_path.clone(),
        config.containers.clone(),
        ContainerSupervisorContext {
            host_capability: host_capability_endpoint.clone(),
            process: container_process.clone(),
            client: container_client.clone(),
            maintenance: maintenance.clone(),
            refresh_interval,
            er_control_key: er_control_key.clone(),
        },
    );'''
if text.count(old_call) != 1:
    raise SystemExit(f"CAP-005 supervisor call anchor changed: {text.count(old_call)}")
text = text.replace(old_call, new_call, 1)

old_fn = '''fn spawn_container_supervisor(
    binary: PathBuf,
    settings: config::ContainersConfig,
    host_capability: host_capability::HostCapabilityEndpoint,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
    client: ContainerClient,
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
    er_control_key: Option<host_bootstrap::ErControlKey>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {'''
new_fn = '''struct ContainerSupervisorContext {
    host_capability: host_capability::HostCapabilityEndpoint,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
    client: ContainerClient,
    maintenance: Arc<MaintenanceMetrics>,
    refresh_interval: Duration,
    er_control_key: Option<host_bootstrap::ErControlKey>,
}

fn spawn_container_supervisor(
    binary: PathBuf,
    settings: config::ContainersConfig,
    context: ContainerSupervisorContext,
) -> tokio::task::JoinHandle<()> {
    let ContainerSupervisorContext {
        host_capability,
        process,
        client,
        maintenance,
        refresh_interval,
        er_control_key,
    } = context;
    tokio::spawn(async move {'''
if text.count(old_fn) != 1:
    raise SystemExit(f"CAP-005 supervisor function anchor changed: {text.count(old_fn)}")
text = text.replace(old_fn, new_fn, 1)

path.write_text(text, encoding="utf-8")
