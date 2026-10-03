use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context};
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibrarySessionBinding, MAX_LIBRARY_PAYLOAD_BYTES,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

pub const CAPABILITY: &str = "net:tls";
const REQUIRED_TCP_CAPABILITY: &str = "net:tcp";
const HANDLE_RANDOM_BYTES: usize = 24;
const MAX_CONNECTIONS_GLOBAL: usize = 128;
const MAX_CONNECTIONS_PER_SESSION: usize = 8;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug)]
enum ManagedTransport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

#[derive(Clone)]
struct ManagedTlsConnection {
    owner: String,
    package: String,
    host: String,
    stream: Arc<AsyncMutex<Option<ManagedTransport>>>,
}

static CONNECTIONS: OnceLock<Mutex<BTreeMap<String, ManagedTlsConnection>>> = OnceLock::new();
static TLS_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

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
struct StartTlsRequest {
    handle: String,
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
            "start_tls".to_string(),
            "close".to_string(),
        ],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package TLS/STARTTLS capability grant")
}

pub async fn dispatch_authorized_call(
    package: &str,
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<Vec<u8>> {
    if call.capability != CAPABILITY || call.target != CAPABILITY {
        bail!("package TLS call does not match admitted net:tls authority");
    }
    let accepted = binding.accepted_info().ok_or_else(|| {
        anyhow::anyhow!("package TLS call arrived before Library Host acceptance")
    })?;
    if !accepted
        .granted_capabilities
        .contains(REQUIRED_TCP_CAPABILITY)
    {
        bail!(
            "package net:tls requires an accepted net:tcp grant in the same Library Host session"
        );
    }
    if !accepted.granted_capabilities.contains(CAPABILITY) {
        bail!("package net:tls is not granted to the accepted Library Host session");
    }
    let owner = accepted.capability_identity.clone();

    match call.operation.as_str() {
        "connect" => connect(package, &owner, &call.payload).await,
        "write" => write(package, &owner, &call.payload).await,
        "read" => read(package, &owner, &call.payload).await,
        "start_tls" => start_tls(package, &owner, &call.payload).await,
        "close" => close(package, &owner, &call.payload).await,
        other => bail!("unsupported package TLS operation {other:?}"),
    }
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
        serde_json::from_slice(payload).context("decode package net:tls connect request")?;
    if request.port == 0 {
        bail!("package net:tls port must be in 1..=65535");
    }
    validate_tls_host(&request.host)?;
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
            "package net:tls could not connect to public destination {}:{}: {}",
            request.host,
            request.port,
            last_error.unwrap_or_else(|| "no usable public address".to_string())
        )
    })?;
    stream
        .set_nodelay(true)
        .context("configure package TLS transport TCP connection")?;

    let handle = insert_connection(package, owner, &request.host, stream)?;
    serde_json::to_vec(&json!({
        "handle": handle,
        "peer": peer.to_string(),
        "tls": false,
    }))
    .context("encode package net:tls connect response")
}

async fn write(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: WriteRequest =
        serde_json::from_slice(payload).context("decode package net:tls write request")?;
    if request.data.is_empty() || request.data.len() > MAX_WRITE_BYTES {
        bail!(
            "package net:tls write data must contain 1..={} bytes",
            MAX_WRITE_BYTES
        );
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = request_timeout(request.timeout_ms)?;
    let mut slot = connection.stream.lock().await;
    let stream = slot
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("package TLS transport is unavailable during transition"))?;
    let future = async {
        match stream {
            ManagedTransport::Plain(stream) => stream.write_all(&request.data).await,
            ManagedTransport::Tls(stream) => stream.write_all(&request.data).await,
        }
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| anyhow::anyhow!("package net:tls write timed out"))?
        .context("write package TLS transport data")?;

    serde_json::to_vec(&json!({ "written": request.data.len() }))
        .context("encode package net:tls write response")
}

