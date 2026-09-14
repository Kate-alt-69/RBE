from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one BUG-IPC-LNX-001 anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


# Shared malformed-input classifier for newline-delimited Service IPC.  We do
# not preview JSON-like input because even malformed JSON may contain tokens or
# application data.  Only an already-classified HTTP-ish probe gets a capped
# printable preview.
path = "engine/crates/service-runtime/src/lib.rs"
replace_once(
    path,
    '''pub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);\npub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;\npub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;\n''',
    r'''pub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
const MALFORMED_IPC_PREVIEW_BYTES: usize = 64;

pub(crate) fn malformed_ipc_class(bytes: &[u8]) -> &'static str {
    if bytes.is_empty() || bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return "empty";
    }
    if bytes.starts_with(b"GET ")
        || bytes.starts_with(b"HEAD ")
        || bytes.starts_with(b"POST ")
        || bytes.starts_with(b"PUT ")
        || bytes.starts_with(b"PATCH ")
        || bytes.starts_with(b"DELETE ")
        || bytes.starts_with(b"OPTIONS ")
        || bytes.starts_with(b"CONNECT ")
        || bytes.starts_with(b"PRI *")
    {
        return "http-like";
    }
    if matches!(bytes.first().copied(), Some(b'{') | Some(b'[')) {
        return "json-like";
    }
    if bytes
        .iter()
        .take(16)
        .all(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace())
    {
        "text-like"
    } else {
        "binary"
    }
}

pub(crate) fn malformed_ipc_preview(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes.iter().take(MALFORMED_IPC_PREVIEW_BYTES) {
        match byte {
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push('.'),
        }
    }
    if bytes.len() > MALFORMED_IPC_PREVIEW_BYTES {
        out.push_str("...");
    }
    out
}
''',
)

replace_once(
    path,
    '''        let request: ServiceRequest = match serde_json::from_str(line.trim()) {\n            Ok(request) => request,\n            Err(error) => {\n                tracing::warn!(service = %file.name, error = %error, "invalid service IPC JSON");\n                continue;\n            }\n        };\n''',
    r'''        let request: ServiceRequest = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                let classification = malformed_ipc_class(line.as_bytes());
                tracing::warn!(
                    service = %file.name,
                    %peer,
                    classification,
                    bytes = line.len(),
                    error = %error,
                    "invalid service IPC JSON"
                );
                if classification == "http-like" {
                    let preview = malformed_ipc_preview(line.as_bytes());
                    tracing::debug!(
                        service = %file.name,
                        %peer,
                        %preview,
                        "malformed service IPC HTTP-like preview"
                    );
                }
                continue;
            }
        };
''',
)

replace_once(
    path,
    '''#[cfg(test)]\nmod tests {\n    #[test]\n    fn short_process_label_preserves_service_file_identity() {\n''',
    r'''#[cfg(test)]
mod tests {
    #[test]
    fn malformed_ipc_classifier_identifies_http_probes() {
        assert_eq!(super::malformed_ipc_class(b"GET / HTTP/1.1\r\n"), "http-like");
        assert_eq!(
            super::malformed_ipc_preview(b"GET / HTTP/1.1\r\n"),
            "GET / HTTP/1.1\\r\\n"
        );
    }

    #[test]
    fn malformed_ipc_classifier_distinguishes_json_and_binary() {
        assert_eq!(super::malformed_ipc_class(br#"{"token":"redacted"}"#), "json-like");
        assert_eq!(super::malformed_ipc_class(&[0, 1, 2, 3]), "binary");
        assert_eq!(super::malformed_ipc_class(b"\r\n"), "empty");
    }

    #[test]
    fn short_process_label_preserves_service_file_identity() {
''',
)

# Service Mother: associate malformed input with its peer and classify it before
# serde rejects it.  HTTP-like text gets a debug-only capped preview.  Returning
# Ok for malformed unauthenticated input avoids the second generic warning.
path = "engine/crates/service-runtime/src/mother.rs"
replace_once(
    path,
    '''                        if let Err(error) = handle_connection(stream, manager, token, shutdown_tx).await {\n                            tracing::warn!(error = %error, "Service Mother request failed");\n                        }\n''',
    r'''                        if let Err(error) =
                            handle_connection(stream, peer, manager, token, shutdown_tx).await
                        {
                            tracing::warn!(%peer, error = %error, "Service Mother request failed");
                        }
''',
)
replace_once(
    path,
    '''async fn handle_connection(\n    stream: TcpStream,\n    manager: ServiceManager,\n''',
    '''async fn handle_connection(\n    stream: TcpStream,\n    peer: SocketAddr,\n    manager: ServiceManager,\n''',
)
replace_once(
    path,
    '''    let request: ServiceMotherRequest = serde_json::from_str(line.trim())?;\n''',
    r'''    let request: ServiceMotherRequest = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(error) => {
            let classification = crate::malformed_ipc_class(line.as_bytes());
            tracing::warn!(
                %peer,
                classification,
                bytes = line.len(),
                error = %error,
                "rejected malformed Service Mother IPC"
            );
            if classification == "http-like" {
                let preview = crate::malformed_ipc_preview(line.as_bytes());
                tracing::debug!(%peer, %preview, "malformed Service Mother HTTP-like preview");
            }
            return Ok(());
        }
    };
''',
)

