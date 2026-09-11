from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# Shared typed protocol between verified Container and Backend.
ipc = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
replace_once(
    ipc,
    "pub const CAPABILITY_ABI_VERSION: u16 = 1;\nconst MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;",
    "pub const CAPABILITY_ABI_VERSION: u16 = 1;\npub const HOST_CAPABILITY_PROTOCOL_VERSION: u16 = 1;\nconst MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;",
    "host capability protocol version",
)
replace_once(
    ipc,
    "pub const MAX_CAPABILITY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;\npub const MAX_AWAIT_RESULT_MS: u64 = 30_000;",
    "pub const MAX_CAPABILITY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;\npub const MAX_HOST_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 64 * 1024;\npub const MAX_AWAIT_RESULT_MS: u64 = 30_000;",
    "host capability frame bound",
)
replace_once(
    ipc,
    '''#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerCapabilityResult {
    Success {
        call_id: u64,
        payload: Vec<u8>,
    },
    Error {
        call_id: u64,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutputFrame {''',
    '''#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerCapabilityResult {
    Success {
        call_id: u64,
        payload: Vec<u8>,
    },
    Error {
        call_id: u64,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapabilityRequest {
    pub version: u16,
    pub auth_token: String,
    pub execution_id: String,
    pub call_id: u64,
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostCapabilityResponse {
    Success {
        execution_id: String,
        call_id: u64,
        payload: Vec<u8>,
    },
    Error {
        execution_id: String,
        call_id: u64,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutputFrame {''',
    "host capability wire types",
)

