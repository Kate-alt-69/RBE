from pathlib import Path


def one(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# backend: freeze the original launch CWD once and carry it across every
# Container restart instead of recomputing process CWD later.
path = Path("engine/crates/backend/src/main.rs")
text = path.read_text(encoding="utf-8")

old = '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {
    let er_control_key = host_ready.issue_er_control_key();
    boot_trace("start");
    boot_trace(format!(
        "exe={}",
'''
new = '''async fn boot_and_run(host_ready: host_bootstrap::HostBootstrapReady) -> anyhow::Result<()> {
    let er_control_key = host_ready.issue_er_control_key();
    boot_trace("start");
    let project_root = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("could not capture RBE project root at backend boot: {error}"))?
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("could not canonicalize RBE project root at backend boot: {error}"))?;
    boot_trace(format!(
        "exe={}",
'''
if text.count(old) != 1:
    raise SystemExit(f"backend frozen ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    boot_trace(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    ));
'''
new = '''    boot_trace(format!("project_root={}", project_root.display()));
'''
if text.count(old) != 1:
    raise SystemExit(f"backend ProjectRoot trace anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    let initial_container = container_process::ContainerProcess::spawn(
        &container_path,
        &config.containers,
        &host_capability_endpoint,
    )
'''
new = '''    let initial_container = container_process::ContainerProcess::spawn(
        &container_path,
        &config.containers,
        &host_capability_endpoint,
        &project_root,
    )
'''
if text.count(old) != 1:
    raise SystemExit(f"backend initial Container ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''        ContainerSupervisorContext {
            host_capability: host_capability_endpoint.clone(),
            process: container_process.clone(),
'''
new = '''        ContainerSupervisorContext {
            host_capability: host_capability_endpoint.clone(),
            project_root: project_root.clone(),
            process: container_process.clone(),
'''
if text.count(old) != 1:
    raise SystemExit(f"backend supervisor ProjectRoot context anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''struct ContainerSupervisorContext {
    host_capability: host_capability::HostCapabilityEndpoint,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
'''
new = '''struct ContainerSupervisorContext {
    host_capability: host_capability::HostCapabilityEndpoint,
    project_root: PathBuf,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,
'''
if text.count(old) != 1:
    raise SystemExit(f"backend supervisor ProjectRoot field anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    let ContainerSupervisorContext {
        host_capability,
        process,
'''
new = '''    let ContainerSupervisorContext {
        host_capability,
        project_root,
        process,
'''
if text.count(old) != 1:
    raise SystemExit(f"backend supervisor ProjectRoot destructure anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''                        match container_process::ContainerProcess::spawn(
                            &binary,
                            &settings,
                            &host_capability,
                        )
'''
new = '''                        match container_process::ContainerProcess::spawn(
                            &binary,
                            &settings,
                            &host_capability,
                            &project_root,
                        )
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"backend crash replacement ProjectRoot anchor count={count}")
text = text.replace(old, new, 1)

old = '''                    match container_process::ContainerProcess::spawn(
                        &binary,
                        &settings,
                        &host_capability,
                    )
'''
new = '''                    match container_process::ContainerProcess::spawn(
                        &binary,
                        &settings,
                        &host_capability,
                        &project_root,
                    )
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"backend rolling replacement ProjectRoot anchor count={count}")
text = text.replace(old, new, 1)

path.write_text(text, encoding="utf-8")


# backend -> Container: project root is explicit bootstrap state.
path = Path("engine/crates/backend/src/container_process.rs")
text = path.read_text(encoding="utf-8")

old = '''    pub async fn spawn(
        binary: &Path,
        settings: &config::ContainersConfig,
        host_capability: &crate::host_capability::HostCapabilityEndpoint,
    ) -> anyhow::Result<Self> {
'''
new = '''    pub async fn spawn(
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
'''
if text.count(old) != 1:
    raise SystemExit(f"ContainerProcess ProjectRoot signature anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''                .arg("--parent-liveness-stdin")
                .arg("--general-environments")
                .arg(settings.environments.to_string());
'''
new = '''                .arg("--parent-liveness-stdin")
                .arg("--application-root")
                .arg(project_root)
                .arg("--general-environments")
                .arg(settings.environments.to_string())
                .current_dir(project_root);
'''
if text.count(old) != 1:
    raise SystemExit(f"ContainerProcess ProjectRoot command anchor count={text.count(old)}")
text = text.replace(old, new, 1)
path.write_text(text, encoding="utf-8")


# Container controller: canonicalize explicit application root once on startup.
path = Path("container-runtime/crates/container-bin/src/main.rs")
text = path.read_text(encoding="utf-8")

old = '''    let debug = args.iter().any(|arg| arg == "--debug");
    let listen = value_after(&args, "--listen");
'''
new = '''    let debug = args.iter().any(|arg| arg == "--debug");
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
'''
if text.count(old) != 1:
    raise SystemExit(f"Container main ProjectRoot parse anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''        Arc::clone(&capability_broker),
        capability_dispatcher,
    )?;
'''
new = '''        Arc::clone(&capability_broker),
        capability_dispatcher,
        project_root,
    )?;
'''
if text.count(old) != 1:
    raise SystemExit(f"Container supervisor ProjectRoot argument anchor count={text.count(old)}")
text = text.replace(old, new, 1)
path.write_text(text, encoding="utf-8")


# Environment process: carry exact ProjectRoot in inherited bootstrap state.
path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = path.read_text(encoding="utf-8")

old = '''use std::ops::{Deref, DerefMut};
use std::process::{Child, ChildStdin, Command, Stdio};
'''
new = '''use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment ProjectRoot import anchor count={text.count(old)}")
text = text.replace(old, new, 1)

if text.count("const CHILD_PROTOCOL_VERSION: u16 = 4;") != 1:
    raise SystemExit("Environment child protocol v4 anchor missing")
text = text.replace("const CHILD_PROTOCOL_VERSION: u16 = 4;", "const CHILD_PROTOCOL_VERSION: u16 = 5;", 1)

old = '''    storage_limit_bytes: u64,
    debug: bool,
    session: String,
'''
new = '''    storage_limit_bytes: u64,
    project_root: String,
    debug: bool,
    session: String,
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment Bootstrap ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
'''
new = '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    project_root: Arc<PathBuf>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment state ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    artifact_crash_circuit: ArtifactCrashCircuit,
'''
new = '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    project_root: Arc<PathBuf>,
    artifact_crash_circuit: ArtifactCrashCircuit,
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment supervisor ProjectRoot field anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
    ) -> Result<Arc<Self>> {
        let supervisor = Arc::new(Self {
'''
new = '''        controller_token: Option<&str>,
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
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment supervisor ProjectRoot start anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            capability_broker,
            capability_dispatcher,
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
'''
new = '''            capability_broker,
            capability_dispatcher,
            project_root: Arc::new(project_root),
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment supervisor ProjectRoot init anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            generation,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            debug: child_debug,
            session: session.clone(),
'''
new = '''            generation,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: self
                .project_root
                .to_str()
                .ok_or_else(|| anyhow!("RBE ProjectRoot is not valid UTF-8"))?
                .to_string(),
            debug: child_debug,
            session: session.clone(),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment spawn Bootstrap ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    let environment = parse_environment(&bootstrap.environment)
        .ok_or_else(|| anyhow!("invalid Environment {}", bootstrap.environment))?;
    let storage_root = runtime_paths::binary_dir()
'''
new = '''    let environment = parse_environment(&bootstrap.environment)
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
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment child ProjectRoot canonicalization anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    let state = Arc::new(EnvironmentChildState {
        storage,
        active_executions: Mutex::new(HashMap::new()),
'''
new = '''    let state = Arc::new(EnvironmentChildState {
        storage,
        project_root: Arc::new(project_root),
        active_executions: Mutex::new(HashMap::new()),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment child state ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''                        let result = match dispatch_storage_capability(
                            &state.storage,
                            &call.target,
'''
new = '''                        let result = match dispatch_storage_capability(
                            &state.storage,
                            &state.project_root,
                            &call.target,
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment Storage ProjectRoot dispatch anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    if bootstrap.storage_limit_bytes == 0 {
        bail!("Environment storage limit must be non-zero");
    }
    if bootstrap.session.len() != 64
'''
new = '''    if bootstrap.storage_limit_bytes == 0 {
        bail!("Environment storage limit must be non-zero");
    }
    if bootstrap.project_root.is_empty() {
        bail!("Environment ProjectRoot must be non-empty");
    }
    if bootstrap.session.len() != 64
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment validate ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            Arc::new(EnvironmentChildState {
                storage,
                active_executions: Mutex::new(HashMap::new()),
'''
new = '''            Arc::new(EnvironmentChildState {
                storage,
                project_root: Arc::new(root.clone()),
                active_executions: Mutex::new(HashMap::new()),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment test state ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            generation,
            storage_limit_bytes: 4096,
            debug: false,
            session: "ab".repeat(32),
'''
new = '''            generation,
            storage_limit_bytes: 4096,
            project_root: root.to_string_lossy().into_owned(),
            debug: false,
            session: "ab".repeat(32),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment transport test ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            debug: false,
            session: "ab".repeat(32),
'''
new = '''            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            debug: false,
            session: "ab".repeat(32),
'''
if text.count(old) != 1:
    raise SystemExit(f"Environment session test ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

path.write_text(text, encoding="utf-8")


# Trusted Storage boundary: add one project-root write operation.
path = Path("container-runtime/crates/container-runtime-core/src/storage_capability.rs")
text = path.read_text(encoding="utf-8")

old = '''use std::sync::Arc;

use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
'''
new = '''use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage ProjectRoot import anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''pub const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];
'''
new = '''pub const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub const STORAGE_CAPABILITY_OPERATIONS: [&str; 5] =
    ["read", "list", "snapshot", "commit", "write"];
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage write operation allowlist anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''enum StorageMutationRequest {
    Put { path: String, data_hex: String },
    Delete { path: String },
}

pub fn storage_capability_operation_allowed'''
new = '''enum StorageMutationRequest {
    Put { path: String, data_hex: String },
    Delete { path: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectWriteRequest {
    path: String,
    data: Value,
    encoding: String,
    level: u8,
}

pub fn storage_capability_operation_allowed'''
if text.count(old) != 1:
    raise SystemExit(f"Storage write request anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''pub fn dispatch_storage_capability(
    storage: &Arc<EnvironmentStorageManager>,
    target: &str,
'''
new = '''pub fn dispatch_storage_capability(
    storage: &Arc<EnvironmentStorageManager>,
    project_root: &Path,
    target: &str,
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage ProjectRoot dispatch signature anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''        _ => unreachable!("operation allowlist checked above"),
    };

    encode_response(&response, max_response_bytes)
}

fn namespace_from_target'''
new = '''        "write" => {
            let [descriptor] = args.as_slice() else {
                return Err(invalid_args(
                    "write expects exactly one normalized write descriptor",
                ));
            };
            let descriptor: ProjectWriteRequest = serde_json::from_value(descriptor.clone())
                .map_err(|_| invalid_args("write descriptor is malformed"))?;
            let (bytes, encoding) = write_project_file(project_root, &descriptor)?;
            json!({
                "path": descriptor.path,
                "bytes": bytes,
                "encoding": encoding,
                "level": descriptor.level,
            })
        }
        _ => unreachable!("operation allowlist checked above"),
    };

    encode_response(&response, max_response_bytes)
}

fn write_project_file(
    project_root: &Path,
    descriptor: &ProjectWriteRequest,
) -> Result<(usize, &'static str), StorageCapabilityError> {
    if !(1..=3).contains(&descriptor.level) {
        return Err(invalid_args("write level must be 1, 2, or 3"));
    }

    let target = resolve_project_write_path(project_root, &descriptor.path)?;
    let (bytes, encoding) = encode_project_write_data(&descriptor.data, &descriptor.encoding)?;
    atomic_io::AtomicIo::new()
        .write_atomic(&target, &bytes)
        .map_err(|_| {
            error(
                "CAPABILITY_STORAGE_WRITE_FAILED",
                "project-root Storage write failed",
            )
        })?;
    Ok((bytes.len(), encoding))
}

fn resolve_project_write_path(
    project_root: &Path,
    logical_path: &str,
) -> Result<PathBuf, StorageCapabilityError> {
    let root = project_root.canonicalize().map_err(|_| {
        error(
            "CAPABILITY_STORAGE_PROJECT_ROOT_INVALID",
            "frozen ProjectRoot is unavailable",
        )
    })?;
    if !root.is_absolute() || !root.is_dir() {
        return Err(error(
            "CAPABILITY_STORAGE_PROJECT_ROOT_INVALID",
            "frozen ProjectRoot must be an existing absolute directory",
        ));
    }

    let relative = logical_path.strip_prefix("$$/").ok_or_else(|| {
        error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path must start with $$/",
        )
    })?;
    if relative.is_empty() || relative.contains('\\') || relative.contains('\0') {
        return Err(error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path is empty or contains a non-portable separator",
        ));
    }

    let mut segments = Vec::<OsString>::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(segment) => {
                if segment.to_string_lossy().contains(':') {
                    return Err(error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path contains a drive/stream separator",
                    ));
                }
                segments.push(segment.to_os_string());
            }
            _ => {
                return Err(error(
                    "CAPABILITY_STORAGE_PATH_INVALID",
                    "project write path may not contain root, dot, or parent components",
                ))
            }
        }
    }
    let Some(file_name) = segments.pop() else {
        return Err(error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path must name a file",
        ));
    };

    let mut parent = root.clone();
    for segment in segments {
        let next = parent.join(segment);
        match std::fs::symlink_metadata(&next) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let resolved = next.canonicalize().map_err(|_| {
                    error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path contains a broken symbolic link",
                    )
                })?;
                if !resolved.starts_with(&root) || !resolved.is_dir() {
                    return Err(error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path escapes frozen ProjectRoot",
                    ));
                }
                parent = resolved;
            }
            Ok(metadata) if metadata.is_dir() => {
                parent = next;
            }
            Ok(_) => {
                return Err(error(
                    "CAPABILITY_STORAGE_PATH_INVALID",
                    "project write parent component is not a directory",
                ))
            }
            Err(error_value) if error_value.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&next).map_err(|_| {
                    error(
                        "CAPABILITY_STORAGE_WRITE_FAILED",
                        "project write could not create a parent directory",
                    )
                })?;
                parent = next;
            }
            Err(_) => {
                return Err(error(
                    "CAPABILITY_STORAGE_WRITE_FAILED",
                    "project write could not inspect a parent directory",
                ))
            }
        }

        let resolved_parent = parent.canonicalize().map_err(|_| {
            error(
                "CAPABILITY_STORAGE_WRITE_FAILED",
                "project write parent directory could not be canonicalized",
            )
        })?;
        if !resolved_parent.starts_with(&root) {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write path escapes frozen ProjectRoot",
            ));
        }
        parent = resolved_parent;
    }

    let target = parent.join(file_name);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write target may not be a symbolic link",
            ))
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write target exists and is not a file",
            ))
        }
        Ok(_) => {}
        Err(error_value) if error_value.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(error(
                "CAPABILITY_STORAGE_WRITE_FAILED",
                "project write target could not be inspected",
            ))
        }
    }
    Ok(target)
}

