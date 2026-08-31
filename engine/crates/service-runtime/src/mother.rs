use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::{ServiceCallError, ServiceManager, ServiceSnapshot};

const MOTHER_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
const MOTHER_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
const MOTHER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const MOTHER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MOTHER_CONNECTIONS: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceMotherReady {
    pub pid: u32,
    pub address: SocketAddr,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServiceMotherRequest {
    Call {
        token: String,
        service: String,
        function: String,
        args: Vec<Value>,
    },
    Event {
        token: String,
        service: String,
        event: Value,
    },
    Snapshot {
        token: String,
    },
    Shutdown {
        token: String,
    },
}

impl ServiceMotherRequest {
    fn token(&self) -> &str {
        match self {
            Self::Call { token, .. }
            | Self::Event { token, .. }
            | Self::Snapshot { token }
            | Self::Shutdown { token } => token,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServiceMotherResponse {
    Value { value: Value },
    Snapshots { services: Vec<ServiceSnapshot> },
    Ok,
    Error { code: String, message: String },
}

#[derive(Clone)]
pub(crate) struct ServiceMotherClient {
    address: SocketAddr,
    token: Arc<str>,
}

impl ServiceMotherClient {
    pub(crate) fn new(address: SocketAddr, token: String) -> Self {
        Self {
            address,
            token: Arc::<str>::from(token),
        }
    }

    pub(crate) async fn call(
        &self,
        service: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ServiceCallError> {
        let response = mother_rpc(
            self.address,
            ServiceMotherRequest::Call {
                token: self.token.to_string(),
                service: service.to_string(),
                function: function.to_string(),
                args,
            },
        )
        .await
        .map_err(|error| ServiceCallError::Ipc {
            service: service.to_string(),
            message: format!("Service Mother IPC failed: {error}"),
        })?;
        map_value_response(service, response)
    }

    pub(crate) async fn event(
        &self,
        service: &str,
        event: Value,
    ) -> Result<Value, ServiceCallError> {
        let response = mother_rpc(
            self.address,
            ServiceMotherRequest::Event {
                token: self.token.to_string(),
                service: service.to_string(),
                event,
            },
        )
        .await
        .map_err(|error| ServiceCallError::Ipc {
            service: service.to_string(),
            message: format!("Service Mother IPC failed: {error}"),
        })?;
        map_value_response(service, response)
    }

    pub(crate) async fn snapshot(&self) -> anyhow::Result<Vec<ServiceSnapshot>> {
        match mother_rpc(
            self.address,
            ServiceMotherRequest::Snapshot {
                token: self.token.to_string(),
            },
        )
        .await?
        {
            ServiceMotherResponse::Snapshots { services } => Ok(services),
            ServiceMotherResponse::Error { code, message } => {
                anyhow::bail!("Service Mother returned {code}: {message}")
            }
            other => {
                anyhow::bail!("Service Mother returned unexpected snapshot response: {other:?}")
            }
        }
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        match mother_rpc(
            self.address,
            ServiceMotherRequest::Shutdown {
                token: self.token.to_string(),
            },
        )
        .await?
        {
            ServiceMotherResponse::Ok => Ok(()),
            ServiceMotherResponse::Error { code, message } => {
                anyhow::bail!("Service Mother returned {code}: {message}")
            }
            other => {
                anyhow::bail!("Service Mother returned unexpected shutdown response: {other:?}")
            }
        }
    }
}

fn map_value_response(
    service: &str,
    response: ServiceMotherResponse,
) -> Result<Value, ServiceCallError> {
    match response {
        ServiceMotherResponse::Value { value } => Ok(value),
        ServiceMotherResponse::Error { code, message: _ } if code == "SVCM404" => {
            Err(ServiceCallError::Unknown {
                service: service.to_string(),
            })
        }
        ServiceMotherResponse::Error { code, message: _ } if code == "SVCM503" => {
            Err(ServiceCallError::Unavailable {
                service: service.to_string(),
            })
        }
        ServiceMotherResponse::Error { code, message } => Err(ServiceCallError::Remote {
            service: service.to_string(),
            code,
            message,
        }),
        other => Err(ServiceCallError::Ipc {
            service: service.to_string(),
            message: format!("unexpected Service Mother response: {other:?}"),
        }),
    }
}

pub fn new_service_mother_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub async fn run_service_mother(manager: ServiceManager, token: String) -> anyhow::Result<()> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("Service Mother token must be a 256-bit hexadecimal value");
    }
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
    let ready = ServiceMotherReady {
        pid: std::process::id(),
        address: listener.local_addr()?,
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;

    let token: Arc<str> = Arc::<str>::from(token);
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_MOTHER_CONNECTIONS));
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
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

async fn handle_connection(
    stream: TcpStream,
    manager: ServiceManager,
    token: Arc<str>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let line = tokio::time::timeout(
        MOTHER_FRAME_TIMEOUT,
        read_bounded_line(read, MOTHER_REQUEST_MAX_BYTES, "request"),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Service Mother request frame timed out"))??;
    let request: ServiceMotherRequest = serde_json::from_str(line.trim())?;
    if !constant_time_eq(request.token().as_bytes(), token.as_bytes()) {
        write_response(
            &mut write,
            &ServiceMotherResponse::Error {
                code: "SVCM401".into(),
                message: "unauthorized Service Mother request".into(),
            },
        )
        .await?;
        return Ok(());
    }

    let response = match request {
        ServiceMotherRequest::Call {
            service,
            function,
            args,
            ..
        } => match manager.call(&service, &function, args).await {
            Ok(value) => ServiceMotherResponse::Value { value },
            Err(error) => wire_service_error(error),
        },
        ServiceMotherRequest::Event { service, event, .. } => {
            match manager.event(&service, event).await {
                Ok(value) => ServiceMotherResponse::Value { value },
                Err(error) => wire_service_error(error),
            }
        }
        ServiceMotherRequest::Snapshot { .. } => ServiceMotherResponse::Snapshots {
            services: manager.snapshot().await,
        },
        ServiceMotherRequest::Shutdown { .. } => {
            manager.shutdown_all().await;
            let _ = shutdown_tx.send(true);
            ServiceMotherResponse::Ok
        }
    };
    write_response(&mut write, &response).await?;
    Ok(())
}

fn wire_service_error(error: ServiceCallError) -> ServiceMotherResponse {
    let (code, message) = match error {
        ServiceCallError::Unknown { service } => {
            ("SVCM404", format!("unknown service {service:?}"))
        }
        ServiceCallError::Unavailable { service } => {
            ("SVCM503", format!("service {service:?} is unavailable"))
        }
        other => ("SVCM500", other.to_string()),
    };
    ServiceMotherResponse::Error {
        code: code.into(),
        message,
    }
}

async fn write_response(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &ServiceMotherResponse,
) -> anyhow::Result<()> {
    let mut payload = serde_json::to_vec(response)?;
    if payload.len().saturating_add(1) > MOTHER_RESPONSE_MAX_BYTES {
        anyhow::bail!("Service Mother response exceeded {MOTHER_RESPONSE_MAX_BYTES} bytes");
    }
    payload.push(b'\n');
    tokio::time::timeout(MOTHER_FRAME_TIMEOUT, async {
        write.write_all(&payload).await?;
        write.shutdown().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("Service Mother response write timed out"))??;
    Ok(())
}

async fn mother_rpc(
    address: SocketAddr,
    request: ServiceMotherRequest,
) -> anyhow::Result<ServiceMotherResponse> {
    if !address.ip().is_loopback() {
        anyhow::bail!("Service Mother endpoint must be loopback");
    }
    let mut payload = serde_json::to_vec(&request)?;
    if payload.len().saturating_add(1) > MOTHER_REQUEST_MAX_BYTES {
        anyhow::bail!("Service Mother request exceeded {MOTHER_REQUEST_MAX_BYTES} bytes");
    }
    payload.push(b'\n');
    let stream = tokio::time::timeout(MOTHER_FRAME_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| anyhow::anyhow!("Service Mother connect timed out"))??;
    let (read, mut write) = stream.into_split();
    tokio::time::timeout(MOTHER_FRAME_TIMEOUT, async {
        write.write_all(&payload).await?;
        write.shutdown().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("Service Mother request write timed out"))??;
    let line = tokio::time::timeout(
        MOTHER_RESPONSE_TIMEOUT,
        read_bounded_line(read, MOTHER_RESPONSE_MAX_BYTES, "response"),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Service Mother response timed out"))??;
    Ok(serde_json::from_str(line.trim())?)
}

async fn read_bounded_line<R>(reader: R, max_bytes: usize, label: &str) -> anyhow::Result<String>
where
    R: AsyncRead + Unpin,
{
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = BufReader::new(reader).take(limit);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        anyhow::bail!("Service Mother {label} is empty");
    }
    if bytes > max_bytes {
        anyhow::bail!("Service Mother {label} exceeded {max_bytes} bytes");
    }
    if !line.ends_with('\n') {
        anyhow::bail!("Service Mother {label} is not newline terminated");
    }
    Ok(line)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&a, &b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mother_tokens_are_256_bit_hex() {
        let token = new_service_mother_token();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn constant_time_compare_checks_content_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[tokio::test]
    async fn bounded_frame_reader_rejects_oversized_and_unterminated_frames() {
        assert!(read_bounded_line(&b"abcd\n"[..], 3, "test").await.is_err());
        assert!(read_bounded_line(&b"abc"[..], 3, "test").await.is_err());
        assert_eq!(
            read_bounded_line(&b"abc\n"[..], 4, "test").await.unwrap(),
            "abc\n"
        );
    }
}
