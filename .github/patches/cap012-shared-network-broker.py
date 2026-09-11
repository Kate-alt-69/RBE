from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# Move public HTTP policy into core-lib so interpreter and trusted host bridge
# share one SSRF/headers/body/timeout implementation.
core_cargo = Path("engine/crates/core/Cargo.toml")
replace_once(
    core_cargo,
    '''anyhow = { workspace = true }
''',
    '''anyhow = { workspace = true }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }
''',
    "core reqwest dependency",
)

network = Path("engine/crates/core/src/network_broker.rs")
network.write_text(r'''//! Trusted public HTTP egress broker shared by REL interpreter and Container
//! host-capability dispatch. The broker owns network policy; callers provide
//! logical operations and JSON arguments, never sockets or resolver handles.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use serde_json::{Map, Value};

pub const PUBLIC_HTTP_TARGET: &str = "public-http";
pub const PUBLIC_HTTP_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const PUBLIC_HTTP_REQUEST_MAX_BYTES: usize = 1024 * 1024;
const PUBLIC_HTTP_DEFAULT_TIMEOUT_MS: u64 = 5_000;
const PUBLIC_HTTP_MAX_TIMEOUT_MS: u64 = 10_000;
const PUBLIC_HTTP_MAX_HEADERS: usize = 32;
const PUBLIC_HTTP_MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
const PUBLIC_HTTP_MAX_RESOLVED_ADDRESSES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicHttpError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for PublicHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicHttpError {}

fn http_error(message: impl Into<String>) -> PublicHttpError {
    PublicHttpError {
        code: "HTTP3000",
        message: message.into(),
    }
}

fn value_string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, PublicHttpError> {
    match value {
        Some(Value::String(value)) => Ok(value),
        _ => Err(http_error(format!("{label} must be a string"))),
    }
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

async fn resolve_public_destination(
    url: &reqwest::Url,
) -> Result<Option<SocketAddr>, PublicHttpError> {
    let host = url
        .host_str()
        .ok_or_else(|| http_error("HTTP URL must include a host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| http_error("HTTP URL has no usable port"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if forbidden_ip(ip) {
            return Err(http_error("HTTP destination is not publicly routable"));
        }
        return Ok(None);
    }

    let mut resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| http_error(format!("HTTP DNS resolution failed: {error}")))?
        .collect::<Vec<_>>();
    resolved.sort_unstable();
    resolved.dedup();
    if resolved.is_empty() {
        return Err(http_error("HTTP DNS resolution returned no addresses"));
    }
    if resolved.len() > PUBLIC_HTTP_MAX_RESOLVED_ADDRESSES {
        return Err(http_error(
            "HTTP DNS resolution returned too many addresses",
        ));
    }
    if resolved.iter().any(|address| forbidden_ip(address.ip())) {
        return Err(http_error("HTTP hostname resolves to a non-public address"));
    }
    Ok(resolved.into_iter().next())
}

fn outbound_header_allowed(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "upgrade"
            | "te"
            | "trailer"
    )
}

struct HttpCall {
    method: reqwest::Method,
    url: reqwest::Url,
    headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
    body: Option<Vec<u8>>,
    timeout: Duration,
}

fn parse_http_call(operation: &str, args: &[Value]) -> Result<HttpCall, PublicHttpError> {
    let (method, url_text, headers_value, body_value, timeout_ms) = match operation {
        "get" => (
            reqwest::Method::GET,
            value_string(args.first(), "HTTP URL")?.to_string(),
            None,
            None,
            PUBLIC_HTTP_DEFAULT_TIMEOUT_MS,
        ),
        "post" => (
            reqwest::Method::POST,
            value_string(args.first(), "HTTP URL")?.to_string(),
            None,
            args.get(1),
            PUBLIC_HTTP_DEFAULT_TIMEOUT_MS,
        ),
        "request" => {
            let Some(Value::Object(options)) = args.first() else {
                return Err(http_error("http.request(options) requires an object"));
            };
            let method = options
                .get("method")
                .map(|value| value_string(Some(value), "HTTP method"))
                .transpose()?
                .unwrap_or("GET");
            let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .map_err(|_| http_error("HTTP method is invalid"))?;
            if !matches!(
                method,
                reqwest::Method::GET
                    | reqwest::Method::POST
                    | reqwest::Method::PUT
                    | reqwest::Method::PATCH
                    | reqwest::Method::DELETE
                    | reqwest::Method::HEAD
            ) {
                return Err(http_error(
                    "HTTP method is not permitted by the REL capability",
                ));
            }
            let url = value_string(options.get("url"), "HTTP URL")?.to_string();
            let timeout_ms = match options.get("timeoutMs") {
                None => PUBLIC_HTTP_DEFAULT_TIMEOUT_MS,
                Some(Value::Number(value))
                    if value.as_u64().is_some_and(|value| value >= 1) =>
                {
                    value.as_u64().unwrap_or(PUBLIC_HTTP_DEFAULT_TIMEOUT_MS)
                        .min(PUBLIC_HTTP_MAX_TIMEOUT_MS)
                }
                Some(_) => return Err(http_error("HTTP timeoutMs must be a positive integer")),
            };
            (
                method,
                url,
                options.get("headers"),
                options.get("body"),
                timeout_ms,
            )
        }
        other => return Err(http_error(format!("http.{other}() does not exist"))),
    };

    let url = reqwest::Url::parse(&url_text)
        .map_err(|error| http_error(format!("HTTP URL is invalid: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(http_error("HTTP URL scheme must be http or https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(http_error("HTTP URL credentials are forbidden"));
    }
    if url.fragment().is_some() {
        return Err(http_error("HTTP URL fragments are forbidden"));
    }

    let mut headers = Vec::new();
    if let Some(headers_value) = headers_value {
        let Value::Object(values) = headers_value else {
            return Err(http_error("HTTP headers must be an object"));
        };
        if values.len() > PUBLIC_HTTP_MAX_HEADERS {
            return Err(http_error("HTTP request has too many headers"));
        }
        for (name, value) in values {
            let Value::String(value) = value else {
                return Err(http_error(format!("HTTP header {name:?} must be a string")));
            };
            if !outbound_header_allowed(name) {
                return Err(http_error(format!(
                    "HTTP header {name:?} is controlled by RBE"
                )));
            }
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| http_error(format!("HTTP header name is invalid: {error}")))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| http_error(format!("HTTP header value is invalid: {error}")))?;
            headers.push((name, value));
        }
    }

    let body = match body_value {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_bytes().to_vec()),
        Some(value) => Some(
            serde_json::to_vec(value)
                .map_err(|error| http_error(format!("encode HTTP request body: {error}")))?,
        ),
    };
    if body
        .as_ref()
        .is_some_and(|body| body.len() > PUBLIC_HTTP_REQUEST_MAX_BYTES)
    {
        return Err(http_error("HTTP request body exceeds 1 MiB"));
    }

    Ok(HttpCall {
        method,
        url,
        headers,
        body,
        timeout: Duration::from_millis(timeout_ms.min(PUBLIC_HTTP_MAX_TIMEOUT_MS)),
    })
}

pub async fn call_public_http(
    operation: &str,
    args: &[Value],
) -> Result<Value, PublicHttpError> {
    let call = parse_http_call(operation, args)?;
    let pinned = resolve_public_destination(&call.url).await?;
    let host = call.url.host_str().map(str::to_string);

    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(call.timeout.min(Duration::from_secs(3)))
        .timeout(call.timeout);
    if let (Some(host), Some(address)) = (host.as_deref(), pinned) {
        builder = builder.resolve(host, address);
    }
    let client = builder
        .build()
        .map_err(|error| http_error(format!("initialize HTTP client: {error}")))?;
    let mut request = client.request(call.method, call.url);
    for (name, value) in call.headers {
        request = request.header(name, value);
    }
    if let Some(body) = call.body {
        request = request.body(body);
    }

    let mut response = request
        .send()
        .await
        .map_err(|error| http_error(format!("HTTP request failed: {error}")))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut response_headers = Map::new();
    for (name, value) in response.headers().iter().take(PUBLIC_HTTP_MAX_HEADERS) {
        if let Ok(value) = value.to_str() {
            if value.len() <= PUBLIC_HTTP_MAX_HEADER_VALUE_BYTES {
                response_headers.insert(name.as_str().to_string(), Value::String(value.to_string()));
            }
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| http_error(format!("read HTTP response: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > PUBLIC_HTTP_RESPONSE_MAX_BYTES {
            return Err(http_error("HTTP response exceeds 2 MiB"));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Value::Object(Map::from_iter([
        ("status".into(), Value::Number(status.as_u16().into())),
        ("ok".into(), Value::Bool(status.is_success())),
        ("headers".into(), Value::Object(response_headers)),
        (
            "body".into(),
            Value::String(String::from_utf8_lossy(&body).into_owned()),
        ),
        (
            "contentType".into(),
            content_type.map(Value::String).unwrap_or(Value::Null),
        ),
    ])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_and_special_network_destinations() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(forbidden_ip(ip.parse().unwrap()), "{ip} must be blocked");
        }
        assert!(!forbidden_ip("1.1.1.1".parse().unwrap()));
        assert!(!forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn parser_rejects_credentials_fragments_and_controlled_headers() {
        assert!(parse_http_call(
            "get",
            &[Value::String("https://user:pass@example.com/".into())]
        )
        .is_err());
        assert!(parse_http_call(
            "get",
            &[Value::String("https://example.com/path#fragment".into())]
        )
        .is_err());
        let request = serde_json::json!({
            "url": "https://example.com/",
            "headers": {"Host": "evil.example"}
        });
        assert!(parse_http_call("request", &[request]).is_err());
    }

    #[test]
    fn logical_target_is_fixed_not_a_socket_or_host() {
        assert_eq!(PUBLIC_HTTP_TARGET, "public-http");
        assert!(!PUBLIC_HTTP_TARGET.contains('/'));
        assert!(!PUBLIC_HTTP_TARGET.contains(':'));
    }
}
''', encoding="utf-8")

