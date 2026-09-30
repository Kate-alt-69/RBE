use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context};
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibraryHostCallReply, LibrarySessionBinding,
    MAX_LIBRARY_PAYLOAD_BYTES,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;

pub const CAPABILITY: &str = "net:tcp";
const HANDLE_RANDOM_BYTES: usize = 24;
const MAX_CONNECTIONS_GLOBAL: usize = 256;
const MAX_CONNECTIONS_PER_SESSION: usize = 8;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone)]
struct ManagedConnection {
    owner: String,
    package: String,
    stream: Arc<AsyncMutex<TcpStream>>,
}

static CONNECTIONS: OnceLock<Mutex<BTreeMap<String, ManagedConnection>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectRequest {
    host: String,
    port: u16,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    handle: String,
    data: Vec<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    handle: String,
    max_bytes: usize,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    handle: String,
}

pub fn grant() -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        CAPABILITY,
        CAPABILITY,
        [
            "connect".to_string(),
            "write".to_string(),
            "read".to_string(),
            "close".to_string(),
        ],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package public TCP capability grant")
}

pub async fn dispatch_authorized_call(
    package: &str,
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<LibraryHostCallReply> {
    let grant = binding
        .authorize_host_call(call)
        .context("authorize package TCP call against accepted Library Host session")?;
    if call.capability != CAPABILITY || call.target != CAPABILITY {
        bail!("package TCP call does not match admitted net:tcp authority");
    }
    let owner = binding
        .accepted_info()
        .map(|info| info.capability_identity.clone())
        .ok_or_else(|| anyhow::anyhow!("package TCP call arrived before Library Host acceptance"))?;

    let payload = match call.operation.as_str() {
        "connect" => connect(package, &owner, &call.payload).await?,
        "write" => write(package, &owner, &call.payload).await?,
        "read" => read(package, &owner, &call.payload).await?,
        "close" => close(package, &owner, &call.payload).await?,
        other => bail!("unsupported package TCP operation {other:?}"),
    };

    if payload.len() > grant.max_response_bytes {
        bail!(
            "package TCP response exceeded admitted capability limit: limit={}, observed={}",
            grant.max_response_bytes,
            payload.len()
        );
    }
    LibraryHostCallReply::success(call.call_id, payload)
        .context("encode successful package TCP host-call reply")
}

pub fn clear_all() {
    if let Some(registry) = CONNECTIONS.get() {
        match registry.lock() {
            Ok(mut connections) => connections.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }
}

async fn connect(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: ConnectRequest =
        serde_json::from_slice(payload).context("decode package net:tcp connect request")?;
    if request.port == 0 {
        bail!("package net:tcp port must be in 1..=65535");
    }
    validate_host(&request.host)?;
    ensure_connection_capacity(owner)?;

    let timeout = request_timeout(request.timeout_ms)?;
    let addresses = resolve_public_addresses(&request.host, request.port).await?;
    let mut connected = None;
    let mut last_error = None;
    for address in addresses {
        match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                connected = Some((stream, address));
                break;
            }
            Ok(Err(error)) => last_error = Some(error.to_string()),
            Err(_) => last_error = Some("connection timed out".to_string()),
        }
    }
    let (stream, peer) = connected.ok_or_else(|| {
        anyhow::anyhow!(
            "package net:tcp could not connect to public destination {}:{}: {}",
            request.host,
            request.port,
            last_error.unwrap_or_else(|| "no usable public address".to_string())
        )
    })?;
    stream
        .set_nodelay(true)
        .context("configure package TCP connection")?;

    let handle = insert_connection(package, owner, stream)?;
    serde_json::to_vec(&json!({
        "handle": handle,
        "peer": peer.to_string(),
    }))
    .context("encode package net:tcp connect response")
}

async fn write(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: WriteRequest =
        serde_json::from_slice(payload).context("decode package net:tcp write request")?;
    if request.data.is_empty() || request.data.len() > MAX_WRITE_BYTES {
        bail!(
            "package net:tcp write data must contain 1..={} bytes",
            MAX_WRITE_BYTES
        );
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = request_timeout(request.timeout_ms)?;
    let mut stream = connection.stream.lock().await;
    tokio::time::timeout(timeout, stream.write_all(&request.data))
        .await
        .map_err(|_| anyhow::anyhow!("package net:tcp write timed out"))?
        .context("write package TCP data")?;

    serde_json::to_vec(&json!({ "written": request.data.len() }))
        .context("encode package net:tcp write response")
}

async fn read(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: ReadRequest =
        serde_json::from_slice(payload).context("decode package net:tcp read request")?;
    if request.max_bytes == 0 || request.max_bytes > MAX_READ_BYTES {
        bail!(
            "package net:tcp max_bytes must be in 1..={}",
            MAX_READ_BYTES
        );
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = request_timeout(request.timeout_ms)?;
    let mut stream = connection.stream.lock().await;
    let mut data = vec![0u8; request.max_bytes];
    let read = tokio::time::timeout(timeout, stream.read(&mut data))
        .await
        .map_err(|_| anyhow::anyhow!("package net:tcp read timed out"))?
        .context("read package TCP data")?;
    data.truncate(read);

    serde_json::to_vec(&json!({
        "data": data,
        "eof": read == 0,
    }))
    .context("encode package net:tcp read response")
}

async fn close(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: CloseRequest =
        serde_json::from_slice(payload).context("decode package net:tcp close request")?;
    let connection = remove_connection(package, owner, &request.handle)?;
    let mut stream = connection.stream.lock().await;
    let _ = stream.shutdown().await;
    Ok(Vec::new())
}

fn registry() -> &'static Mutex<BTreeMap<String, ManagedConnection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock_registry(
) -> anyhow::Result<std::sync::MutexGuard<'static, BTreeMap<String, ManagedConnection>>> {
    registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP connection registry lock is poisoned"))
}