# Host Capability uses a length-prefixed protocol.  Four bytes are protocol
# metadata rather than a body, so logging them on an invalid length cannot dump
# a valid capability payload or authentication token.
path = "engine/crates/backend/src/host_capability.rs"
replace_once(
    path,
    '''                        handle_connection(stream, expected_token, services, video),\n''',
    '''                        handle_connection(stream, peer, expected_token, services, video),\n''',
)
replace_once(
    path,
    '''                        Ok(Err(error)) => {\n                            tracing::warn!(error = %error, "host capability bridge call failed")\n                        }\n                        Err(_) => tracing::warn!("host capability bridge call timed out"),\n''',
    r'''                        Ok(Err(error)) => {
                            if error.to_string() == "host capability frame length is invalid" {
                                tracing::debug!(
                                    %peer,
                                    error = %error,
                                    "host capability bridge rejected malformed call"
                                );
                            } else {
                                tracing::warn!(
                                    %peer,
                                    error = %error,
                                    "host capability bridge call failed"
                                );
                            }
                        }
                        Err(_) => tracing::warn!(%peer, "host capability bridge call timed out"),
''',
)
replace_once(
    path,
    '''async fn handle_connection(\n    mut stream: TcpStream,\n    expected_token: String,\n''',
    '''async fn handle_connection(\n    mut stream: TcpStream,\n    peer: SocketAddr,\n    expected_token: String,\n''',
)
replace_once(
    path,
    '''    let request: HostCapabilityRequest = read_typed_frame(&mut stream).await?;\n''',
    '''    let request: HostCapabilityRequest = read_typed_frame(&mut stream, peer).await?;\n''',
)
replace_once(
    path,
    '''async fn read_typed_frame<T: DeserializeOwned>(stream: &mut TcpStream) -> anyhow::Result<T> {\n    let mut len = [0u8; 4];\n    stream.read_exact(&mut len).await?;\n    let length = u32::from_be_bytes(len) as usize;\n    if length == 0 || length > MAX_HOST_CAPABILITY_FRAME_BYTES {\n        anyhow::bail!("host capability frame length is invalid");\n    }\n''',
    r'''fn malformed_host_prefix_class(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"GET ")
        || bytes.starts_with(b"HEAD")
        || bytes.starts_with(b"POST")
        || bytes.starts_with(b"PUT ")
        || bytes.starts_with(b"PATC")
        || bytes.starts_with(b"DELE")
        || bytes.starts_with(b"OPTI")
        || bytes.starts_with(b"CONN")
        || bytes.starts_with(b"PRI ")
    {
        "http-like"
    } else if bytes.iter().all(|byte| *byte == 0) {
        "zero-length-prefix"
    } else if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace())
    {
        "text-like"
    } else {
        "binary"
    }
}

fn malformed_host_prefix_ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '.'
            }
        })
        .collect()
}

async fn read_typed_frame<T: DeserializeOwned>(
    stream: &mut TcpStream,
    peer: SocketAddr,
) -> anyhow::Result<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let length = u32::from_be_bytes(len) as usize;
    if length == 0 || length > MAX_HOST_CAPABILITY_FRAME_BYTES {
        let classification = malformed_host_prefix_class(&len);
        let first4_ascii = malformed_host_prefix_ascii(&len);
        let first4_hex = format!(
            "{:02x} {:02x} {:02x} {:02x}",
            len[0], len[1], len[2], len[3]
        );
        tracing::warn!(
            %peer,
            classification,
            decoded_length = length,
            max_frame_bytes = MAX_HOST_CAPABILITY_FRAME_BYTES,
            %first4_hex,
            %first4_ascii,
            "rejected malformed Host Capability frame"
        );
        anyhow::bail!("host capability frame length is invalid");
    }
''',
)