# Container-side authenticated forwarding after CapabilityBroker authorization.
envp = Path("container-runtime/crates/container-bin/src/environment_process.rs")
replace_once(
    envp,
    '''use ipc_protocol::{
    read_frame, read_worker_output, write_frame, write_worker_capability_result,
    write_worker_input, CapabilityKind, WorkerCapabilityCall, WorkerCapabilityResult,
    WorkerOutputFrame, WorkerResultFrame, CAPABILITY_ABI_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES,
};''',
    '''use ipc_protocol::{
    read_frame, read_worker_output, write_frame, write_worker_capability_result,
    write_worker_input, CapabilityKind, HostCapabilityRequest, HostCapabilityResponse,
    WorkerCapabilityCall, WorkerCapabilityResult, WorkerOutputFrame, WorkerResultFrame,
    CAPABILITY_ABI_VERSION, HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES,
};''',
    "environment host capability imports",
)
replace_once(
    envp,
    '''pub struct CapabilityDispatchRequest {
    pub execution_id: String,
    pub kind: CapabilityKind,''',
    '''pub struct CapabilityDispatchRequest {
    pub execution_id: String,
    pub call_id: u64,
    pub kind: CapabilityKind,''',
    "dispatch call identity",
)
replace_once(
    envp,
    '''pub fn unavailable_capability_dispatcher() -> CapabilityDispatcher {
    Arc::new(|request| {
        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.kind,
            request.target.as_str(),
            request.operation.as_str(),
            request.payload.len(),
            request.max_response_bytes,
        );
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}

struct ManagedEnvironment {''',
    '''pub fn unavailable_capability_dispatcher() -> CapabilityDispatcher {
    Arc::new(|request| {
        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.call_id,
            request.kind,
            request.target.as_str(),
            request.operation.as_str(),
            request.payload.len(),
            request.max_response_bytes,
        );
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}

pub fn authenticated_host_capability_dispatcher(
    address: SocketAddr,
    token: String,
) -> Result<CapabilityDispatcher> {
    if !address.ip().is_loopback() {
        bail!("trusted host capability endpoint must be loopback");
    }
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("trusted host capability token must be 256-bit hexadecimal");
    }

    Ok(Arc::new(move |request| {
        let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|_| {
            CapabilityDispatchError {
                code: "CAPABILITY_HOST_UNAVAILABLE".into(),
                message: "trusted host capability endpoint is unavailable".into(),
            }
        })?;
        stream.set_nodelay(true).map_err(|_| CapabilityDispatchError {
            code: "CAPABILITY_HOST_IO".into(),
            message: "failed to configure trusted host capability channel".into(),
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_IO".into(),
                message: "failed to configure trusted host capability channel".into(),
            })?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_IO".into(),
                message: "failed to configure trusted host capability channel".into(),
            })?;

        let execution_id = request.execution_id.clone();
        let call_id = request.call_id;
        let host_request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: token.clone(),
            execution_id: execution_id.clone(),
            call_id,
            kind: request.kind,
            target: request.target,
            operation: request.operation,
            payload: request.payload,
            max_response_bytes: request.max_response_bytes,
        };
        write_frame(&mut stream, &host_request).map_err(|_| CapabilityDispatchError {
            code: "CAPABILITY_HOST_IO".into(),
            message: "failed to send trusted host capability request".into(),
        })?;
        let frame = read_frame(&mut stream).map_err(|_| CapabilityDispatchError {
            code: "CAPABILITY_HOST_IO".into(),
            message: "failed to read trusted host capability response".into(),
        })?;
        let response: HostCapabilityResponse =
            serde_json::from_slice(&frame).map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_PROTOCOL".into(),
                message: "trusted host capability response was malformed".into(),
            })?;
        match response {
            HostCapabilityResponse::Success {
                execution_id: returned_execution,
                call_id: returned_call,
                payload,
            } if returned_execution == execution_id && returned_call == call_id => Ok(payload),
            HostCapabilityResponse::Error {
                execution_id: returned_execution,
                call_id: returned_call,
                code,
                message,
            } if returned_execution == execution_id && returned_call == call_id => {
                Err(CapabilityDispatchError { code, message })
            }
            _ => Err(CapabilityDispatchError {
                code: "CAPABILITY_HOST_PROTOCOL".into(),
                message: "trusted host capability response identity did not match the request".into(),
            }),
        }
    }))
}

struct ManagedEnvironment {''',
    "authenticated host dispatcher",
)
replace_once(
    envp,
    '''        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
            kind: call.kind,''',
    '''        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
            call_id: call.call_id,
            kind: call.kind,''',
    "dispatch call id propagation",
)
replace_once(
    envp,
    '''        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--environment-child");''',
    '''        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--environment-child")
            .env_remove("RBE_CONTAINER_TOKEN")
            .env_remove("RBE_HOST_CAPABILITY_ADDR")
            .env_remove("RBE_HOST_CAPABILITY_TOKEN");''',
    "environment child secret stripping",
)

# Container Controller selects the trusted dispatcher from Backend-provided credentials.
container_main = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    container_main,
    "use std::net::{TcpListener, TcpStream};",
    "use std::net::{SocketAddr, TcpListener, TcpStream};",
    "container socket address import",
)
replace_once(
    container_main,
    '''    let token = env::var("RBE_CONTAINER_TOKEN").ok();
    let capability_broker = Arc::new(CapabilityBroker::new(debug));
    let capability_dispatcher = environment_process::unavailable_capability_dispatcher();''',
    '''    let token = env::var("RBE_CONTAINER_TOKEN").ok();
    let capability_broker = Arc::new(CapabilityBroker::new(debug));
    let capability_dispatcher = match (
        env::var("RBE_HOST_CAPABILITY_ADDR").ok(),
        env::var("RBE_HOST_CAPABILITY_TOKEN").ok(),
    ) {
        (Some(address), Some(host_token)) => {
            let address = address.parse::<SocketAddr>().map_err(|error| {
                anyhow::anyhow!("invalid RBE_HOST_CAPABILITY_ADDR: {error}")
            })?;
            environment_process::authenticated_host_capability_dispatcher(address, host_token)?
        }
        (None, None) => environment_process::unavailable_capability_dispatcher(),
        _ => {
            return Err(anyhow::anyhow!(
                "trusted host capability endpoint requires both address and token"
            ));
        }
    };''',
    "controller host dispatcher selection",
)
replace_once(
    container_main,
    '''            let mut child = match std::process::Command::new(&exe)
                .arg("--monitor")
                .arg("--pid")''',
    '''            let mut child = match std::process::Command::new(&exe)
                .arg("--monitor")
                .env_remove("RBE_CONTAINER_TOKEN")
                .env_remove("RBE_HOST_CAPABILITY_ADDR")
                .env_remove("RBE_HOST_CAPABILITY_TOKEN")
                .arg("--pid")''',
    "monitor secret stripping",
)

