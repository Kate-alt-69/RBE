use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, CapabilityGrant, ExecuteRequest,
    HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
    RegisterCapabilityManifestRequest, Request, Response, ResumeRequest, WorkCost as IpcWorkCost,
    CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES,
    MAX_EXECUTION_OUTPUT_BYTES,
};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ContainerEndpointSnapshot {
    pub address: SocketAddr,
    pub pid: Option<u32>,
    pub generation: u64,
}

/// Immutable caller identity used for capability registration and execution.
/// Environment generation is deliberately absent because Container Controller
/// is the sole authority allowed to stamp the live generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExecutionIdentity<'a> {
    pub runtime_image: &'a str,
    pub source_id: &'a str,
    pub environment: &'a str,
}

#[derive(Clone)]
struct Endpoint {
    address: SocketAddr,
    token: String,
    pid: Option<u32>,
    generation: u64,
}

#[derive(Clone)]
pub struct ContainerClient {
    endpoint: Arc<RwLock<Endpoint>>,
}

impl ContainerClient {
    pub fn new(address: SocketAddr, token: String, pid: Option<u32>) -> Self {
        Self {
            endpoint: Arc::new(RwLock::new(Endpoint {
                address,
                token,
                pid,
                generation: 1,
            })),
        }
    }

    pub fn update_endpoint(&self, address: SocketAddr, token: String, pid: Option<u32>) {
        let mut endpoint = self
            .endpoint
            .write()
            .expect("container endpoint lock poisoned");
        endpoint.address = address;
        endpoint.token = token;
        endpoint.pid = pid;
        endpoint.generation = endpoint.generation.saturating_add(1);
    }

    pub fn endpoint_snapshot(&self) -> ContainerEndpointSnapshot {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned");
        ContainerEndpointSnapshot {
            address: endpoint.address,
            pid: endpoint.pid,
            generation: endpoint.generation,
        }
    }

    pub async fn register_artifact(
        &self,
        artifact_hash: &str,
        wasm: Vec<u8>,
    ) -> anyhow::Result<bool> {
        if wasm.is_empty() || wasm.len() > MAX_ARTIFACT_BYTES {
            anyhow::bail!("WASM artifact size is outside the Container IPC limit");
        }
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            artifact_hash: artifact_hash.to_string(),
            wasm,
        });
        match call(endpoint, request, Duration::from_secs(10)).await? {
            Response::ArtifactRegistered {
                already_present, ..
            } => Ok(already_present),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container artifact registration failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected artifact registration response: {other:?}"),
        }
    }

    /// Register the exact capability set for one Runtime Image source.
    /// Container Controller supplies and returns the live Environment generation.
    pub async fn register_capability_manifest(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<u64> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::RegisterCapabilityManifest(RegisterCapabilityManifestRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            capability_abi: CAPABILITY_ABI_VERSION,
            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            environment: identity.environment.to_string(),
            grants,
        });
        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::CapabilityManifestRegistered { generation, .. } => Ok(generation),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container capability registration failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected capability registration response: {other:?}"),
        }
    }

    pub async fn execute(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
    ) -> anyhow::Result<String> {
        if input.len() > MAX_EXECUTION_INPUT_BYTES {
            anyhow::bail!("Container execution input exceeds the IPC limit");
        }
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::Execute(ExecuteRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: identity.environment.to_string(),
            artifact_hash: artifact_hash.to_string(),
            declared_cost,
            input,
        });
        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::Accepted { execution_id, .. } => Ok(execution_id),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container execution submission failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container execute response: {other:?}"),
        }
    }

    pub async fn await_result(
        &self,
        execution_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let timeout_ms = timeout
            .as_millis()
            .clamp(1, u128::from(MAX_AWAIT_RESULT_MS)) as u64;
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::AwaitResult(AwaitResultRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            execution_id: execution_id.to_string(),
            timeout_ms,
        });
        let call_timeout = Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(3));
        match call(endpoint, request, call_timeout).await? {
            Response::ExecutionFinished { output, .. } => {
                if output.len() > MAX_EXECUTION_OUTPUT_BYTES {
                    anyhow::bail!("Container returned an oversized execution result");
                }
                Ok(Some(output))
            }
            Response::ExecutionPending { .. } => Ok(None),
            Response::ExecutionFailed { code, message, .. } => {
                anyhow::bail!("container execution failed [{code}]: {message}")
            }
            Response::Error { code, message, .. } => {
                anyhow::bail!("container result wait failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container result response: {other:?}"),
        }
    }

    pub async fn execute_and_wait(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let execution_id = self
            .execute(identity, artifact_hash, input, declared_cost)
            .await?;
        match self.await_result(&execution_id, timeout).await? {
            Some(output) => Ok(output),
            None => {
                anyhow::bail!("container execution {execution_id} is still pending after timeout")
            }
        }
    }

    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::Health(HealthRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
        });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Health { body, .. } => Ok(body),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container health failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container health response: {other:?}"),
        }
    }

    pub async fn inspect(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::Inspect(InspectRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            execution_id: None,
        });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Inspection { body, .. } => Ok(body),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container inspection failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container inspection response: {other:?}"),
        }
    }

    pub async fn prepare_refresh(&self, drain_timeout: Duration) -> anyhow::Result<()> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::PrepareRefresh(PrepareRefreshRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            drain_timeout_ms: drain_timeout.as_millis().min(u64::MAX as u128) as u64,
        });
        let call_timeout = drain_timeout.saturating_add(Duration::from_secs(5));
        match call(endpoint, request, call_timeout).await? {
            Response::ReadyForRefresh { .. } => Ok(()),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container refresh preparation failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container refresh response: {other:?}"),
        }
    }

    pub async fn resume(&self) -> anyhow::Result<()> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::Resume(ResumeRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
        });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Resumed { .. } => Ok(()),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container resume failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container resume response: {other:?}"),
        }
    }
}

async fn call(endpoint: Endpoint, request: Request, timeout: Duration) -> anyhow::Result<Response> {
    tokio::task::spawn_blocking(move || transact(&endpoint, &request, timeout))
        .await
        .map_err(|err| anyhow::anyhow!("container IPC task failed: {err}"))?
}

fn transact(endpoint: &Endpoint, request: &Request, timeout: Duration) -> anyhow::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&endpoint.address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write_frame(&mut stream, request)?;
    let body = read_frame(&mut stream)?;
    Ok(decode_response(&body)?)
}

fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("backend-{}-{sequence}", std::process::id())
}