core_lib = Path("engine/crates/core/src/lib.rs")
replace_once(
    core_lib,
    '''mod metrics;
mod video_language;''',
    '''mod metrics;
mod network_broker;
mod video_language;''',
    "core network module",
)
replace_once(
    core_lib,
    '''pub use metrics::{
    BackendMetrics, BackendMetricsSnapshot, MaintenanceMetrics, MaintenanceSnapshot,
};''',
    '''pub use metrics::{
    BackendMetrics, BackendMetricsSnapshot, MaintenanceMetrics, MaintenanceSnapshot,
};
pub use network_broker::{
    call_public_http, PublicHttpError, PUBLIC_HTTP_REQUEST_MAX_BYTES, PUBLIC_HTTP_RESPONSE_MAX_BYTES,
    PUBLIC_HTTP_TARGET,
};''',
    "core network exports",
)

# Interpreter now delegates HTTP to the exact same trusted broker.
route_cargo = Path("engine/crates/route-engine/Cargo.toml")
replace_once(
    route_cargo,
    '''reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }
''',
    '''''',
    "remove route-engine reqwest dependency",
)

video = Path("engine/crates/route-engine/src/video_host.rs")
replace_once(
    video,
    '''use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use core_lib::{AppState, VideoLanguage};''',
    '''use std::collections::HashMap;
use std::sync::Arc;

use core_lib::{call_public_http, AppState, VideoLanguage};''',
    "interpreter network imports",
)
replace_once(
    video,
    '''            if module == "http" {
                return call_http(function, args).await.map(Some);
            }''',
    '''            if module == "http" {
                let args = args.into_iter().map(value_to_json).collect::<Vec<_>>();
                let value = call_public_http(function, &args)
                    .await
                    .map_err(|error| ModuleEvalError {
                        code: error.code,
                        message: error.message,
                    })?;
                return value_from_json(value).map(Some);
            }''',
    "interpreter uses shared HTTP broker",
)
# Remove the old route-engine HTTP implementation as one contiguous block.
text = video.read_text(encoding="utf-8")
start = text.find("const HTTP_RESPONSE_MAX_BYTES")
end = text.find("fn value_to_json", start)
if start < 0 or end < 0:
    raise SystemExit("route-engine HTTP policy block anchors changed")
