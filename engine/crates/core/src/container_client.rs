use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ipc_protocol::{
    decode_response, read_frame, write_frame, HealthRequest, InspectRequest,
    PrepareRefreshRequest, Request, Response, ResumeRequest,
};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ContainerEndpointSnapshot {
    pub address: SocketAddr,
    pub pid: Option<u32>,
    pub generation: u64,
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
        Self { endpoint: Arc::new(RwLock::new(Endpoint { address, token, pid, generation: 1 })) }
    }

    pub fn update_endpoint(&self, address: SocketAddr, token: String, pid: Option<u32>) {
        let mut endpoint = self.endpoint.write().expect("container endpoint lock poisoned");
        endpoint.address = address;
        endpoint.token = token;
        endpoint.pid = pid;
        endpoint.generation = endpoint.generation.saturating_add(1);
    }

    pub fn endpoint_snapshot(&self) -> ContainerEndpointSnapshot {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned");
        ContainerEndpointSnapshot { address: endpoint.address, pid: endpoint.pid, generation: endpoint.generation }
    }

    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::Health(HealthRequest { request_id: next_request_id(), auth_token: endpoint.token.clone() });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Health { body, .. } => Ok(body),
            Response::Error { code, message, .. } => anyhow::bail!("container health failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container health response: {other:?}"),
        }
    }

    pub async fn inspect(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::Inspect(InspectRequest { request_id: next_request_id(), auth_token: endpoint.token.clone(), execution_id: None });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Inspection { body, .. } => Ok(body),
            Response::Error { code, message, .. } => anyhow::bail!("container inspection failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container inspection response: {other:?}"),
        }
    }

    pub async fn prepare_refresh(&self, drain_timeout: Duration) -> anyhow::Result<()> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::PrepareRefresh(PrepareRefreshRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            drain_timeout_ms: drain_timeout.as_millis().min(u64::MAX as u128) as u64,
        });
        let call_timeout = drain_timeout.saturating_add(Duration::from_secs(5));
        match call(endpoint, request, call_timeout).await? {
            Response::ReadyForRefresh { .. } => Ok(()),
            Response::Error { code, message, .. } => anyhow::bail!("container refresh preparation failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container refresh response: {other:?}"),
        }
    }

    pub async fn resume(&self) -> anyhow::Result<()> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::Resume(ResumeRequest { request_id: next_request_id(), auth_token: endpoint.token.clone() });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Resumed { .. } => Ok(()),
            Response::Error { code, message, .. } => anyhow::bail!("container resume failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container resume response: {other:?}"),
        }
    }
}

async fn call(endpoint: Endpoint, request: Request, timeout: Duration) -> anyhow::Result<Response> {
    tokio::task::spawn_blocking(move || transact(&endpoint, &request, timeout)).await
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
