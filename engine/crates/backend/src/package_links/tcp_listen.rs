use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibrarySessionBinding, MAX_LIBRARY_PAYLOAD_BYTES,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;

pub const CAPABILITY: &str = "net:tcp-listen";
const HANDLE_RANDOM_BYTES: usize = 24;
const MAX_LISTENERS_GLOBAL: usize = 32;
const MAX_LISTENERS_PER_SESSION: usize = 2;
const MAX_CONNECTIONS_GLOBAL: usize = 512;
const MAX_CONNECTIONS_PER_SESSION: usize = 32;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 16 * 1024;
const DEFAULT_ACCEPT_TIMEOUT_MS: u64 = 30_000;
const MAX_ACCEPT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_IO_TIMEOUT_MS: u64 = 5_000;
const MAX_IO_TIMEOUT_MS: u64 = 10_000;
const LISTENER_IDLE_LEASE: Duration = Duration::from_secs(120);
const LISTENER_REAP_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct ManagedListener {
    owner: String,
    package: String,
    listener: Arc<AsyncMutex<TcpListener>>,
    last_touch: Arc<Mutex<Instant>>,
}

#[derive(Clone)]
struct ManagedInboundConnection {
    owner: String,
    package: String,
    stream: Arc<AsyncMutex<TcpStream>>,
}