# Backend-owned, loopback-only Service capability adapter.
host_module = Path("engine/crates/backend/src/host_capability.rs")
host_module.write_text(r'''use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ipc_protocol::{
    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse, HOST_CAPABILITY_PROTOCOL_VERSION,
    MAX_CAPABILITY_PAYLOAD_BYTES, MAX_HOST_CAPABILITY_FRAME_BYTES,
};
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use service_runtime::ServiceManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinHandle;

const HOST_CAPABILITY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 64;
const MAX_EXECUTION_ID_BYTES: usize = 128;
const MAX_LOGICAL_NAME_BYTES: usize = 256;

#[derive(Clone)]
pub struct HostCapabilityEndpoint {
    address: SocketAddr,
    token: String,
}

impl HostCapabilityEndpoint {
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

pub struct HostCapabilityBridge {
    endpoint: HostCapabilityEndpoint,
    services: Arc<RwLock<Option<ServiceManager>>>,
    server_task: JoinHandle<()>,
}

impl HostCapabilityBridge {
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            anyhow::bail!("host capability bridge unexpectedly bound a non-loopback address");
        }
        let token = generate_token();
        let endpoint = HostCapabilityEndpoint {
            address,
            token: token.clone(),
        };
        let services = Arc::new(RwLock::new(None));
        let server_services = Arc::clone(&services);
        let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        let server_task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::error!(error = %error, "host capability bridge accept failed");
                        break;
                    }
                };
                if !peer.ip().is_loopback() {
                    tracing::warn!(%peer, "host capability bridge rejected non-loopback peer");
                    continue;
                }
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!("host capability bridge saturated; rejecting call");
                        continue;
                    }
                };
                let services = Arc::clone(&server_services);
                let expected_token = token.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match tokio::time::timeout(
                        HOST_CAPABILITY_TIMEOUT,
                        handle_connection(stream, expected_token, services),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = %error, "host capability bridge call failed")
                        }
                        Err(_) => tracing::warn!("host capability bridge call timed out"),
                    }
                });
            }
        });

        Ok(Self {
            endpoint,
            services,
            server_task,
        })
    }

    pub fn endpoint(&self) -> HostCapabilityEndpoint {
        self.endpoint.clone()
    }

    pub async fn install_service_manager(&self, manager: ServiceManager) {
        *self.services.write().await = Some(manager);
    }
}

impl Drop for HostCapabilityBridge {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    expected_token: String,
    services: Arc<RwLock<Option<ServiceManager>>>,
) -> anyhow::Result<()> {
    let request: HostCapabilityRequest = read_typed_frame(&mut stream).await?;
    let response = dispatch_request(request, &expected_token, &services).await;
    write_typed_frame(&mut stream, &response).await?;
    Ok(())
}

async fn dispatch_request(
    request: HostCapabilityRequest,
    expected_token: &str,
    services: &Arc<RwLock<Option<ServiceManager>>>,
) -> HostCapabilityResponse {
    let error = |code: &str, message: &str| HostCapabilityResponse::Error {
        execution_id: request.execution_id.clone(),
        call_id: request.call_id,
        code: code.into(),
        message: message.into(),
    };

    if request.version != HOST_CAPABILITY_PROTOCOL_VERSION {
        return error("CAPABILITY_HOST_PROTOCOL", "unsupported host capability protocol");
    }
    if !constant_time_eq(request.auth_token.as_bytes(), expected_token.as_bytes()) {
        return error("CAPABILITY_HOST_AUTH_FAILED", "trusted host capability authentication failed");
    }
    if request.execution_id.is_empty() || request.execution_id.len() > MAX_EXECUTION_ID_BYTES {
        return error("CAPABILITY_HOST_INVALID_REQUEST", "invalid execution identity");
    }
    if request.kind != CapabilityKind::Service {
        return error(
            "CAPABILITY_KIND_UNSUPPORTED",
            "this trusted host adapter only supports Service capabilities",
        );
    }
    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return error("CAPABILITY_REQUEST_TOO_LARGE", "service capability request exceeded protocol limit");
    }
    let Some(service_name) = normalize_service_target(&request.target) else {
        return error("CAPABILITY_HOST_INVALID_TARGET", "invalid logical service target");
    };
    if !valid_logical_name(&request.operation) {
        return error("CAPABILITY_HOST_INVALID_OPERATION", "invalid logical service operation");
    }
    let args: Vec<Value> = match serde_json::from_slice(&request.payload) {
        Ok(args) => args,
        Err(_) => {
            return error(
                "CAPABILITY_SERVICE_ARGS_INVALID",
                "service capability payload must be a JSON argument array",
            )
        }
    };
    let manager = match services.read().await.clone() {
        Some(manager) => manager,
        None => {
            return error(
                "CAPABILITY_SERVICE_MANAGER_UNAVAILABLE",
                "trusted Service Manager is not ready",
            )
        }
    };

    let value = match manager.call(service_name, &request.operation, args).await {
        Ok(value) => value,
        Err(call_error) => {
            tracing::warn!(
                execution_id = %request.execution_id,
                call_id = request.call_id,
                service = %service_name,
                operation = %request.operation,
                error = %call_error,
                "authorized sandbox Service capability call failed"
            );
            return error("CAPABILITY_SERVICE_CALL_FAILED", "trusted Service call failed");
        }
    };
    let payload = match serde_json::to_vec(&value) {
        Ok(payload) => payload,
        Err(_) => {
            return error(
                "CAPABILITY_SERVICE_RESPONSE_INVALID",
                "trusted Service returned an unserializable response",
            )
        }
    };
    let response_limit = request
        .max_response_bytes
        .min(MAX_CAPABILITY_PAYLOAD_BYTES as u64) as usize;
    if payload.len() > response_limit {
        return error(
            "CAPABILITY_RESPONSE_TOO_LARGE",
            "trusted Service response exceeded the capability grant",
        );
    }

    HostCapabilityResponse::Success {
        execution_id: request.execution_id,
        call_id: request.call_id,
        payload,
    }
}

fn normalize_service_target(target: &str) -> Option<&str> {
    let target = target.strip_prefix("service:").unwrap_or(target);
    valid_logical_name(target).then_some(target)
}

fn valid_logical_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LOGICAL_NAME_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
        })
}

async fn read_typed_frame<T: DeserializeOwned>(stream: &mut TcpStream) -> anyhow::Result<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let length = u32::from_be_bytes(len) as usize;
    if length == 0 || length > MAX_HOST_CAPABILITY_FRAME_BYTES {
        anyhow::bail!("host capability frame length is invalid");
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(Into::into)
}

async fn write_typed_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.is_empty() || body.len() > MAX_HOST_CAPABILITY_FRAME_BYTES {
        anyhow::bail!("host capability response frame length is invalid");
    }
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_target_normalization_is_logical_only() {
        assert_eq!(normalize_service_target("service:uac-cache"), Some("uac-cache"));
        assert_eq!(normalize_service_target("mail"), Some("mail"));
        assert_eq!(normalize_service_target("../mail"), None);
        assert_eq!(normalize_service_target("service:mail/socket"), None);
    }

    #[test]
    fn host_token_comparison_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
''', encoding="utf-8")

