from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing anchor: {label}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Route-engine dependencies for async outbound HTTP and constant-time helpers.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/Cargo.toml"
text = read(path)
text = replace_once(
    text,
    'anyhow = { workspace = true }\n',
    'anyhow = { workspace = true }\n'
    'tokio = { workspace = true }\n'
    'reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }\n'
    'sha2 = "0.10"\n'
    'subtle = "2"\n',
    "route-engine dependencies",
)
write(path, text)


# ---------------------------------------------------------------------------
# Built-in request/security surfaces + advertise the async HTTP capability.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/modules.rs"
text = read(path)
text = replace_once(
    text,
    '        "crypto" => matches!(function, "hash"),\n        "env" => matches!(function, "get"),\n',
    '        "crypto" => matches!(function, "hash"),\n'
    '        "http" => matches!(function, "request" | "get" | "post"),\n'
    '        "request" => matches!(\n'
    '            function,\n'
    '            "header"\n'
    '                | "hasHeader"\n'
    '                | "has_header"\n'
    '                | "query"\n'
    '                | "param"\n'
    '                | "cookie"\n'
    '                | "method"\n'
    '                | "path"\n'
    '                | "body"\n'
    '        ),\n'
    '        "security" => matches!(\n'
    '            function,\n'
    '            "constantTimeEqual"\n'
    '                | "constant_time_equal"\n'
    '                | "isSecure"\n'
    '                | "is_secure"\n'
    '                | "clientIp"\n'
    '                | "client_ip"\n'
    '        ),\n'
    '        "env" => matches!(function, "get"),\n',
    "builtin function surface",
)
text = replace_once(
    text,
    '            ModuleKind::Builtin(BuiltinModule::Http) => Err(ModuleError {\n'
    '                message: format!("{module_name}.{function_name}() is not implemented yet"),\n'
    '            }),\n'
    '            ModuleKind::Builtin(BuiltinModule::Request) => Err(ModuleError {\n'
    '                message: format!("{module_name}.{function_name}() is not implemented yet"),\n'
    '            }),\n'
    '            ModuleKind::Builtin(BuiltinModule::Security) => Err(ModuleError {\n'
    '                message: format!("{module_name}.{function_name}() is not implemented yet"),\n'
    '            }),\n',
    '            ModuleKind::Builtin(BuiltinModule::Http) => Err(ModuleError {\n'
    '                message: format!(\n'
    '                    "{module_name}.{function_name}() requires the async runtime HTTP host capability"\n'
    '                ),\n'
    '            }),\n'
    '            ModuleKind::Builtin(BuiltinModule::Request) => call_request(function_name, args),\n'
    '            ModuleKind::Builtin(BuiltinModule::Security) => call_security(function_name, args),\n',
    "builtin dispatch",
)