static LISTENERS: OnceLock<Mutex<BTreeMap<String, ManagedListener>>> = OnceLock::new();
static CONNECTIONS: OnceLock<Mutex<BTreeMap<String, ManagedInboundConnection>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindRequest {
    address: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptRequest {
    handle: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IoRequest {
    handle: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    data: Vec<u8>,
    #[serde(default)]
    max_bytes: usize,
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
            "bind".to_string(),
            "accept".to_string(),
            "read".to_string(),
            "write".to_string(),
            "close_connection".to_string(),
            "close".to_string(),
        ],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package TCP listener capability grant")
}

pub async fn dispatch_authorized_call(
    package: &str,
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<Vec<u8>> {
    if call.capability != CAPABILITY || call.target != CAPABILITY {
        bail!("package TCP listener call does not match admitted net:tcp-listen authority");
    }
    let accepted = binding.accepted_info().ok_or_else(|| {
        anyhow::anyhow!("package TCP listener call arrived before Library Host acceptance")
    })?;
    if !accepted.granted_capabilities.contains(CAPABILITY) {
        bail!("package net:tcp-listen is not granted to the accepted Library Host session");
    }
    let owner = accepted.capability_identity.clone();

    match call.operation.as_str() {
        "bind" => bind(package, &owner, &call.payload).await,
        "accept" => accept(package, &owner, &call.payload).await,
        "read" => read(package, &owner, &call.payload).await,
        "write" => write(package, &owner, &call.payload).await,
        "close_connection" => close_connection(package, &owner, &call.payload).await,
        "close" => close_listener(package, &owner, &call.payload),
        other => bail!("unsupported package TCP listener operation {other:?}"),
    }
}

pub fn revoke_package(package: &str) {
    if let Some(registry) = LISTENERS.get() {
        match registry.lock() {
            Ok(mut listeners) => listeners.retain(|_, listener| listener.package != package),
            Err(poisoned) => poisoned
                .into_inner()
                .retain(|_, listener| listener.package != package),
        }
    }
    if let Some(registry) = CONNECTIONS.get() {
        match registry.lock() {
            Ok(mut connections) => {
                connections.retain(|_, connection| connection.package != package)
            }
            Err(poisoned) => poisoned
                .into_inner()
                .retain(|_, connection| connection.package != package),
        }
    }
}

async fn bind(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: BindRequest =
        serde_json::from_slice(payload).context("decode package net:tcp-listen bind request")?;
    if request.port == 0 {
        bail!("package net:tcp-listen port must be in 1..=65535");
    }
    let address = validate_bind_address(&request.address)?;
    remove_stale_package_sessions(package, owner);
    ensure_listener_capacity(owner)?;

    let socket = SocketAddr::new(address, request.port);
    let listener = TcpListener::bind(socket).await.map_err(|error| {
        if request.port < 1024 {
            anyhow::anyhow!(
                "package net:tcp-listen could not bind privileged port {} on {}: {}; the RBE host/platform must permit privileged TCP binds (for Linux typically CAP_NET_BIND_SERVICE or equivalent)",
                request.port,
                request.address,
                error
            )
        } else {
            anyhow::anyhow!(
                "package net:tcp-listen could not bind {}:{}: {}",
                request.address,
                request.port,
                error
            )
        }
    })?;
    let local = listener
        .local_addr()
        .context("inspect package TCP listener local address")?;
    let handle = insert_listener(package, owner, listener)?;
    spawn_listener_reaper(handle.clone(), package.to_string(), owner.to_string());

    serde_json::to_vec(&json!({
        "handle": handle,
        "local": local.to_string(),
    }))
    .context("encode package net:tcp-listen bind response")
}

async fn accept(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: AcceptRequest =
        serde_json::from_slice(payload).context("decode package net:tcp-listen accept request")?;
    let listener = listener_for(package, owner, &request.handle)?;
    touch_listener(&listener)?;
    ensure_connection_capacity(owner)?;
    let timeout = accept_timeout(request.timeout_ms)?;

    let listener_guard = listener.listener.lock().await;
    let (stream, peer) = tokio::time::timeout(timeout, listener_guard.accept())
        .await
        .map_err(|_| anyhow::anyhow!("package net:tcp-listen accept timed out"))?
        .context("accept package TCP listener connection")?;
    drop(listener_guard);
    stream
        .set_nodelay(true)
        .context("configure accepted package TCP connection")?;
    let local = stream
        .local_addr()
        .context("inspect accepted package TCP local address")?;
    let connection_handle = insert_connection(package, owner, stream)?;

    serde_json::to_vec(&json!({
        "handle": connection_handle,
        "peer": peer.to_string(),
        "local": local.to_string(),
    }))
    .context("encode package net:tcp-listen accept response")
}

async fn read(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: IoRequest =
        serde_json::from_slice(payload).context("decode package net:tcp-listen read request")?;
    if request.max_bytes == 0 || request.max_bytes > MAX_READ_BYTES || !request.data.is_empty() {
        bail!("package net:tcp-listen read max_bytes must be in 1..={MAX_READ_BYTES} and data must be empty");
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = io_timeout(request.timeout_ms)?;
    let mut stream = connection.stream.lock().await;
    let mut data = vec![0u8; request.max_bytes];
    let read = tokio::time::timeout(timeout, stream.read(&mut data))
        .await
        .map_err(|_| anyhow::anyhow!("package net:tcp-listen read timed out"))?
        .context("read accepted package TCP data")?;
    data.truncate(read);
    serde_json::to_vec(&json!({ "data": data, "eof": read == 0 }))
        .context("encode package net:tcp-listen read response")
}

async fn write(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: IoRequest =
        serde_json::from_slice(payload).context("decode package net:tcp-listen write request")?;
    if request.data.is_empty() || request.data.len() > MAX_WRITE_BYTES || request.max_bytes != 0 {
        bail!("package net:tcp-listen write data must contain 1..={MAX_WRITE_BYTES} bytes and max_bytes must be zero");
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = io_timeout(request.timeout_ms)?;
    let mut stream = connection.stream.lock().await;
    tokio::time::timeout(timeout, stream.write_all(&request.data))
        .await
        .map_err(|_| anyhow::anyhow!("package net:tcp-listen write timed out"))?
        .context("write accepted package TCP data")?;
    serde_json::to_vec(&json!({ "written": request.data.len() }))
        .context("encode package net:tcp-listen write response")
}

async fn close_connection(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: CloseRequest = serde_json::from_slice(payload)
        .context("decode package net:tcp-listen close_connection request")?;
    let connection = remove_connection(package, owner, &request.handle)?;
    let mut stream = connection.stream.lock().await;
    let _ = stream.shutdown().await;
    Ok(Vec::new())
}

fn close_listener(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: CloseRequest =
        serde_json::from_slice(payload).context("decode package net:tcp-listen close request")?;
    let _ = remove_listener(package, owner, &request.handle)?;
    Ok(Vec::new())
}

fn listener_registry() -> &'static Mutex<BTreeMap<String, ManagedListener>> {
    LISTENERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn connection_registry() -> &'static Mutex<BTreeMap<String, ManagedInboundConnection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn ensure_listener_capacity(owner: &str) -> anyhow::Result<()> {
    let listeners = listener_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP listener registry lock is poisoned"))?;
    if listeners.len() >= MAX_LISTENERS_GLOBAL {
        bail!("package TCP listener registry reached global limit {MAX_LISTENERS_GLOBAL}");
    }
    if listeners
        .values()
        .filter(|listener| listener.owner == owner)
        .count()
        >= MAX_LISTENERS_PER_SESSION
    {
        bail!("package TCP listener session reached listener limit {MAX_LISTENERS_PER_SESSION}");
    }
    Ok(())
}

fn ensure_connection_capacity(owner: &str) -> anyhow::Result<()> {
    let connections = connection_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package inbound TCP connection registry lock is poisoned"))?;
    if connections.len() >= MAX_CONNECTIONS_GLOBAL {
        bail!("package inbound TCP registry reached global limit {MAX_CONNECTIONS_GLOBAL}");
    }
    if connections
        .values()
        .filter(|connection| connection.owner == owner)
        .count()
        >= MAX_CONNECTIONS_PER_SESSION
    {
        bail!("package TCP listener session reached accepted-connection limit {MAX_CONNECTIONS_PER_SESSION}");
    }
    Ok(())
}

fn insert_listener(package: &str, owner: &str, listener: TcpListener) -> anyhow::Result<String> {
    let mut listeners = listener_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP listener registry lock is poisoned"))?;
    for _ in 0..8 {
        let handle = random_handle("tcp-listen");
        if !listeners.contains_key(&handle) {
            listeners.insert(
                handle.clone(),
                ManagedListener {
                    owner: owner.to_string(),
                    package: package.to_string(),
                    listener: Arc::new(AsyncMutex::new(listener)),
                    last_touch: Arc::new(Mutex::new(Instant::now())),
                },
            );
            return Ok(handle);
        }
    }
    bail!("could not allocate unique package TCP listener handle")
}

fn insert_connection(package: &str, owner: &str, stream: TcpStream) -> anyhow::Result<String> {
    let mut connections = connection_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package inbound TCP connection registry lock is poisoned"))?;
    for _ in 0..8 {
        let handle = random_handle("tcp-in");
        if !connections.contains_key(&handle) {
            connections.insert(
                handle.clone(),
                ManagedInboundConnection {
                    owner: owner.to_string(),
                    package: package.to_string(),
                    stream: Arc::new(AsyncMutex::new(stream)),
                },
            );
            return Ok(handle);
        }
    }
    bail!("could not allocate unique package inbound TCP handle")
}

fn listener_for(package: &str, owner: &str, handle: &str) -> anyhow::Result<ManagedListener> {
    validate_handle(handle, "tcp-listen")?;
    let listeners = listener_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP listener registry lock is poisoned"))?;
    let listener = listeners
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TCP listener handle"))?;
    if listener.owner != owner || listener.package != package {
        bail!("package TCP listener handle is not owned by the accepted package session");
    }
    Ok(listener.clone())
}

fn connection_for(
    package: &str,
    owner: &str,
    handle: &str,
) -> anyhow::Result<ManagedInboundConnection> {
    validate_handle(handle, "tcp-in")?;
    let connections = connection_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package inbound TCP connection registry lock is poisoned"))?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package inbound TCP handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package inbound TCP handle is not owned by the accepted package session");
    }
    Ok(connection.clone())
}

fn remove_listener(package: &str, owner: &str, handle: &str) -> anyhow::Result<ManagedListener> {
    validate_handle(handle, "tcp-listen")?;
    let mut listeners = listener_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP listener registry lock is poisoned"))?;
    let listener = listeners
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TCP listener handle"))?;
    if listener.owner != owner || listener.package != package {
        bail!("package TCP listener handle is not owned by the accepted package session");
    }
    listeners
        .remove(handle)
        .ok_or_else(|| anyhow::anyhow!("package TCP listener disappeared during close"))
}

fn remove_connection(
    package: &str,
    owner: &str,
    handle: &str,
) -> anyhow::Result<ManagedInboundConnection> {
    validate_handle(handle, "tcp-in")?;
    let mut connections = connection_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package inbound TCP connection registry lock is poisoned"))?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package inbound TCP handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package inbound TCP handle is not owned by the accepted package session");
    }
    connections
        .remove(handle)
        .ok_or_else(|| anyhow::anyhow!("package inbound TCP handle disappeared during close"))
}