async fn read(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: ReadRequest =
        serde_json::from_slice(payload).context("decode package net:tls read request")?;
    if request.max_bytes == 0 || request.max_bytes > MAX_READ_BYTES {
        bail!(
            "package net:tls max_bytes must be in 1..={}",
            MAX_READ_BYTES
        );
    }
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = request_timeout(request.timeout_ms)?;
    let mut slot = connection.stream.lock().await;
    let stream = slot
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("package TLS transport is unavailable during transition"))?;
    let encrypted = matches!(stream, ManagedTransport::Tls(_));
    let mut data = vec![0u8; request.max_bytes];
    let future = async {
        match stream {
            ManagedTransport::Plain(stream) => stream.read(&mut data).await,
            ManagedTransport::Tls(stream) => stream.read(&mut data).await,
        }
    };
    let read = tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| anyhow::anyhow!("package net:tls read timed out"))?
        .context("read package TLS transport data")?;
    data.truncate(read);

    serde_json::to_vec(&json!({
        "data": data,
        "eof": read == 0,
        "tls": encrypted,
    }))
    .context("encode package net:tls read response")
}

async fn start_tls(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: StartTlsRequest =
        serde_json::from_slice(payload).context("decode package net:tls STARTTLS request")?;
    let connection = connection_for(package, owner, &request.handle)?;
    let timeout = request_timeout(request.timeout_ms)?;
    let server_name = connection.host.clone();

    let mut slot = connection.stream.lock().await;
    match slot.as_ref() {
        Some(ManagedTransport::Tls(_)) => {
            bail!("package net:tls connection is already encrypted")
        }
        Some(ManagedTransport::Plain(_)) => {}
        None => bail!("package TLS transport is unavailable during transition"),
    }

    let plain = match slot.take() {
        Some(ManagedTransport::Plain(stream)) => stream,
        Some(ManagedTransport::Tls(stream)) => {
            *slot = Some(ManagedTransport::Tls(stream));
            bail!("package net:tls connection is already encrypted");
        }
        None => bail!("package TLS transport is unavailable during transition"),
    };

    let name = match ServerName::try_from(server_name.clone()) {
        Ok(name) => name,
        Err(error) => {
            drop(slot);
            let _ = remove_connection(package, owner, &request.handle);
            return Err(anyhow::anyhow!(
                "package net:tls server name {server_name:?} is invalid: {error}"
            ));
        }
    };
    let connector = TlsConnector::from(tls_config());
    let handshake = tokio::time::timeout(timeout, connector.connect(name, plain)).await;
    match handshake {
        Ok(Ok(stream)) => {
            *slot = Some(ManagedTransport::Tls(Box::new(stream)));
            serde_json::to_vec(&json!({
                "handle": request.handle,
                "tls": true,
                "server_name": server_name,
            }))
            .context("encode package net:tls STARTTLS response")
        }
        Ok(Err(error)) => {
            drop(slot);
            let _ = remove_connection(package, owner, &request.handle);
            bail!(
                "package net:tls handshake failed for {server_name:?}; connection was destroyed to prevent plaintext fallback: {error}"
            )
        }
        Err(_) => {
            drop(slot);
            let _ = remove_connection(package, owner, &request.handle);
            bail!(
                "package net:tls handshake timed out for {server_name:?}; connection was destroyed to prevent plaintext fallback"
            )
        }
    }
}

async fn close(package: &str, owner: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: CloseRequest =
        serde_json::from_slice(payload).context("decode package net:tls close request")?;
    let connection = remove_connection(package, owner, &request.handle)?;
    let mut slot = connection.stream.lock().await;
    if let Some(mut stream) = slot.take() {
        match &mut stream {
            ManagedTransport::Plain(stream) => {
                let _ = stream.shutdown().await;
            }
            ManagedTransport::Tls(stream) => {
                let _ = stream.shutdown().await;
            }
        }
    }
    Ok(Vec::new())
}

fn tls_config() -> Arc<ClientConfig> {
    TLS_CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

fn registry() -> &'static Mutex<BTreeMap<String, ManagedTlsConnection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock_registry(
) -> anyhow::Result<std::sync::MutexGuard<'static, BTreeMap<String, ManagedTlsConnection>>> {
    registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("package TLS connection registry lock is poisoned"))
}

fn ensure_connection_capacity(owner: &str) -> anyhow::Result<()> {
    let connections = lock_registry()?;
    if connections.len() >= MAX_CONNECTIONS_GLOBAL {
        bail!(
            "package TLS connection registry reached global limit {}",
            MAX_CONNECTIONS_GLOBAL
        );
    }
    let owned = connections
        .values()
        .filter(|connection| connection.owner == owner)
        .count();
    if owned >= MAX_CONNECTIONS_PER_SESSION {
        bail!(
            "package TLS session reached connection limit {}",
            MAX_CONNECTIONS_PER_SESSION
        );
    }
    Ok(())
}

