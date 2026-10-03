use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use core_lib::{resolve_public_https_target, ResolvedPublicHttpsTarget};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

static TLS_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

pub async fn upload_file(
    url: &str,
    source: &Path,
    expected_size: u64,
    content_type: &str,
    timeout: Duration,
) -> Result<()> {
    if expected_size == 0 {
        anyhow::bail!("deployment source artifact must not be empty");
    }
    validate_content_type(content_type)?;
    let metadata = tokio::fs::metadata(source)
        .await
        .with_context(|| format!("stat deployment source artifact {}", source.display()))?;
    if !metadata.is_file() || metadata.len() != expected_size {
        anyhow::bail!("deployment source artifact size changed before upload");
    }

    let target = resolve_public_https_target(url)
        .await
        .context("resolve source artifact upload target")?;
    let mut stream = connect(&target).await?;
    let mut file = tokio::fs::File::open(source)
        .await
        .with_context(|| format!("open deployment source artifact {}", source.display()))?;

    let operation = async {
        let request = format!(
            "PUT {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nUser-Agent: rbe-deployment-worker/1\r\n\r\n",
            target.request_target, target.host_header, content_type, expected_size
        );
        stream.write_all(request.as_bytes()).await?;

        let mut observed = 0_u64;
        let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .context("source artifact upload size overflow")?;
            if observed > expected_size {
                anyhow::bail!("source artifact grew during upload");
            }
            stream.write_all(&buffer[..read]).await?;
        }
        if observed != expected_size {
            anyhow::bail!(
                "source artifact upload size mismatch: expected {expected_size}, observed {observed}"
            );
        }
        stream.flush().await?;

        let head = read_response_head(&mut stream).await?;
        if !(200..300).contains(&head.status) {
            anyhow::bail!("source artifact PUT returned HTTP {}", head.status);
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::time::timeout(timeout, operation)
        .await
        .context("source artifact upload timed out")??;
    Ok(())
}

pub async fn download_file(
    url: &str,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
    timeout: Duration,
) -> Result<()> {
    if expected_size == 0 || !valid_sha256(expected_sha256) {
        anyhow::bail!("invalid deployment source artifact download identity");
    }
    if destination.exists() {
        anyhow::bail!("deployment source artifact destination must be fresh");
    }
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let target = resolve_public_https_target(url)
        .await
        .context("resolve source artifact download target")?;
    let mut stream = connect(&target).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .await
        .with_context(|| format!("create source artifact {}", destination.display()))?;

    let operation = async {
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: rbe-deployment-worker/1\r\n\r\n",
            target.request_target, target.host_header
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let head = read_response_head(&mut stream).await?;
        if head.status != 200 {
            anyhow::bail!("source artifact GET returned HTTP {}", head.status);
        }
        if head.transfer_encoding.is_some() {
            anyhow::bail!("source artifact GET must use a bounded Content-Length body");
        }
        if head.content_length != Some(expected_size) {
            anyhow::bail!("source artifact Content-Length does not match durable metadata");
        }

        let mut observed = 0_u64;
        let mut hasher = Sha256::new();
        if !head.leftover.is_empty() {
            observed = head.leftover.len() as u64;
            if observed > expected_size {
                anyhow::bail!("source artifact response exceeded expected size");
            }
            hasher.update(&head.leftover);
            file.write_all(&head.leftover).await?;
        }

        let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
        while observed < expected_size {
            let remaining = expected_size - observed;
            let read_limit = usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64))
                .context("source artifact remaining size does not fit usize")?;
            let read = stream.read(&mut buffer[..read_limit]).await?;
            if read == 0 {
                anyhow::bail!("source artifact response ended before Content-Length bytes arrived");
            }
            observed = observed
                .checked_add(read as u64)
                .context("source artifact download size overflow")?;
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read]).await?;
        }
        if observed != expected_size {
            anyhow::bail!("source artifact download size mismatch");
        }
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            anyhow::bail!("source artifact SHA-256 does not match durable metadata");
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok::<(), anyhow::Error>(())
    };

    let result = tokio::time::timeout(timeout, operation)
        .await
        .context("source artifact download timed out")?;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            drop(file);
            let _ = tokio::fs::remove_file(destination).await;
            Err(error)
        }
    }
}

async fn connect(target: &ResolvedPublicHttpsTarget) -> Result<TlsStream<TcpStream>> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target.address))
        .await
        .context("trusted artifact TCP connection timed out")??;
    tcp.set_nodelay(true)?;
    let server_name = ServerName::try_from(target.host.clone())
        .context("trusted artifact TLS server name is invalid")?;
    let connector = TlsConnector::from(tls_config());
    tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .context("trusted artifact TLS handshake timed out")?
        .context("trusted artifact TLS handshake failed")
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

struct ResponseHead {
    status: u16,
    content_length: Option<u64>,
    transfer_encoding: Option<String>,
    leftover: Vec<u8>,
}

async fn read_response_head(stream: &mut TlsStream<TcpStream>) -> Result<ResponseHead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            anyhow::bail!("artifact server closed before sending HTTP response headers");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_header_end(&bytes) {
            break index;
        }
        if bytes.len() > MAX_RESPONSE_HEADER_BYTES {
            anyhow::bail!("artifact server response headers exceed limit");
        }
    };
    if header_end > MAX_RESPONSE_HEADER_BYTES {
        anyhow::bail!("artifact server response headers exceed limit");
    }
    let leftover = bytes.split_off(header_end + 4);
    bytes.truncate(header_end);
    let text = std::str::from_utf8(&bytes).context("artifact server headers are not UTF-8/ASCII")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().context("artifact server response is empty")?;
    let mut status_parts = status_line.split_whitespace();
    let protocol = status_parts.next().unwrap_or_default();
    let status = status_parts
        .next()
        .context("artifact server response is missing status code")?
        .parse::<u16>()
        .context("artifact server status code is invalid")?;
    if !matches!(protocol, "HTTP/1.1" | "HTTP/1.0") {
        anyhow::bail!("artifact server returned unsupported HTTP protocol");
    }

    let mut content_length = None;
    let mut transfer_encoding = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .context("artifact server returned malformed HTTP header")?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            let parsed = value
                .parse::<u64>()
                .context("artifact server Content-Length is invalid")?;
            if content_length.is_some_and(|existing| existing != parsed) {
                anyhow::bail!("artifact server returned conflicting Content-Length headers");
            }
            content_length = Some(parsed);
        } else if name == "transfer-encoding" {
            if transfer_encoding.is_some() {
                anyhow::bail!("artifact server returned duplicate Transfer-Encoding headers");
            }
            transfer_encoding = Some(value.to_ascii_lowercase());
        }
    }

    Ok(ResponseHead {
        status,
        content_length,
        transfer_encoding,
        leftover,
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn validate_content_type(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.contains('\r')
        || value.contains('\n')
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("source artifact content type is invalid");
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