fn encode_project_write_data(
    data: &Value,
    encoding: &str,
) -> Result<(Vec<u8>, &'static str), StorageCapabilityError> {
    let normalized = encoding.trim().to_ascii_uppercase().replace('-', "");
    match normalized.as_str() {
        "UTF8" => {
            let bytes = match data {
                Value::String(value) => value.as_bytes().to_vec(),
                _ => serde_json::to_vec(data)
                    .map_err(|_| invalid_args("write data could not be encoded as UTF-8 JSON"))?,
            };
            Ok((bytes, "UTF8"))
        }
        "UTF16LE" => {
            let text = project_write_text(data)?;
            let mut bytes = Vec::with_capacity(text.len().saturating_mul(2));
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            Ok((bytes, "UTF16LE"))
        }
        "UTF16BE" => {
            let text = project_write_text(data)?;
            let mut bytes = Vec::with_capacity(text.len().saturating_mul(2));
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_be_bytes());
            }
            Ok((bytes, "UTF16BE"))
        }
        "HEX" => {
            let value = data
                .as_str()
                .ok_or_else(|| invalid_args("HEX write data must be a hexadecimal string"))?;
            let bytes =
                hex::decode(value).map_err(|_| invalid_args("HEX write data is malformed"))?;
            Ok((bytes, "HEX"))
        }
        "BYTES" => {
            let values = data
                .as_array()
                .ok_or_else(|| invalid_args("BYTES write data must be an array of integers"))?;
            let mut bytes = Vec::with_capacity(values.len());
            for value in values {
                let byte = value
                    .as_u64()
                    .filter(|value| *value <= u8::MAX as u64)
                    .ok_or_else(|| invalid_args("BYTES values must be integers from 0 through 255"))?;
                bytes.push(byte as u8);
            }
            Ok((bytes, "BYTES"))
        }
        _ => Err(invalid_args(
            "write encoding must be UTF8, UTF16LE, UTF16BE, HEX, or BYTES",
        )),
    }
}