fn remove_stale_package_sessions(package: &str, owner: &str) {
    if let Ok(mut listeners) = listener_registry().lock() {
        listeners.retain(|_, listener| listener.package != package || listener.owner == owner);
    }
    if let Ok(mut connections) = connection_registry().lock() {
        connections
            .retain(|_, connection| connection.package != package || connection.owner == owner);
    }
}

fn touch_listener(listener: &ManagedListener) -> anyhow::Result<()> {
    let mut last_touch = listener
        .last_touch
        .lock()
        .map_err(|_| anyhow::anyhow!("package TCP listener lease lock is poisoned"))?;
    *last_touch = Instant::now();
    Ok(())
}

fn spawn_listener_reaper(handle: String, package: String, owner: String) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LISTENER_REAP_INTERVAL).await;
            let stale = {
                let listeners = match listener_registry().lock() {
                    Ok(listeners) => listeners,
                    Err(_) => return,
                };
                let Some(listener) = listeners.get(&handle) else {
                    return;
                };
                if listener.package != package || listener.owner != owner {
                    return;
                }
                let idle = match listener.last_touch.lock() {
                    Ok(last_touch) => last_touch.elapsed() >= LISTENER_IDLE_LEASE,
                    Err(_) => true,
                };
                idle
            };
            if stale {
                if let Ok(mut listeners) = listener_registry().lock() {
                    if listeners.get(&handle).is_some_and(|listener| {
                        listener.package == package && listener.owner == owner
                    }) {
                        listeners.remove(&handle);
                    }
                }
                return;
            }
        }
    });
}