fn insert_connection(
    package: &str,
    owner: &str,
    host: &str,
    stream: TcpStream,
) -> anyhow::Result<String> {
    let mut connections = lock_registry()?;
    if connections.len() >= MAX_CONNECTIONS_GLOBAL {
        bail!("package TLS connection registry became full before insertion");
    }
    if connections
        .values()
        .filter(|connection| connection.owner == owner)
        .count()
        >= MAX_CONNECTIONS_PER_SESSION
    {
        bail!("package TLS session became full before insertion");
    }

    for _ in 0..8 {
        let mut random = [0u8; HANDLE_RANDOM_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let handle = format!("tls:{}", hex::encode(random));
        if !connections.contains_key(&handle) {
            connections.insert(
                handle.clone(),
                ManagedTlsConnection {
                    owner: owner.to_string(),
                    package: package.to_string(),
                    host: host.to_string(),
                    stream: Arc::new(AsyncMutex::new(Some(ManagedTransport::Plain(stream)))),
                },
            );
            return Ok(handle);
        }
    }
    bail!("could not allocate unique package TLS handle")
}

fn connection_for(
    package: &str,
    owner: &str,
    handle: &str,
) -> anyhow::Result<ManagedTlsConnection> {
    validate_handle(handle)?;
    let connections = lock_registry()?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TLS handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package TLS handle is not owned by the accepted package session");
    }
    Ok(connection.clone())
}

fn remove_connection(
    package: &str,
    owner: &str,
    handle: &str,
) -> anyhow::Result<ManagedTlsConnection> {
    validate_handle(handle)?;
    let mut connections = lock_registry()?;
    let connection = connections
        .get(handle)
        .ok_or_else(|| anyhow::anyhow!("unknown package TLS handle"))?;
    if connection.owner != owner || connection.package != package {
        bail!("package TLS handle is not owned by the accepted package session");
    }
    connections
        .remove(handle)
        .ok_or_else(|| anyhow::anyhow!("package TLS handle disappeared during close"))
}

fn validate_handle(handle: &str) -> anyhow::Result<()> {
    let Some(value) = handle.strip_prefix("tls:") else {
        bail!("invalid package TLS handle");
    };
    if value.len() != HANDLE_RANDOM_BYTES * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid package TLS handle");
    }
    Ok(())
}

fn request_timeout(value: Option<u64>) -> anyhow::Result<Duration> {
    let millis = value.unwrap_or(DEFAULT_TIMEOUT_MS);
    if millis == 0 {
        bail!("package net:tls timeout_ms must be positive");
    }
    Ok(Duration::from_millis(millis.min(MAX_TIMEOUT_MS)))
}

async fn resolve_public_addresses(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve package TLS destination {host:?}"))?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("package net:tls DNS resolution returned no addresses");
    }
    if addresses.len() > MAX_RESOLVED_ADDRESSES {
        bail!(
            "package net:tls DNS resolution exceeded {} addresses",
            MAX_RESOLVED_ADDRESSES
        );
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        bail!("package net:tls hostname resolves to a non-public address");
    }
    Ok(addresses)
}

fn validate_tls_host(host: &str) -> anyhow::Result<()> {
    if host.is_empty()
        || host.len() > 253
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("package net:tls host is invalid");
    }
    if host.parse::<IpAddr>().is_ok() {
        bail!("package net:tls requires a public DNS hostname for certificate verification");
    }
    let host = host.trim_end_matches('.');
    if host.is_empty()
        || !host.contains('.')
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".local")
    {
        bail!("package net:tls host must be a public DNS name");
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
            bail!("package net:tls host contains an invalid DNS label");
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
    fn tls_grant_is_bounded_and_requires_explicit_upgrade() {
        let grant = grant().unwrap();
        assert_eq!(grant.capability, CAPABILITY);
        assert_eq!(grant.target, CAPABILITY);
        assert_eq!(
            grant.operations,
            std::collections::BTreeSet::from([
                "close".to_string(),
                "connect".to_string(),
                "read".to_string(),
                "start_tls".to_string(),
                "write".to_string(),
            ])
        );
    }

    #[test]
    fn tls_requires_dns_identity_and_rejects_local_names() {
        for host in ["", "127.0.0.1", "::1", "localhost", "mail.local", "printer"] {
            assert!(
                validate_tls_host(host).is_err(),
                "{host:?} must be rejected"
            );
        }
        assert!(validate_tls_host("smtp.example.com").is_ok());
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
}
