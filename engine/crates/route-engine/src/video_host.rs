//! Async language bridge from `.module` execution into runtime-owned capabilities.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use core_lib::{AppState, VideoLanguage};

use crate::ast::Value;
use crate::module_eval::{HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError};
use crate::runtime_image::RuntimeImage;

pub struct RuntimeHostCapabilities {
    video: VideoLanguage,
    image: Arc<RuntimeImage>,
}

impl RuntimeHostCapabilities {
    pub fn from_state_and_image(state: &AppState, image: Arc<RuntimeImage>) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
        }
    }
}

impl HostCapabilityCaller for RuntimeHostCapabilities {
    fn call<'a>(
        &'a self,
        scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            if module == "ENV" {
                let value = self
                    .image
                    .environment
                    .call_rel(function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: "ENV3000",
                        message: error.to_string(),
                    })?;
                return Ok(Some(value));
            }
            if module == "http" {
                return call_http(function, args).await.map(Some);
            }
            if !matches!(module, "vm" | "video-manager") {
                return Ok(None);
            }
            let owner = scope.ok_or_else(|| ModuleEvalError {
                code: "VID3003",
                message: "Video Manager capability requires a resolved .module identity".into(),
            })?;
            let args = args.into_iter().map(value_to_json).collect::<Vec<_>>();
            let value =
                self.video
                    .call(&owner, function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: error.code,
                        message: error.message,
                    })?;
            Ok(Some(value_from_json(value)?))
        })
    }
}

const HTTP_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const HTTP_REQUEST_MAX_BYTES: usize = 1024 * 1024;
const HTTP_DEFAULT_TIMEOUT_MS: u64 = 5_000;
const HTTP_MAX_TIMEOUT_MS: u64 = 10_000;
const HTTP_MAX_HEADERS: usize = 32;

fn http_error(message: impl Into<String>) -> ModuleEvalError {
    ModuleEvalError {
        code: "HTTP3000",
        message: message.into(),
    }
}

fn value_string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, ModuleEvalError> {
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
        || (segments[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (segments[0] & 0xffc0) == 0xfe80 // link local fe80::/10
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
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
) -> Result<Option<SocketAddr>, ModuleEvalError> {
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
    if resolved.len() > 16 {
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

fn parse_http_call(function: &str, args: &[Value]) -> Result<HttpCall, ModuleEvalError> {
    let (method, url_text, headers_value, body_value, timeout_ms) = match function {
        "get" => (
            reqwest::Method::GET,
            value_string(args.first(), "HTTP URL")?.to_string(),
            None,
            None,
            HTTP_DEFAULT_TIMEOUT_MS,
        ),
        "post" => (
            reqwest::Method::POST,
            value_string(args.first(), "HTTP URL")?.to_string(),
            None,
            args.get(1),
            HTTP_DEFAULT_TIMEOUT_MS,
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
                None => HTTP_DEFAULT_TIMEOUT_MS,
                Some(Value::Number(value))
                    if value.is_finite() && value.fract() == 0.0 && *value >= 1.0 =>
                {
                    (*value as u64).min(HTTP_MAX_TIMEOUT_MS)
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
        if values.len() > HTTP_MAX_HEADERS {
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
            serde_json::to_vec(&value_to_json(value.clone()))
                .map_err(|error| http_error(format!("encode HTTP request body: {error}")))?,
        ),
    };
    if body
        .as_ref()
        .is_some_and(|body| body.len() > HTTP_REQUEST_MAX_BYTES)
    {
        return Err(http_error("HTTP request body exceeds 1 MiB"));
    }

    Ok(HttpCall {
        method,
        url,
        headers,
        body,
        timeout: Duration::from_millis(timeout_ms.min(HTTP_MAX_TIMEOUT_MS)),
    })
}

async fn call_http(function: &str, args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    let call = parse_http_call(function, &args)?;
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
    let mut response_headers = HashMap::new();
    for (name, value) in response.headers().iter().take(HTTP_MAX_HEADERS) {
        if let Ok(value) = value.to_str() {
            if value.len() <= 8 * 1024 {
                response_headers
                    .insert(name.as_str().to_string(), Value::String(value.to_string()));
            }
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| http_error(format!("read HTTP response: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > HTTP_RESPONSE_MAX_BYTES {
            return Err(http_error("HTTP response exceeds 2 MiB"));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Value::Object(HashMap::from([
        ("status".into(), Value::Number(f64::from(status.as_u16()))),
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

fn value_to_json(value: Value) -> serde_json::Value {
    match value {
        Value::String(value) => serde_json::Value::String(value),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(value) => serde_json::Value::Bool(value),
        Value::Null => serde_json::Value::Null,
        Value::Object(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, value_to_json(value)))
                .collect(),
        ),
        Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
    }
}

fn value_from_json(value: serde_json::Value) -> Result<Value, ModuleEvalError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Bool(value)),
        serde_json::Value::Number(value) => {
            value
                .as_f64()
                .map(Value::Number)
                .ok_or_else(|| ModuleEvalError {
                    code: "VID3002",
                    message: "Video Manager returned a number outside the RBE numeric range".into(),
                })
        }
        serde_json::Value::String(value) => Ok(Value::String(value)),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(value_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_json::Value::Object(fields) => {
            let mut out = std::collections::HashMap::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, value_from_json(value)?);
            }
            Ok(Value::Object(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_public_http_destinations() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(forbidden_ip(ip.parse().unwrap()), "{ip} must be rejected");
        }
        assert!(!forbidden_ip("1.1.1.1".parse().unwrap()));
        assert!(!forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn http_parser_rejects_credentials_fragments_and_controlled_headers() {
        assert!(parse_http_call("get", &[Value::String("http://u:p@example.com".into())]).is_err());
        assert!(parse_http_call("get", &[Value::String("https://example.com/#x".into())]).is_err());
        let options = Value::Object(HashMap::from([
            ("url".into(), Value::String("https://example.com/".into())),
            (
                "headers".into(),
                Value::Object(HashMap::from([(
                    "Host".into(),
                    Value::String("evil".into()),
                )])),
            ),
        ]));
        assert!(parse_http_call("request", &[options]).is_err());
    }
}
