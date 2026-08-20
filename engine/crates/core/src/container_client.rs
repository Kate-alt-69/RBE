use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ipc_protocol::{decode_response, read_frame, write_frame, HealthRequest, InspectRequest, Request, Response};

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
        let mut endpoint = self.endpoint.write().expect("container endpoint lock poisoned");
        endpoint.address = address;
        endpoint.token = token;
        endpoint.pid = pid;
        endpoint.generation = endpoint.generation.saturating_add(1);
    }

    pub fn endpoint_snapshot(&self) -> ContainerEndpointSnapshot {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned");
        ContainerEndpointSnapshot {
            address: endpoint.address,
            pid: endpoint.pid,
            generation: endpoint.generation,
        }
    }

    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::Health(HealthRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
        });
        let response = tokio::task::spawn_blocking(move || transact(&endpoint, &request)).await
            .map_err(|err| anyhow::anyhow!("container IPC health task failed: {err}"))??;
        match response {
            Response::Health { body, .. } => Ok(body),
            Response::Error { code, message, .. } => anyhow::bail!("container health failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container health response: {other:?}"),
        }
    }

    pub async fn inspect(&self) -> anyhow::Result<serde_json::Value> {
        let endpoint = self.endpoint.read().expect("container endpoint lock poisoned").clone();
        let request = Request::Inspect(InspectRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            execution_id: None,
        });
        let response = tokio::task::spawn_blocking(move || transact(&endpoint, &request)).await
            .map_err(|err| anyhow::anyhow!("container IPC inspection task failed: {err}"))??;
        match response {
            Response::Inspection { body, .. } => Ok(body),
            Response::Error { code, message, .. } => anyhow::bail!("container inspection failed [{code}]: {message}"),
            other => anyhow::bail!("unexpected container inspection response: {other:?}"),
        }
    }
}

fn transact(endpoint: &Endpoint, request: &Request) -> anyhow::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&endpoint.address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write_frame(&mut stream, request)?;
    let body = read_frame(&mut stream)?;
    Ok(decode_response(&body)?)
}

fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("backend-{}-{sequence}", std::process::id())
}