helper = r'''
fn request_object<'a>(value: Option<&'a Value>) -> Result<&'a HashMap<String, Value>, ModuleError> {
    match value {
        Some(Value::Object(fields)) => Ok(fields),
        _ => Err(ModuleError {
            message: "request helper requires the request object as its first argument".into(),
        }),
    }
}

fn request_named_value(
    request: &HashMap<String, Value>,
    bucket: &str,
    name: &str,
    case_insensitive: bool,
) -> Value {
    let Some(Value::Object(values)) = request.get(bucket) else {
        return Value::Null;
    };
    if !case_insensitive {
        return values.get(name).cloned().unwrap_or(Value::Null);
    }
    values
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .unwrap_or(Value::Null)
}

fn request_name(value: Option<&Value>, label: &str) -> Result<&str, ModuleError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(ModuleError {
            message: format!("{label} must be a non-empty string"),
        }),
    }
}

fn call_request(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let request = request_object(args.first())?;
    match function_name {
        "header" => {
            let name = request_name(args.get(1), "request header name")?;
            Ok(request_named_value(request, "headers", name, true))
        }
        "hasHeader" | "has_header" => {
            let name = request_name(args.get(1), "request header name")?;
            Ok(Value::Bool(!matches!(
                request_named_value(request, "headers", name, true),
                Value::Null
            )))
        }
        "query" => {
            let name = request_name(args.get(1), "query name")?;
            Ok(request_named_value(request, "query", name, false))
        }
        "param" => {
            let name = request_name(args.get(1), "route parameter name")?;
            Ok(request_named_value(request, "params", name, false))
        }
        "cookie" => {
            let name = request_name(args.get(1), "cookie name")?;
            Ok(request_named_value(request, "cookies", name, false))
        }
        "method" | "path" | "body" => Ok(request
            .get(function_name)
            .cloned()
            .unwrap_or(Value::Null)),
        other => Err(ModuleError {
            message: format!("request.{other}() does not exist"),
        }),
    }
}

fn security_string(value: Option<&Value>, label: &str) -> Result<&str, ModuleError> {
    match value {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ModuleError {
            message: format!("{label} must be a string"),
        }),
    }
}

fn call_security(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "constantTimeEqual" | "constant_time_equal" => {
            use sha2::{Digest, Sha256};
            use subtle::ConstantTimeEq;

            let left = security_string(args.first(), "left value")?;
            let right = security_string(args.get(1), "right value")?;
            // Hash to equal-width inputs first, so comparison work does not vary
            // with the original secret lengths.
            let left = Sha256::digest(left.as_bytes());
            let right = Sha256::digest(right.as_bytes());
            Ok(Value::Bool(bool::from(left.ct_eq(&right))))
        }
        "isSecure" | "is_secure" => {
            let request = request_object(args.first())?;
            Ok(Value::Bool(matches!(
                request.get("protocol"),
                Some(Value::String(protocol)) if protocol.eq_ignore_ascii_case("https")
            )))
        }
        "clientIp" | "client_ip" => {
            let request = request_object(args.first())?;
            Ok(request.get("ip").cloned().unwrap_or(Value::Null))
        }
        other => Err(ModuleError {
            message: format!("security.{other}() does not exist"),
        }),
    }
}

'''
text = replace_once(text, 'const HTTP_RESPONSE_MARKER: &str = "__rbeHttpResponse";\n', helper + 'const HTTP_RESPONSE_MARKER: &str = "__rbeHttpResponse";\n', "request/security helpers")

new_tests = r'''
    #[test]
    fn request_helpers_read_the_bounded_request_snapshot() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("request".into())]);
        let request = Value::Object(HashMap::from([
            ("method".into(), Value::String("POST".into())),
            ("path".into(), Value::String("/api/users/7".into())),
            ("protocol".into(), Value::String("https".into())),
            ("ip".into(), Value::String("203.0.113.7".into())),
            ("body".into(), Value::Bool(true)),
            ("headers".into(), Value::Object(HashMap::from([("x-test".into(), Value::String("yes".into()))]))),
            ("query".into(), Value::Object(HashMap::from([("page".into(), Value::String("2".into()))]))),
            ("params".into(), Value::Object(HashMap::from([("id".into(), Value::String("7".into()))]))),
            ("cookies".into(), Value::Object(HashMap::from([("session".into(), Value::String("abc".into()))]))),
        ]));
        assert!(matches!(registry.call("request", "method", std::slice::from_ref(&request)).unwrap(), Value::String(value) if value == "POST"));
        assert!(matches!(registry.call("request", "header", &[request.clone(), Value::String("X-Test".into())]).unwrap(), Value::String(value) if value == "yes"));
        assert!(matches!(registry.call("request", "param", &[request.clone(), Value::String("id".into())]).unwrap(), Value::String(value) if value == "7"));
        assert!(matches!(registry.call("request", "query", &[request.clone(), Value::String("page".into())]).unwrap(), Value::String(value) if value == "2"));
        assert!(matches!(registry.call("request", "cookie", &[request, Value::String("session".into())]).unwrap(), Value::String(value) if value == "abc"));
    }

    #[test]
    fn security_helpers_are_explicit_and_constant_width() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("security".into())]);
        assert!(matches!(registry.call("security", "constantTimeEqual", &[Value::String("same".into()), Value::String("same".into())]).unwrap(), Value::Bool(true)));
        assert!(matches!(registry.call("security", "constantTimeEqual", &[Value::String("same".into()), Value::String("other".into())]).unwrap(), Value::Bool(false)));
        let request = Value::Object(HashMap::from([
            ("protocol".into(), Value::String("https".into())),
            ("ip".into(), Value::String("198.51.100.4".into())),
        ]));
        assert!(matches!(registry.call("security", "isSecure", std::slice::from_ref(&request)).unwrap(), Value::Bool(true)));
        assert!(matches!(registry.call("security", "clientIp", &[request]).unwrap(), Value::String(value) if value == "198.51.100.4"));
    }

'''
text = replace_once(text, '    #[test]\n    fn unknown_private_function_is_reported() {\n', new_tests + '    #[test]\n    fn unknown_private_function_is_reported() {\n', "request/security tests")
write(path, text)


