use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use core_lib::{call_public_http, PUBLIC_HTTP_TARGET};
use ipc_protocol::{
    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse, CAPABILITY_ABI_VERSION,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
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
const MAX_SOURCE_ID_BYTES: usize = 512;

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
        return error(
            "CAPABILITY_HOST_PROTOCOL",
            "unsupported host capability protocol",
        );
    }
    if !constant_time_eq(request.auth_token.as_bytes(), expected_token.as_bytes()) {
        return error(
            "CAPABILITY_HOST_AUTH_FAILED",
            "trusted host capability authentication failed",
        );
    }
    if request.execution_id.is_empty() || request.execution_id.len() > MAX_EXECUTION_ID_BYTES {
        return error(
            "CAPABILITY_HOST_INVALID_REQUEST",
            "invalid execution identity",
        );
    }
    if request.capability_abi != CAPABILITY_ABI_VERSION
        || !valid_runtime_image(&request.runtime_image)
        || !valid_source_id(&request.source_id)
        || !valid_environment_identity(&request.environment)
    {
        return error(
            "CAPABILITY_HOST_PROVENANCE_INVALID",
            "trusted host capability provenance is invalid",
        );
    }
    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return error(
            "CAPABILITY_REQUEST_TOO_LARGE",
            "capability request exceeded protocol limit",
        );
    }
    if request.kind == CapabilityKind::Network {
        if request.target != PUBLIC_HTTP_TARGET {
            return error(
                "CAPABILITY_HOST_INVALID_TARGET",
                "invalid logical Network capability target",
            );
        }
        if !matches!(request.operation.as_str(), "get" | "post" | "request") {
            return error(
                "CAPABILITY_HOST_INVALID_OPERATION",
                "invalid public HTTP capability operation",
            );
        }
        let args: Vec<Value> = match serde_json::from_slice(&request.payload) {
            Ok(args) => args,
            Err(_) => {
                return error(
                    "CAPABILITY_NETWORK_ARGS_INVALID",
                    "public HTTP capability payload must be a JSON argument array",
                )
            }
        };
        let value = match call_public_http(&request.operation, &args).await {
            Ok(value) => value,
            Err(call_error) => {
                tracing::warn!(
                    execution_id = %request.execution_id,
                    runtime_image = %request.runtime_image,
                    source_id = %request.source_id,
                    environment = %request.environment,
                    generation = request.generation,
                    call_id = request.call_id,
                    operation = %request.operation,
                    error = %call_error,
                    "authorized sandbox Network capability call failed"
                );
                return error(
                    "CAPABILITY_NETWORK_CALL_FAILED",
                    "trusted public HTTP request failed",
                );
            }
        };
        let payload = match serde_json::to_vec(&value) {
            Ok(payload) => payload,
            Err(_) => {
                return error(
                    "CAPABILITY_NETWORK_RESPONSE_INVALID",
                    "trusted public HTTP broker returned an unserializable response",
                )
            }
        };
        let response_limit = request
            .max_response_bytes
            .min(MAX_CAPABILITY_PAYLOAD_BYTES as u64) as usize;
        if payload.len() > response_limit {
            return error(
                "CAPABILITY_RESPONSE_TOO_LARGE",
                "trusted public HTTP response exceeded the capability grant",
            );
        }
        return HostCapabilityResponse::Success {
            execution_id: request.execution_id,
            call_id: request.call_id,
            payload,
        };
    }
    if request.kind != CapabilityKind::Service {
        return error(
            "CAPABILITY_KIND_UNSUPPORTED",
            "this trusted host adapter does not support that capability kind",
        );
    }
    let Some(service_name) = normalize_service_target(&request.target) else {
        return error(
            "CAPABILITY_HOST_INVALID_TARGET",
            "invalid logical service target",
        );
    };
    if !valid_logical_name(&request.operation) {
        return error(
            "CAPABILITY_HOST_INVALID_OPERATION",
            "invalid logical service operation",
        );
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
                runtime_image = %request.runtime_image,
                source_id = %request.source_id,
                environment = %request.environment,
                generation = request.generation,
                call_id = request.call_id,
                service = %service_name,
                operation = %request.operation,
                error = %call_error,
                "authorized sandbox Service capability call failed"
            );
            return error(
                "CAPABILITY_SERVICE_CALL_FAILED",
                "trusted Service call failed",
            );
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

fn valid_runtime_image(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SOURCE_ID_BYTES
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

fn valid_environment_identity(value: &str) -> bool {
    matches!(
        value,
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5" | "payment"
    )
}

fn normalize_service_target(target: &str) -> Option<&str> {
    let target = target.strip_prefix("service:").unwrap_or(target);
    valid_logical_name(target).then_some(target)
}

fn valid_logical_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LOGICAL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
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
    fn attested_host_provenance_format_is_strict() {
        assert!(valid_runtime_image(&"ab".repeat(32)));
        assert!(!valid_runtime_image(&"AB".repeat(32)));
        assert!(valid_source_id("module:physical:media/player"));
        assert!(!valid_source_id("route:\napi"));
        assert!(valid_environment_identity("general-5"));
        assert!(valid_environment_identity("payment"));
        assert!(!valid_environment_identity("general"));
        assert!(!valid_environment_identity("visitor-ip-1"));
    }

    #[tokio::test]
    async fn host_bridge_rejects_invalid_attested_provenance_before_dispatch() {
        let services = Arc::new(RwLock::new(None));
        let request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: "secret".into(),
            execution_id: "exec-1".into(),
            runtime_image: "NOT-A-RUNTIME-IMAGE".into(),
            source_id: "route:api/test".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-1".into(),
            generation: 3,
            call_id: 1,
            kind: CapabilityKind::Network,
            target: PUBLIC_HTTP_TARGET.into(),
            operation: "get".into(),
            payload: br#"["https://example.com"]"#.to_vec(),
            max_response_bytes: 4096,
        };
        let response = dispatch_request(request, "secret", &services).await;
        let HostCapabilityResponse::Error { code, .. } = response else {
            panic!("malformed provenance must fail before capability dispatch");
        };
        assert_eq!(code, "CAPABILITY_HOST_PROVENANCE_INVALID");
    }

    #[test]
    fn service_target_normalization_is_logical_only() {
        assert_eq!(
            normalize_service_target("service:uac-cache"),
            Some("uac-cache")
        );
        assert_eq!(normalize_service_target("mail"), Some("mail"));
        assert_eq!(normalize_service_target("../mail"), None);
        assert_eq!(normalize_service_target("service:mail/socket"), None);
    }

    #[test]
    fn network_target_is_fixed_logical_public_http() {
        assert_eq!(PUBLIC_HTTP_TARGET, "public-http");
        assert_ne!(PUBLIC_HTTP_TARGET, "127.0.0.1:80");
        assert_ne!(PUBLIC_HTTP_TARGET, "example.com:443");
    }

    #[test]
    fn host_token_comparison_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