fn validate_bind_address(value: &str) -> anyhow::Result<IpAddr> {
    let address: IpAddr = value
        .parse()
        .map_err(|_| anyhow::anyhow!("package net:tcp-listen address must be an IP literal"))?;
    let allowed = match address {
        IpAddr::V4(ip) => ip == Ipv4Addr::UNSPECIFIED || ip.is_loopback(),
        IpAddr::V6(ip) => ip == Ipv6Addr::UNSPECIFIED || ip.is_loopback(),
    };
    if !allowed {
        bail!("package net:tcp-listen address must be wildcard or loopback; arbitrary interface binds are not allowed");
    }
    Ok(address)
}

fn validate_handle(handle: &str, prefix: &str) -> anyhow::Result<()> {
    let expected = format!("{prefix}:");
    let Some(value) = handle.strip_prefix(&expected) else {
        bail!("invalid package {prefix} handle");
    };
    if value.len() != HANDLE_RANDOM_BYTES * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid package {prefix} handle");
    }
    Ok(())
}

fn random_handle(prefix: &str) -> String {
    let mut random = [0u8; HANDLE_RANDOM_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut random);
    format!("{prefix}:{}", hex::encode(random))
}

fn accept_timeout(value: Option<u64>) -> anyhow::Result<Duration> {
    bounded_timeout(
        value,
        DEFAULT_ACCEPT_TIMEOUT_MS,
        MAX_ACCEPT_TIMEOUT_MS,
        "accept",
    )
}

fn io_timeout(value: Option<u64>) -> anyhow::Result<Duration> {
    bounded_timeout(value, DEFAULT_IO_TIMEOUT_MS, MAX_IO_TIMEOUT_MS, "IO")
}

fn bounded_timeout(
    value: Option<u64>,
    default_ms: u64,
    max_ms: u64,
    label: &str,
) -> anyhow::Result<Duration> {
    let millis = value.unwrap_or(default_ms);
    if millis == 0 {
        bail!("package net:tcp-listen {label} timeout_ms must be positive");
    }
    Ok(Duration::from_millis(millis.min(max_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_grant_is_bounded() {
        let grant = grant().unwrap();
        assert_eq!(grant.capability, CAPABILITY);
        assert_eq!(grant.target, CAPABILITY);
        assert_eq!(
            grant.operations,
            std::collections::BTreeSet::from([
                "accept".to_string(),
                "bind".to_string(),
                "close".to_string(),
                "close_connection".to_string(),
                "read".to_string(),
                "write".to_string(),
            ])
        );
    }

    #[test]
    fn listener_bind_rejects_specific_interfaces() {
        assert!(validate_bind_address("0.0.0.0").is_ok());
        assert!(validate_bind_address("::").is_ok());
        assert!(validate_bind_address("127.0.0.1").is_ok());
        assert!(validate_bind_address("::1").is_ok());
        assert!(validate_bind_address("192.0.2.1").is_err());
        assert!(validate_bind_address("example.com").is_err());
    }

    #[test]
    fn listener_handles_are_namespace_separated() {
        assert!(validate_handle(&format!("tcp-listen:{}", "a".repeat(48)), "tcp-listen").is_ok());
        assert!(validate_handle(&format!("tcp-in:{}", "b".repeat(48)), "tcp-in").is_ok());
        assert!(validate_handle(&format!("tcp:{}", "c".repeat(48)), "tcp-in").is_err());
    }
}