# ---------------------------------------------------------------------------
# Async HTTP host capability. No redirects/proxy, bounded body, DNS pinning,
# and no loopback/private/link-local/special-address destinations.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/video_host.rs"
text = read(path)
text = replace_once(
    text,
    'use std::sync::Arc;\n',
    'use std::collections::HashMap;\nuse std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};\nuse std::sync::Arc;\nuse std::time::Duration;\n',
    "HTTP imports",
)
text = replace_once(
    text,
    '            if !matches!(module, "vm" | "video-manager") {\n                return Ok(None);\n            }\n',
    '            if module == "http" {\n                return call_http(function, args).await.map(Some);\n            }\n            if !matches!(module, "vm" | "video-manager") {\n                return Ok(None);\n            }\n',
    "HTTP host dispatch",
)

http_impl = r'''
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

async fn resolve_public_destination(url: &reqwest::Url) -> Result<Option<SocketAddr>, ModuleEvalError> {
    let host = url.host_str().ok_or_else(|| http_error("HTTP URL must include a host"))?;
    let port = url.port_or_known_default().ok_or_else(|| http_error("HTTP URL has no usable port"))?;
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
        return Err(http_error("HTTP DNS resolution returned too many addresses"));
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
        "get" => (reqwest::Method::GET, value_string(args.first(), "HTTP URL")?.to_string(), None, None, HTTP_DEFAULT_TIMEOUT_MS),
        "post" => (reqwest::Method::POST, value_string(args.first(), "HTTP URL")?.to_string(), None, args.get(1), HTTP_DEFAULT_TIMEOUT_MS),
        "request" => {
            let Some(Value::Object(options)) = args.first() else {
                return Err(http_error("http.request(options) requires an object"));
            };
            let method = options.get("method").map(|value| value_string(Some(value), "HTTP method")).transpose()?.unwrap_or("GET");
            let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .map_err(|_| http_error("HTTP method is invalid"))?;
            if !matches!(method, reqwest::Method::GET | reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH | reqwest::Method::DELETE | reqwest::Method::HEAD) {
                return Err(http_error("HTTP method is not permitted by the REL capability"));
            }
            let url = value_string(options.get("url"), "HTTP URL")?.to_string();
            let timeout_ms = match options.get("timeoutMs") {
                None => HTTP_DEFAULT_TIMEOUT_MS,
                Some(Value::Number(value)) if value.is_finite() && value.fract() == 0.0 && *value >= 1.0 => (*value as u64).min(HTTP_MAX_TIMEOUT_MS),
                Some(_) => return Err(http_error("HTTP timeoutMs must be a positive integer")),
            };
            (method, url, options.get("headers"), options.get("body"), timeout_ms)
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
                return Err(http_error(format!("HTTP header {name:?} is controlled by RBE")));
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
    if body.as_ref().is_some_and(|body| body.len() > HTTP_REQUEST_MAX_BYTES) {
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
        if body.len().saturating_add(chunk.len()) > HTTP_RESPONSE_MAX_BYTES {
            return Err(http_error("HTTP response exceeds 2 MiB"));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Value::Object(HashMap::from([
        ("status".into(), Value::Number(f64::from(status.as_u16()))),
        ("ok".into(), Value::Bool(status.is_success())),
        ("headers".into(), Value::Object(response_headers)),
        ("body".into(), Value::String(String::from_utf8_lossy(&body).into_owned())),
        (
            "contentType".into(),
            content_type.map(Value::String).unwrap_or(Value::Null),
        ),
    ])))
}

'''
text = replace_once(text, '\nfn value_to_json(value: Value) -> serde_json::Value {\n', '\n' + http_impl + 'fn value_to_json(value: Value) -> serde_json::Value {\n', "HTTP implementation")

