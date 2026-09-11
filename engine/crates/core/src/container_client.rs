use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, CancelRequest, CapabilityGrant,
    ExecuteRequest, HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerCapabilityBinding {
    pub environment: String,
    pub generation: u64,
}

#[derive(Debug)]
pub struct ContainerAuthorizedExecution<'a> {
    pub identity: ContainerExecutionIdentity<'a>,
    pub artifact_hash: &'a str,
    pub wasm: Vec<u8>,
    pub grants: Vec<CapabilityGrant>,
    pub input: Vec<u8>,
    pub declared_cost: IpcWorkCost,
    pub timeout: Duration,
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
        identity: ContainerExecutionIdentity<'_>,
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
            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            artifact_hash: artifact_hash.to_string(),
            wasm,
        });
        match call(endpoint, request, Duration::from_secs(10)).await? {
            Response::ArtifactRegistered {
                runtime_image,
                source_id,
                capability_abi,
                already_present,
                ..
            } if runtime_image == identity.runtime_image
                && source_id == identity.source_id
                && capability_abi == CAPABILITY_ABI_VERSION =>
            {
                Ok(already_present)
            }
            Response::ArtifactRegistered { .. } => {
                anyhow::bail!("Container returned a mismatched artifact provenance binding")
            }
            Response::Error { code, message, .. } => {
                anyhow::bail!("container artifact registration failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected artifact registration response: {other:?}"),
        }
    }

    /// Register one source's capability set against an exact Environment or
    /// a Controller-owned logical profile such as `general`. Controller returns
    /// the exact Environment + generation it bound; callers never mint either.
    pub async fn register_capability_manifest(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<ContainerCapabilityBinding> {
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
            Response::CapabilityManifestRegistered {
                environment,
                generation,
                ..
            } => Ok(ContainerCapabilityBinding {
                environment,
                generation,
            }),
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

    pub async fn cancel_execution(&self, execution_id: &str) -> anyhow::Result<bool> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request_id = next_request_id();
        let request = Request::Cancel(CancelRequest {
            request_id: request_id.clone(),
            auth_token: endpoint.token.clone(),
            execution_id: execution_id.to_string(),
        });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Cancelled {
                request_id: returned,
            } if returned == request_id => Ok(true),
            Response::Cancelled { .. } => {
                anyhow::bail!("Container returned a mismatched cancellation response")
            }
            Response::Error {
                request_id: Some(returned),
                code,
                ..
            } if returned == request_id && code == "NOT_FOUND" => Ok(false),
            Response::Error {
                request_id: Some(returned),
                code,
                message,
            } if returned == request_id => {
                anyhow::bail!("container cancellation failed [{code}]: {message}")
            }
            Response::Error { .. } => {
                anyhow::bail!("Container returned a mismatched cancellation error")
            }
            other => anyhow::bail!("unexpected container cancellation response: {other:?}"),
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
            None => match self.cancel_execution(&execution_id).await {
                Ok(true) => anyhow::bail!(
                    "container execution {execution_id} exceeded the caller timeout; cancellation was accepted"
                ),
                Ok(false) => {
                    // Completion can linearize immediately after AwaitResult
                    // reports pending and before Cancel reaches Controller. A
                    // NOT_FOUND cancellation therefore gets one short recovery
                    // read before we classify the synchronous call as timed out.
                    match self
                        .await_result(&execution_id, Duration::from_millis(100))
                        .await?
                    {
                        Some(output) => Ok(output),
                        None => anyhow::bail!(
                            "container execution {execution_id} exceeded the caller timeout and was no longer cancellable"
                        ),
                    }
                }
                Err(error) => anyhow::bail!(
                    "container execution {execution_id} exceeded the caller timeout and cancellation failed: {error}"
                ),
            },
        }
    }

    /// Admit and execute one immutable artifact under one exact capability
    /// identity. Registration is deliberately repeated/idempotent so an
    /// Environment generation restart cannot leave a stale backend-side cache
    /// authorizing work that Controller has already invalidated.
    pub async fn execute_authorized(
        &self,
        request: ContainerAuthorizedExecution<'_>,
    ) -> anyhow::Result<Vec<u8>> {
        self.register_artifact(request.identity, request.artifact_hash, request.wasm)
            .await?;
        let binding = self
            .register_capability_manifest(request.identity, request.grants)
            .await?;
        let exact_identity = ContainerExecutionIdentity {
            runtime_image: request.identity.runtime_image,
            source_id: request.identity.source_id,
            environment: &binding.environment,
        };
        tracing::debug!(
            runtime_image = exact_identity.runtime_image,
            source_id = exact_identity.source_id,
            requested_environment = request.identity.environment,
            environment = exact_identity.environment,
            generation = binding.generation,
            artifact_hash = request.artifact_hash,
            "Container execution authority admitted"
        );
        self.execute_and_wait(
            exact_identity,
            request.artifact_hash,
            request.input,
            request.declared_cost,
            request.timeout,
        )
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn respond(stream: &mut TcpStream, response: &Response) {
        write_frame(stream, response).unwrap();
    }

    #[tokio::test]
    async fn execute_and_wait_cancels_execution_when_synchronous_deadline_expires() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server_token = token.clone();
        let server = std::thread::spawn(move || {
            for step in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let frame = read_frame(&mut stream).unwrap();
                let request: Request = serde_json::from_slice(&frame).unwrap();
                match (step, request) {
                    (0, Request::Execute(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        respond(
                            &mut stream,
                            &Response::Accepted {
                                request_id: request.request_id,
                                execution_id: "exec-timeout".into(),
                            },
                        );
                    }
                    (1, Request::AwaitResult(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        assert_eq!(request.execution_id, "exec-timeout");
                        respond(
                            &mut stream,
                            &Response::ExecutionPending {
                                request_id: request.request_id,
                                execution_id: request.execution_id,
                            },
                        );
                    }
                    (2, Request::Cancel(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        assert_eq!(request.execution_id, "exec-timeout");
                        respond(
                            &mut stream,
                            &Response::Cancelled {
                                request_id: request.request_id,
                            },
                        );
                    }
                    (_, other) => panic!("unexpected mock Container request: {other:?}"),
                }
            }
        });

        let client = ContainerClient::new(address, token, None);
        let error = client
            .execute_and_wait(
                ContainerExecutionIdentity {
                    runtime_image: "runtime-image",
                    source_id: "route:test",
                    environment: "general-1",
                },
                "artifact",
                Vec::new(),
                IpcWorkCost {
                    cpu: 1,
                    memory: 1,
                    io: 0,
                    network: 0,
                },
                Duration::from_millis(1),
            )
            .await
            .expect_err("pending synchronous execution must be cancelled");
        assert!(error.to_string().contains("cancellation was accepted"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_not_found_recovers_completion_race() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server_token = token.clone();
        let server = std::thread::spawn(move || {
            for step in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let frame = read_frame(&mut stream).unwrap();
                let request: Request = serde_json::from_slice(&frame).unwrap();
                match (step, request) {
                    (0, Request::Execute(request)) => respond(
                        &mut stream,
                        &Response::Accepted {
                            request_id: request.request_id,
                            execution_id: "exec-race".into(),
                        },
                    ),
                    (1, Request::AwaitResult(request)) => respond(
                        &mut stream,
                        &Response::ExecutionPending {
                            request_id: request.request_id,
                            execution_id: request.execution_id,
                        },
                    ),
                    (2, Request::Cancel(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        respond(
                            &mut stream,
                            &Response::Error {
                                request_id: Some(request.request_id),
                                code: "NOT_FOUND".into(),
                                message: "execution already completed".into(),
                            },
                        );
                    }
                    (3, Request::AwaitResult(request)) => respond(
                        &mut stream,
                        &Response::ExecutionFinished {
                            request_id: request.request_id,
                            execution_id: request.execution_id,
                            output: b"done".to_vec(),
                            elapsed_ms: 2,
                        },
                    ),
                    (_, other) => panic!("unexpected mock Container request: {other:?}"),
                }
            }
        });

        let client = ContainerClient::new(address, token, None);
        let output = client
            .execute_and_wait(
                ContainerExecutionIdentity {
                    runtime_image: "runtime-image",
                    source_id: "route:test",
                    environment: "general-1",
                },
                "artifact",
                Vec::new(),
                IpcWorkCost {
                    cpu: 1,
                    memory: 1,
                    io: 0,
                    network: 0,
                },
                Duration::from_millis(1),
            )
            .await
            .unwrap();
        assert_eq!(output, b"done");
        server.join().unwrap();
    }
}