fn ensure_connection_capacity(owner: &str) -> anyhow::Result<()> {
    let connections = lock_registry()?;
    if connections.len() >= MAX_CONNECTIONS_GLOBAL {
        bail!(
            "package TCP connection registry reached global limit {}",
            MAX_CONNECTIONS_GLOBAL
        );
    }
    let owned = connections
        .values()
        .filter(|connection| connection.owner == owner)
        .count();
    if owned >= MAX_CONNECTIONS_PER_SESSION {
        bail!(
            "package TCP session reached connection limit {}",
            MAX_CONNECTIONS_PER_SESSION
        );
    }
    Ok(())
}

fn insert_connection(package: &str, owner: &str, stream: TcpStream) -> anyhow::Result<String> {
    let mut connections = lock_registry()?;
    if connections.len() >= MAX_CONNECTIONS_GLOBAL {
        bail!("package TCP connection registry became full before insertion");
    }
    if connections
        .values()
        .filter(|connection| connection.owner == owner)
        .count()
        >= MAX_CONNECTIONS_PER_SESSION
    {
        bail!("package TCP session became full before insertion");
    }

    for _ in 0..8 {
        let mut random = [0u8; HANDLE_RANDOM_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let handle = format!("tcp:{}", hex::encode(random));
        if !connections.contains_key(&handle) {
            connections.insert(
                handle.clone(),
                ManagedConnection {
                    owner: owner.to_string(),
                    package: package.to_string(),
                    stream: Arc::new(AsyncMutex::new(stream)),
                },
            );
            return Ok(handle);
        }
    }
    bail!("could not allocate unique package TCP handle")
}

fn connection_for(package: &str, owner: &str, handle: &str) -> anyhow::Result<ManagedConnection> {
    validate_handle(handle)?;
    let connections = lock_registry()?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TCP handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package TCP handle is not owned by the accepted package session");
    }
    Ok(connection.clone())
}

fn remove_connection(package: &str, owner: &str, handle: &str) -> anyhow::Result<ManagedConnection> {
    validate_handle(handle)?;
    let mut connections = lock_registry()?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TCP handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package TCP handle is not owned by the accepted package session");
    }
    connections
        .remove(handle)
        .ok_or_else(|| anyhow::anyhow!("package TCP handle disappeared during close"))
}

fn validate_handle(handle: &str) -> anyhow::Result<()> {
    let Some(value) = handle.strip_prefix("tcp:") else {
        bail!("invalid package TCP handle");
    };
    if value.len() != HANDLE_RANDOM_BYTES * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid package TCP handle");
    }
    Ok(())
}

fn request_timeout(value: Option<u64>) -> anyhow::Result<Duration> {
    let millis = value.unwrap_or(DEFAULT_TIMEOUT_MS);
    if millis == 0 {
        bail!("package net:tcp timeout_ms must be positive");
    }
    Ok(Duration::from_millis(millis.min(MAX_TIMEOUT_MS)))
}

async fn resolve_public_addresses(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if forbidden_ip(ip) {
            bail!("package net:tcp destination is not publicly routable");
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve package TCP destination {host:?}"))?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("package net:tcp DNS resolution returned no addresses");
    }
    if addresses.len() > MAX_RESOLVED_ADDRESSES {
        bail!(
            "package net:tcp DNS resolution exceeded {} addresses",
            MAX_RESOLVED_ADDRESSES
        );
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        bail!("package net:tcp hostname resolves to a non-public address");
    }
    Ok(addresses)
}

fn validate_host(host: &str) -> anyhow::Result<()> {
    if host.is_empty()
        || host.len() > 253
        || host.chars().any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("package net:tcp host is invalid");
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }

    let host = host.trim_end_matches('.');
    if host.is_empty()
        || !host.contains('.')
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".local")
    {
        bail!("package net:tcp host must be a public DNS name");
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("package net:tcp host contains an invalid DNS label");
        }
    }
    Ok(())
}

fn forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip == Ipv4Addr::BROADCAST
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn forbidden_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || ip.to_ipv4_mapped().is_some_and(forbidden_ipv4)
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_ipv4(ip),
        IpAddr::V6(ip) => forbidden_ipv6(ip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_grant_is_bounded_to_stateful_operations() {
        let grant = grant().unwrap();
        assert_eq!(grant.capability, CAPABILITY);
        assert_eq!(grant.target, CAPABILITY);
        assert_eq!(
            grant.operations,
            std::collections::BTreeSet::from([
                "close".to_string(),
                "connect".to_string(),
                "read".to_string(),
                "write".to_string(),
            ])
        );
    }

    #[test]
    fn private_and_special_addresses_are_rejected() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(forbidden_ip(ip), "{address} must be forbidden");
        }
        assert!(!forbidden_ip("1.1.1.1".parse().unwrap()));
        assert!(!forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn host_validation_rejects_local_and_ambiguous_names() {
        for host in ["", "localhost", "printer", "mail.local", "-bad.example", "bad-.example"] {
            assert!(validate_host(host).is_err(), "{host:?} must be rejected");
        }
        assert!(validate_host("smtp.example.com").is_ok());
        assert!(validate_host("1.1.1.1").is_ok());
    }

    #[test]
    fn malformed_handles_are_rejected() {
        assert!(validate_handle("tcp:1234").is_err());
        assert!(validate_handle("http:000000000000000000000000000000000000000000000000").is_err());
        assert!(validate_handle(&format!("tcp:{}", "a".repeat(HANDLE_RANDOM_BYTES * 2))).is_ok());
    }
}