# Verified Container receives bridge credentials; child sandboxes never do.
container_process = Path("engine/crates/backend/src/container_process.rs")
replace_once(
    container_process,
    '''impl ContainerProcess {
    pub async fn spawn(binary: &Path, settings: &config::ContainersConfig) -> anyhow::Result<Self> {''',
    '''impl ContainerProcess {
    pub async fn spawn(
        binary: &Path,
        settings: &config::ContainersConfig,
        host_capability: &crate::host_capability::HostCapabilityEndpoint,
    ) -> anyhow::Result<Self> {''',
    "container spawn host endpoint parameter",
)
replace_once(
    container_process,
    '''            command
                .env("RBE_CONTAINER_TOKEN", &token)
                .kill_on_drop(true);''',
    '''            command
                .env("RBE_CONTAINER_TOKEN", &token)
                .env("RBE_HOST_CAPABILITY_ADDR", host_capability.address().to_string())
                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())
                .kill_on_drop(true);''',
    "container bridge environment",
)

backend_main = Path("engine/crates/backend/src/main.rs")
replace_once(
    backend_main,
    '''mod error_reporter_daemon;
mod host_bootstrap;
mod maintenance_notice;''',
    '''mod error_reporter_daemon;
mod host_bootstrap;
mod host_capability;
mod maintenance_notice;''',
    "backend host capability module",
)
replace_once(
    backend_main,
    '''    let initial_container =
        container_process::ContainerProcess::spawn(&container_path, &config.containers).await?;''',
    '''    let host_capability_bridge = host_capability::HostCapabilityBridge::start().await?;
    let host_capability_endpoint = host_capability_bridge.endpoint();
    tracing::info!(
        address = %host_capability_endpoint.address(),
        "trusted host capability bridge ready"
    );

    let initial_container = container_process::ContainerProcess::spawn(
        &container_path,
        &config.containers,
        &host_capability_endpoint,
    )
    .await?;''',
    "start host capability bridge before container",
)
replace_once(
    backend_main,
    '''        container_path.clone(),
        config.containers.clone(),
        container_process.clone(),''',
    '''        container_path.clone(),
        config.containers.clone(),
        host_capability_endpoint.clone(),
        container_process.clone(),''',
    "supervisor host endpoint argument",
)
replace_once(
    backend_main,
    '''    let service_manager = service_mother
        .as_ref()
        .map(|mother| mother.manager())
        .unwrap_or_default();

    let (video_manager, video_worker_task) = if config.video_manager.enabled {''',
    '''    let service_manager = service_mother
        .as_ref()
        .map(|mother| mother.manager())
        .unwrap_or_default();
    host_capability_bridge
        .install_service_manager(service_manager.clone())
        .await;
    tracing::info!("trusted host capability bridge attached to Service Manager");

    let (video_manager, video_worker_task) = if config.video_manager.enabled {''',
    "install service manager into host bridge",
)
replace_once(
    backend_main,
    '''fn spawn_container_supervisor(
    binary: PathBuf,
    settings: config::ContainersConfig,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,''',
    '''fn spawn_container_supervisor(
    binary: PathBuf,
    settings: config::ContainersConfig,
    host_capability: host_capability::HostCapabilityEndpoint,
    process: Arc<tokio::sync::Mutex<container_process::ContainerProcess>>,''',
    "supervisor host endpoint parameter",
)
replace_once(
    backend_main,
    '''                        match container_process::ContainerProcess::spawn(&binary, &settings).await {''',
    '''                        match container_process::ContainerProcess::spawn(
                            &binary,
                            &settings,
                            &host_capability,
                        )
                        .await
                        {''',
    "crash replacement host endpoint",
)
replace_once(
    backend_main,
    '''                    match container_process::ContainerProcess::spawn(&binary, &settings).await {''',
    '''                    match container_process::ContainerProcess::spawn(
                        &binary,
                        &settings,
                        &host_capability,
                    )
                    .await
                    {''',
    "rolling refresh host endpoint",
)