fn project_write_text(data: &Value) -> Result<String, StorageCapabilityError> {
    match data {
        Value::String(value) => Ok(value.clone()),
        _ => serde_json::to_string(data)
            .map_err(|_| invalid_args("write data could not be serialized as text")),
    }
}

fn namespace_from_target'''
if text.count(old) != 1:
    raise SystemExit(f"Storage write dispatch/helper anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    fn dispatch(
        storage: &Arc<EnvironmentStorageManager>,
        target: &str,
        operation: &str,
        args: Value,
    ) -> Result<Value, StorageCapabilityError> {
        let payload = serde_json::to_vec(&args).unwrap();
        let response = dispatch_storage_capability(
            storage,
            target,
'''
new = '''    fn dispatch(
        storage: &Arc<EnvironmentStorageManager>,
        project_root: &Path,
        target: &str,
        operation: &str,
        args: Value,
    ) -> Result<Value, StorageCapabilityError> {
        let payload = serde_json::to_vec(&args).unwrap();
        let response = dispatch_storage_capability(
            storage,
            project_root,
            target,
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage test helper ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

text = text.replace('dispatch(\n            &storage,\n            &target,', 'dispatch(\n            &storage,\n            &root,\n            &target,')
text = text.replace('dispatch(&storage, &target, "read",', 'dispatch(&storage, &root, &target, "read",')
text = text.replace('dispatch(&storage, &target, "list",', 'dispatch(&storage, &root, &target, "list",')
text = text.replace('dispatch(&storage, &target, "snapshot",', 'dispatch(&storage, &root, &target, "snapshot",')
text = text.replace('dispatch(\n            &storage,\n            "storage:uac",', 'dispatch(\n            &storage,\n            &root,\n            "storage:uac",')

old = '''            dispatch_storage_capability(&storage, "storage:uac", "open_host_file", b"[]", 1024)
                .unwrap_err();