http_tests = r'''

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
            ("headers".into(), Value::Object(HashMap::from([("Host".into(), Value::String("evil".into()))]))),
        ]));
        assert!(parse_http_call("request", &[options]).is_err());
    }
}
'''
text += http_tests
write(path, text)


# ---------------------------------------------------------------------------
# Runtime Image module loading must enforce the same dependency graph and
# direct-export contracts as disk loading.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/module_runtime.rs"
text = read(path)
text = replace_once(
    text,
    '            modules.insert(normalize(&path), file);\n        }\n\n        if !errors.is_empty() {\n',
    '            modules.insert(normalize(&path), file);\n        }\n\n        validate_module_graph(&binary_root, &modules, &mut errors);\n\n        if !errors.is_empty() {\n',
    "Runtime Image module graph validation",
)
old_graph = '''        let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for (path, file) in &modules {
            let mut dependencies = Vec::new();
            for import in &file.imports {
                if let Some(raw_path) = custom_import_path(import) {
                    let resolved = normalize(&resolve_custom_import(&binary_root, raw_path));
                    if modules.contains_key(&resolved) {
                        dependencies.push(resolved);
                    } else {
                        errors.push(ModuleCompileError {
                            code: "MOD2001",
                            path: path.clone(),
                            line: 1,
                            column: 1,
                            message: format!(
                                "module import {raw_path:?} resolves to missing file {}",
                                resolved.display()
                            ),
                        });
                    }
                }
            }
            graph.insert(path.clone(), dependencies);
        }
        detect_cycles(&graph, &mut errors);
'''
text = replace_once(text, old_graph, '        validate_module_graph(&binary_root, &modules, &mut errors);\n', "disk module graph validation")
module_graph = r'''
fn validate_module_graph(
    binary_root: &Path,
    modules: &HashMap<PathBuf, Arc<ModuleFile>>,
    errors: &mut Vec<ModuleCompileError>,
) {
    let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (owner_path, file) in modules {
        let mut dependencies = Vec::new();
        for import in &file.imports {
            let (raw_path, requested_export) = match import_base(import) {
                ImportTarget::Custom(path) => (path.as_str(), None),
                ImportTarget::CustomFunction { path, function } => {
                    (path.as_str(), Some(function.as_str()))
                }
                _ => continue,
            };
            let resolved = normalize(&resolve_custom_import(binary_root, raw_path));
            let Some(target) = modules.get(&resolved) else {
                errors.push(ModuleCompileError {
                    code: "MOD2001",
                    path: owner_path.clone(),
                    line: 1,
                    column: 1,
                    message: format!(
                        "module import {raw_path:?} resolves to missing file {}",
                        resolved.display()
                    ),
                });
                continue;
            };
            if let Some(export) = requested_export {
                if !target.exports.iter().any(|candidate| candidate == export) {
                    errors.push(ModuleCompileError {
                        code: "MOD2011",
                        path: owner_path.clone(),
                        line: 1,
                        column: 1,
                        message: format!(
                            "module import {raw_path:?} requests missing export {export:?}"
                        ),
                    });
                }
            }
            dependencies.push(resolved);
        }
        graph.insert(owner_path.clone(), dependencies);
    }
    detect_cycles(&graph, errors);
}

'''
text = replace_once(text, '\nfn load_one(path: &Path) -> Result<ModuleFile, ModuleCompileError> {\n', '\n' + module_graph + 'fn load_one(path: &Path) -> Result<ModuleFile, ModuleCompileError> {\n', "module graph helper")
module_test = r'''
    #[test]
    fn rejects_direct_import_of_missing_module_export() {
        let root = root();
        fs::write(
            root.join("module/b.module"),
            "export function present() { return true; }",
        )
        .unwrap();
        fs::write(
            root.join("module/a.module"),
            ":import[module&b.missing as missing]\nexport function run() { return true; }",
        )
        .unwrap();
        let errors = ModuleProgram::load(&root.join("module"))
            .expect_err("direct imports must reference a real export");
        assert!(errors.0.iter().any(|error| error.code == "MOD2011"));
        let _ = fs::remove_dir_all(root);
    }

'''
text = replace_once(text, '    #[test]\n    fn accepts_service_imports_without_module_dependencies() {\n', module_test + '    #[test]\n    fn accepts_service_imports_without_module_dependencies() {\n', "direct module export test")
write(path, text)