text = text[:start] + text[end:]
video.write_text(text, encoding="utf-8")

# Backend trusted adapter gains only the fixed logical Network/public-http
# target. Controller authorization still occurs before this adapter is called.
host = Path("engine/crates/backend/src/host_capability.rs")
replace_once(
    host,
    '''use ipc_protocol::{
    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    '''use core_lib::{call_public_http, PUBLIC_HTTP_TARGET};
use ipc_protocol::{
    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    "Backend network broker import",
)
replace_once(
    host,
    '''    if request.kind != CapabilityKind::Service {
        return error(
            "CAPABILITY_KIND_UNSUPPORTED",
            "this trusted host adapter only supports Service capabilities",
        );
    }
    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return error(
            "CAPABILITY_REQUEST_TOO_LARGE",
            "service capability request exceeded protocol limit",
        );
    }
    let Some(service_name) = normalize_service_target(&request.target) else {''',
    '''    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
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
    let Some(service_name) = normalize_service_target(&request.target) else {''',
    "Backend Network dispatch",
)
replace_once(
    host,
    '''    #[test]
    fn host_token_comparison_is_exact() {''',
    '''    #[test]
    fn network_target_is_fixed_logical_public_http() {
        assert_eq!(PUBLIC_HTTP_TARGET, "public-http");
        assert_ne!(PUBLIC_HTTP_TARGET, "127.0.0.1:80");
        assert_ne!(PUBLIC_HTTP_TARGET, "example.com:443");
    }

    #[test]
    fn host_token_comparison_is_exact() {''',
    "Backend Network target test",
)

# Document the shared broker boundary.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''These compiler requirements are not themselves Controller grants; native lowering must still bind exact policy/limits before registration.''',
    '''These compiler requirements are not themselves Controller grants; native lowering must still bind exact policy/limits before registration. Public HTTP execution is centralized in `core_lib` as the `public-http` Network Broker: interpreter calls and authenticated Container host calls share the same public-address DNS pinning, redirect/proxy denial, controlled-header rules, request/response ceilings, and timeouts.''',
    "shared network broker documentation",
)