'''
new = '''            dispatch_storage_capability(
                &storage,
                &root,
                "storage:uac",
                "open_host_file",
                b"[]",
                1024,
            )
            .unwrap_err();
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage invalid-op test ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''            dispatch_storage_capability(&storage, "storage:uac", "read", &payload, 8).unwrap_err();
'''
new = '''            dispatch_storage_capability(&storage, &root, "storage:uac", "read", &payload, 8)
                .unwrap_err();
'''
if text.count(old) != 1:
    raise SystemExit(f"Storage response-limit test ProjectRoot anchor count={text.count(old)}")
text = text.replace(old, new, 1)

test_marker = '''    #[test]
    fn invalid_mutation_aborts_the_entire_transaction() {
'''
if text.count(test_marker) != 1:
    raise SystemExit(f"Storage project write test insertion anchor count={text.count(test_marker)}")
tests = '''    #[test]
    fn project_write_uses_frozen_root_and_creates_parent_directories() {
        let (root, storage) = temp_storage("project-write", 4096);
        let target = storage_capability_target("accounts").unwrap();
        let response = dispatch(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/data/users/kate.json",
                "data":{"name":"Kate","active":true},
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap();
        assert_eq!(response["path"], "$$/data/users/kate.json");
        assert_eq!(response["encoding"], "UTF8");
        assert_eq!(response["level"], 1);
        let saved: Value =
            serde_json::from_slice(&std::fs::read(root.join("data/users/kate.json")).unwrap())
                .unwrap();
        assert_eq!(saved["name"], "Kate");
        assert_eq!(saved["active"], true);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_supports_utf16_and_binary_encodings() {
        let (root, storage) = temp_storage("project-encodings", 4096);
        let target = storage_capability_target("media").unwrap();
        dispatch(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/utf16.txt",
                "data":"K8",
                "encoding":"UTF16LE",
                "level":2
            }]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("utf16.txt")).unwrap(),
            vec![b'K', 0, b'8', 0]
        );

        dispatch(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/bytes.bin",
                "data":[0,1,127,255],
                "encoding":"BYTES",
                "level":3
            }]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("bytes.bin")).unwrap(),
            vec![0, 1, 127, 255]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_rejects_escape_and_invalid_level() {
        let (root, storage) = temp_storage("project-closed", 4096);
        let target = storage_capability_target("accounts").unwrap();
        let escape = dispatch(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/../escape.txt",
                "data":"nope",
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap_err();
        assert_eq!(escape.code, "CAPABILITY_STORAGE_PATH_INVALID");

        let invalid_level = dispatch(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/data/nope.txt",
                "data":"nope",
                "encoding":"UTF8",
                "level":4
            }]),
        )
        .unwrap_err();
        assert_eq!(invalid_level.code, "CAPABILITY_STORAGE_ARGS_INVALID");
        assert!(!root.join("data/nope.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

'''
text = text.replace(test_marker, tests + test_marker, 1)

path.write_text(text, encoding="utf-8")

print("storage project-root write boundary patch applied")