# ---------------------------------------------------------------------------
# Correct stale docs: executable modules already exist; this wave tightens the
# Runtime Image graph and completes the request/security/http built-in surface.
# ---------------------------------------------------------------------------
path = "docs/module-language.md"
text = read(path)
start = text.find("# `.module`")
marker = text.find("# RBE Module Language Specification")
if start != 0 or marker < 0:
    raise SystemExit("module docs header anchor changed")
text = '''# `.module`\n\n> Status: **IMPLEMENTED / EXECUTABLE**\n>\n> `.module` files are discovered or linked into the immutable Runtime Image, parsed as Module REL, validated as a dependency graph, and executed by the async `ModuleExecutor`. Exported functions support positional parameters, module-to-module calls, Service calls, and capability-scoped host calls. Cycles, missing module targets, and direct imports of missing exports fail during initialization.\n\n---\n\n''' + text[marker:]
text = text.replace("**Status:** **PLANNED / RESERVED — NOT IMPLEMENTED**", "**Status:** **IMPLEMENTED / LIVING SPECIFICATION**", 1)
text = text.replace("**This relationship is design-only today.** No executable `.module` call path currently exists end-to-end.", "This relationship is executable today through the immutable Runtime Image and `ModuleExecutor`.", 1)
text = text.replace("### `.module`\n\nPlanned behavior:", "### `.module`\n\nImplemented behavior:", 1)
text = text.replace("Once module execution is implemented, a caller is intended to use:", "Module callers use:", 1)
text = text.replace("The future loader must maintain a dependency graph and detect cycles:", "The loader maintains a dependency graph and rejects cycles:", 1)
write(path, text)

path = "docs/route-language.md"
text = read(path)
text = text.replace(
    "`.module` is the reusable logic layer. It is designed for arbitrary function names, multiple parameters, privileged capabilities, and heavier work. **Full `.module` execution is planned and remains separate from the currently implemented `.route` interpreter.**",
    "`.module` is the executable reusable logic layer. It supports arbitrary exported function names, multiple parameters, module-to-module calls, Service calls, and capability-scoped host calls through the immutable Runtime Image.",
    1,
)
text = text.replace(
    "Import resolution distinguishes curated built-ins from quoted module paths. `.module` loading is a planned capability even though path resolution/desugaring exists.",
    "Import resolution distinguishes curated built-ins from quoted module paths. `.module` targets are loaded from the immutable Runtime Image and their dependency/export contracts are validated before requests are served.",
    1,
)
text = text.replace("| `http` | Yes | No | Planned surface |", "| `http` | Yes | No | Implemented as bounded async outbound HTTP with SSRF protections |", 1)
text = text.replace("| `request` | Yes | Yes | Planned beyond the current minimal surface |", "| `request` | Yes | Yes | Implemented request snapshot helpers |", 1)
text = text.replace("| `security` | Yes | Yes | Policy-controlled surface; broader implementation planned |", "| `security` | Yes | Yes | Implemented request-security and constant-time comparison helpers |", 1)
text = text.replace(
    "The current RBE route engine is a tree-walking interpreter rather than a complete Rust source generator. Invalid routes must not become runnable route registrations.",
    "The active request path currently evaluates the immutable linked REL AST through `ModuleExecutor`; the cache also emits readable Rust source as the staging artifact for the route-to-WASM pipeline. Invalid routes never become runnable registrations.",
    1,
)
text = text.replace(
    "The intended module model is documented separately in [`module-language.md`](module-language.md). Full module loading/execution, recursive module graphs, arbitrary module function parameters, and cycle guards remain planned until the module runtime is implemented end-to-end.",
    "The executable module model is documented separately in [`module-language.md`](module-language.md). Module loading/execution, recursive calls, arbitrary parameters, Service calls, dependency validation, and cycle guards are implemented; the remaining compiler work is moving route execution from the linked AST to the WASM artifact path.",
    1,
)
write(path, text)

print("REL capability + module runtime wave applied")
