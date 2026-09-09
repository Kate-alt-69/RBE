from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:180]!r}")
    write(path, text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# response builtin: typed REL response descriptors interpreted only at the
# HTTP edge. Ordinary return values remain JSON 200 for compatibility.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/modules.rs"
text = read(path)
text = text.replace(
    '''        "env" => matches!(function, "get"),
        "vm" | "video-manager" => matches!(''',
    '''        "env" => matches!(function, "get"),
        "response" => matches!(
            function,
            "json"
                | "text"
                | "html"
                | "status"
                | "noContent"
                | "no_content"
                | "redirect"
                | "withHeader"
                | "with_header"
                | "cookie"
                | "clearCookie"
                | "clear_cookie"
        ),
        "vm" | "video-manager" => matches!(''',
    1,
)
text = text.replace(
    '''            ModuleKind::Builtin(BuiltinModule::Response) => Err(ModuleError {
                message: format!("{module_name}.{function_name}() is not implemented yet"),
            }),''',
    '''            ModuleKind::Builtin(BuiltinModule::Response) => call_response(function_name, args),''',
    1,
)
anchor = '''fn runtime_start() -> &'static Instant {
'''
if anchor not in text:
    raise SystemExit("missing modules runtime_start anchor")
response_code = r'''const HTTP_RESPONSE_MARKER: &str = "__rbeHttpResponse";

fn response_error(message: impl Into<String>) -> ModuleError {
    ModuleError {
        message: message.into(),
    }
}

fn parse_http_status(value: Option<&Value>, default: u16) -> Result<u16, ModuleError> {
    let Some(value) = value else {
        return Ok(default);
    };
    let Value::Number(number) = value else {
        return Err(response_error("response status must be a number"));
    };
    if !number.is_finite() || number.fract() != 0.0 || !(100.0..=599.0).contains(number) {
        return Err(response_error("response status must be an integer from 100 through 599"));
    }
    Ok(*number as u16)
}

fn response_descriptor(kind: &str, status: u16, body: Value) -> Value {
    Value::Object(HashMap::from([
        (HTTP_RESPONSE_MARKER.into(), Value::Bool(true)),
        ("kind".into(), Value::String(kind.into())),
        ("status".into(), Value::Number(status as f64)),
        ("body".into(), body),
        ("headers".into(), Value::Object(HashMap::new())),
        ("cookies".into(), Value::Array(Vec::new())),
    ]))
}

fn response_fields(value: &Value) -> Result<HashMap<String, Value>, ModuleError> {
    let Value::Object(fields) = value else {
        return Err(response_error("response helper requires a response value as its first argument"));
    };
    if !matches!(fields.get(HTTP_RESPONSE_MARKER), Some(Value::Bool(true))) {
        return Err(response_error("response helper received a normal object instead of a response value"));
    }
    Ok(fields.clone())
}

fn response_string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, ModuleError> {
    match value {
        Some(Value::String(value)) => Ok(value),
        _ => Err(response_error(format!("{label} must be a string"))),
    }
}

fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            matches!(
                byte,
                b'!' | b'#'..=b'+' | b'-'..=b':' | b'<'..=b'[' | b']'..=b'~'
            ) && !matches!(byte, b'(' | b')' | b',' | b'/' | b':' | b';' | b'<' | b'=' | b'>' | b'?' | b'@' | b'[' | b'\\' | b']' | b'{' | b'}')
        })
}

fn safe_cookie_component(value: &str) -> bool {
    !value.contains(['\r', '\n', ';'])
}

fn build_cookie(
    name: &str,
    value: &str,
    options: Option<&Value>,
    clear: bool,
) -> Result<String, ModuleError> {
    if !valid_cookie_name(name) {
        return Err(response_error("cookie name contains invalid characters"));
    }
    if !safe_cookie_component(value) {
        return Err(response_error("cookie value contains invalid characters"));
    }
    let mut cookie = format!("{name}={value}");
    if clear {
        cookie.push_str("; Max-Age=0");
    }
    let Some(options) = options else {
        return Ok(cookie);
    };
    let Value::Object(options) = options else {
        return Err(response_error("cookie options must be an object"));
    };
    if let Some(Value::String(path)) = options.get("path") {
        if !safe_cookie_component(path) {
            return Err(response_error("cookie path contains invalid characters"));
        }
        cookie.push_str("; Path=");
        cookie.push_str(path);
    }
    if let Some(Value::String(domain)) = options.get("domain") {
        if !safe_cookie_component(domain) {
            return Err(response_error("cookie domain contains invalid characters"));
        }
        cookie.push_str("; Domain=");
        cookie.push_str(domain);
    }
    if !clear {
        if let Some(Value::Number(max_age)) = options.get("maxAge") {
            if !max_age.is_finite() || max_age.fract() != 0.0 || *max_age < 0.0 {
                return Err(response_error("cookie maxAge must be a non-negative integer"));
            }
            cookie.push_str(&format!("; Max-Age={}", *max_age as u64));
        }
    }
    if matches!(options.get("httpOnly"), Some(Value::Bool(true))) {
        cookie.push_str("; HttpOnly");
    }
    if matches!(options.get("secure"), Some(Value::Bool(true))) {
        cookie.push_str("; Secure");
    }
    if let Some(Value::String(same_site)) = options.get("sameSite") {
        let normalized = match same_site.to_ascii_lowercase().as_str() {
            "strict" => "Strict",
            "lax" => "Lax",
            "none" => "None",
            _ => return Err(response_error("cookie sameSite must be Strict, Lax, or None")),
        };
        cookie.push_str("; SameSite=");
        cookie.push_str(normalized);
    }
    Ok(cookie)
}

fn call_response(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "json" => {
            let body = args.first().cloned().unwrap_or(Value::Null);
            let status = parse_http_status(args.get(1), 200)?;
            Ok(response_descriptor("json", status, body))
        }
        "text" | "html" => {
            let body = response_string(args.first(), "response body")?.to_string();
            let status = parse_http_status(args.get(1), 200)?;
            Ok(response_descriptor(function_name, status, Value::String(body)))
        }
        "status" => {
            let status = parse_http_status(args.first(), 200)?;
            let body = args.get(1).cloned().unwrap_or(Value::Null);
            let kind = if matches!(body, Value::Null) { "empty" } else { "json" };
            Ok(response_descriptor(kind, status, body))
        }
        "noContent" | "no_content" => Ok(response_descriptor("empty", 204, Value::Null)),
        "redirect" => {
            let location = response_string(args.first(), "redirect location")?;
            if location.contains(['\r', '\n']) {
                return Err(response_error("redirect location contains invalid characters"));
            }
            let status = parse_http_status(args.get(1), 302)?;
            if !(300..=399).contains(&status) {
                return Err(response_error("redirect status must be in the 300 range"));
            }
            let Value::Object(mut fields) = response_descriptor("empty", status, Value::Null) else {
                unreachable!();
            };
            fields.insert(
                "headers".into(),
                Value::Object(HashMap::from([(
                    "location".into(),
                    Value::String(location.to_string()),
                )])),
            );
            Ok(Value::Object(fields))
        }
        "withHeader" | "with_header" => {
            let mut fields = response_fields(args.first().ok_or_else(|| response_error("missing response value"))?)?;
            let name = response_string(args.get(1), "header name")?.to_ascii_lowercase();
            let value = response_string(args.get(2), "header value")?.to_string();
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err(response_error("HTTP header contains invalid newline characters"));
            }
            let headers = fields
                .entry("headers".into())
                .or_insert_with(|| Value::Object(HashMap::new()));
            let Value::Object(headers) = headers else {
                return Err(response_error("response header storage is invalid"));
            };
            headers.insert(name, Value::String(value));
            Ok(Value::Object(fields))
        }
        "cookie" | "clearCookie" | "clear_cookie" => {
            let mut fields = response_fields(args.first().ok_or_else(|| response_error("missing response value"))?)?;
            let name = response_string(args.get(1), "cookie name")?;
            let clear = matches!(function_name, "clearCookie" | "clear_cookie");
            let (value, options) = if clear {
                ("", args.get(2))
            } else {
                (response_string(args.get(2), "cookie value")?, args.get(3))
            };
            let cookie = build_cookie(name, value, options, clear)?;
            let cookies = fields
                .entry("cookies".into())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(cookies) = cookies else {
                return Err(response_error("response cookie storage is invalid"));
            };
            cookies.push(Value::String(cookie));
            Ok(Value::Object(fields))
        }
        other => Err(response_error(format!("response.{other}() does not exist"))),
    }
}

'''
text = text.replace(anchor, response_code + anchor, 1)
# Add focused response tests before the last test module brace.
test_anchor = '''    #[test]
    fn unknown_private_function_is_reported() {
'''
if test_anchor not in text:
    raise SystemExit("missing modules test anchor")
response_tests = r'''    #[test]
    fn response_builders_create_typed_descriptors() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("response".into())]);
        let response = registry
            .call(
                "response",
                "json",
                &[Value::Bool(true), Value::Number(201.0)],
            )
            .expect("response.json");
        let Value::Object(fields) = response else {
            panic!("expected response object")
        };
        assert!(matches!(fields.get(HTTP_RESPONSE_MARKER), Some(Value::Bool(true))));
        assert!(matches!(fields.get("status"), Some(Value::Number(201.0))));
        assert!(matches!(fields.get("kind"), Some(Value::String(kind)) if kind == "json"));
    }

    #[test]
    fn response_headers_and_cookies_reject_injection() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("response".into())]);
        let base = registry.call("response", "noContent", &[]).unwrap();
        assert!(registry
            .call(
                "response",
                "withHeader",
                &[
                    base.clone(),
                    Value::String("x-ok".into()),
                    Value::String("bad\r\nheader".into()),
                ],
            )
            .is_err());
        assert!(registry
            .call(
                "response",
                "cookie",
                &[
                    base,
                    Value::String("session".into()),
                    Value::String("oops; injected=1".into()),
                ],
            )
            .is_err());
    }

'''
text = text.replace(test_anchor, response_tests + test_anchor, 1)
write(path, text)

# ---------------------------------------------------------------------------
# Route HTTP edge: dynamic file routes, full request context and conversion of
# response descriptors into real Axum responses.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
text = text.replace("use std::io::Write;", "use std::io::Write;\nuse std::net::SocketAddr;", 1)
text = text.replace(
    '''use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::routing::MethodRouter;
use axum::Router;''',
    '''use axum::body::{to_bytes, Body};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::MethodRouter;
use axum::Router;''',
    1,
)
old_url = '''pub(crate) fn url_path_for(api_dir: &Path, file_path: &Path) -> String {
    let relative = file_path.strip_prefix(api_dir).unwrap_or(file_path);
    let without_ext = relative.with_extension("");
    let mut segments: Vec<String> = without_ext
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if segments.last().map(|s| s == "index").unwrap_or(false) {
        segments.pop();
    }
    format!("/api/{}", segments.join("/"))
}
'''
new_url = r'''fn route_segment(segment: String) -> String {
    if let Some(inner) = segment
        .strip_prefix("[...")
        .and_then(|value| value.strip_suffix(']'))
        .filter(|value| !value.is_empty())
    {
        return format!("*{inner}");
    }
    if let Some(inner) = segment
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .filter(|value| !value.is_empty())
    {
        return format!(":{inner}");
    }
    segment
}

pub(crate) fn url_path_for(api_dir: &Path, file_path: &Path) -> String {
    let relative = file_path.strip_prefix(api_dir).unwrap_or(file_path);
    let without_ext = relative.with_extension("");
    let mut segments: Vec<String> = without_ext
        .components()
        .map(|component| route_segment(component.as_os_str().to_string_lossy().to_string()))
        .collect();
    if segments.last().is_some_and(|segment| segment == "index") {
        segments.pop();
    }
    format!("/api/{}", segments.join("/"))
}

pub(crate) fn collision_key_for(url_path: &str) -> String {
    url_path
        .split('/')
        .map(|segment| {
            if segment.starts_with(':') {
                ":"
            } else if segment.starts_with('*') {
                "*"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
'''
if old_url not in text:
    raise SystemExit("missing discovery url_path_for anchor")
text = text.replace(old_url, new_url, 1)
start = text.index("fn request_value(method: &str, path: &str) -> Value {")
end = text.index("\nfn build_method_router(", start)
http_core = r'''const HTTP_RESPONSE_MARKER: &str = "__rbeHttpResponse";

fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => Value::Number(value.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => {
            Value::Array(values.into_iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, json_to_value(value)))
                .collect(),
        ),
    }
}

fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn headers_value(headers: &HeaderMap) -> Value {
    let mut out = HashMap::new();
    for name in headers.keys() {
        let values = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        out.insert(name.as_str().to_string(), Value::String(values));
    }
    Value::Object(out)
}

fn cookies_value(headers: &HeaderMap) -> Value {
    let mut cookies = HashMap::new();
    if let Some(raw) = headers.get(header::COOKIE).and_then(|value| value.to_str().ok()) {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if !name.is_empty() {
                cookies.insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }
    Value::Object(cookies)
}

fn request_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

async fn request_value(
    state: &AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Result<Value, Response> {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0);
    let (parts, body) = request.into_parts();
    let raw = to_bytes(body, state.config.security.max_json_payload_bytes)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "REL route request body rejected");
            request_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured payload limit",
            )
        })?;

    let content_type = header_string(&parts.headers, header::CONTENT_TYPE);
    let body_value = if raw.is_empty() {
        Value::Null
    } else if content_type
        .as_deref()
        .is_some_and(|value| value.contains("application/json") || value.contains("+json"))
    {
        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|error| {
            request_error(StatusCode::BAD_REQUEST, format!("invalid JSON request body: {error}"))
        })?;
        json_to_value(parsed)
    } else {
        Value::String(String::from_utf8_lossy(&raw).into_owned())
    };

    let trust_proxy = state.config.security.trusted_proxy_headers;
    let forwarded_for = if trust_proxy {
        parts
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| Value::String(value.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let peer_ip = peer.map(|peer| peer.ip().to_string());
    let client_ip = forwarded_for
        .first()
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            _ => None,
        })
        .or(peer_ip)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let protocol = if trust_proxy {
        parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| matches!(*value, "http" | "https"))
            .unwrap_or("http")
    } else {
        "http"
    };
    let host = header_string(&parts.headers, header::HOST)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let user_agent = header_string(&parts.headers, header::USER_AGENT)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let content_length = header_string(&parts.headers, header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| Value::Number(value as f64))
        .unwrap_or(Value::Null);

    let fields = HashMap::from([
        ("method".into(), Value::String(parts.method.as_str().to_string())),
        ("path".into(), Value::String(parts.uri.path().to_string())),
        ("originalUrl".into(), Value::String(parts.uri.to_string())),
        (
            "params".into(),
            Value::Object(
                params
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            ),
        ),
        (
            "query".into(),
            Value::Object(
                query
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            ),
        ),
        ("headers".into(), headers_value(&parts.headers)),
        ("cookies".into(), cookies_value(&parts.headers)),
        ("body".into(), body_value),
        (
            "rawBody".into(),
            Value::String(String::from_utf8_lossy(&raw).into_owned()),
        ),
        ("ip".into(), client_ip),
        ("forwardedFor".into(), Value::Array(forwarded_for)),
        ("protocol".into(), Value::String(protocol.to_string())),
        ("host".into(), host),
        ("userAgent".into(), user_agent),
        (
            "contentType".into(),
            content_type.map(Value::String).unwrap_or(Value::Null),
        ),
        ("contentLength".into(), content_length),
    ]);
    Ok(Value::Object(fields))
}

fn descriptor_status(fields: &HashMap<String, Value>) -> Result<StatusCode, String> {
    let Some(Value::Number(status)) = fields.get("status") else {
        return Err("REL response descriptor is missing numeric status".into());
    };
    if !status.is_finite() || status.fract() != 0.0 || !(100.0..=599.0).contains(status) {
        return Err("REL response descriptor has invalid HTTP status".into());
    }
    StatusCode::from_u16(*status as u16).map_err(|error| error.to_string())
}

fn rel_http_response(value: &Value) -> Result<Option<Response>, String> {
    let Value::Object(fields) = value else {
        return Ok(None);
    };
    if !matches!(fields.get(HTTP_RESPONSE_MARKER), Some(Value::Bool(true))) {
        return Ok(None);
    }
    let status = descriptor_status(fields)?;
    let kind = match fields.get("kind") {
        Some(Value::String(kind)) => kind.as_str(),
        _ => return Err("REL response descriptor is missing response kind".into()),
    };
    let body_value = fields.get("body").unwrap_or(&Value::Null);
    let mut builder = Response::builder().status(status);
    if let Some(Value::Object(headers)) = fields.get("headers") {
        for (name, value) in headers {
            let Value::String(value) = value else {
                return Err(format!("REL response header {name:?} must be a string"));
            };
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            let value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
            builder = builder.header(name, value);
        }
    }
    if let Some(Value::Array(cookies)) = fields.get("cookies") {
        for cookie in cookies {
            let Value::String(cookie) = cookie else {
                return Err("REL response cookie must be a string".into());
            };
            let value = HeaderValue::from_str(cookie).map_err(|error| error.to_string())?;
            builder = builder.header(header::SET_COOKIE, value);
        }
    }

    let body = match kind {
        "json" => {
            if !fields
                .get("headers")
                .and_then(|value| match value { Value::Object(value) => Some(value), _ => None })
                .is_some_and(|headers| headers.keys().any(|name| name.eq_ignore_ascii_case("content-type")))
            {
                builder = builder.header(header::CONTENT_TYPE, "application/json; charset=utf-8");
            }
            Body::from(serde_json::to_vec(&value_to_json(body_value)).map_err(|error| error.to_string())?)
        }
        "text" => {
            let Value::String(body) = body_value else {
                return Err("REL text response body must be a string".into());
            };
            builder = builder.header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
            Body::from(body.clone())
        }
        "html" => {
            let Value::String(body) = body_value else {
                return Err("REL HTML response body must be a string".into());
            };
            builder = builder.header(header::CONTENT_TYPE, "text/html; charset=utf-8");
            Body::from(body.clone())
        }
        "empty" => Body::empty(),
        other => return Err(format!("unknown REL response kind {other:?}")),
    };
    builder.body(body).map(Some).map_err(|error| error.to_string())
}

async fn execute(
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    takes_request: bool,
    state: AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    let args = if takes_request {
        match request_value(&state, params, query, request).await {
            Ok(request) => vec![request],
            Err(response) => return response,
        }
    } else {
        // Even handlers without a request parameter must consume the request
        // body so connection reuse/backpressure behavior remains predictable.
        let _ = to_bytes(request.into_body(), state.config.security.max_json_payload_bytes).await;
        Vec::new()
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(VideoHostCapabilities::from_state(&state)),
    );
    match executor
        .call_inline(inline_file, INLINE_ROUTE_HANDLER, args)
        .await
    {
        Ok(value) => match rel_http_response(&value) {
            Ok(Some(response)) => response,
            Ok(None) => Json(value_to_json(&value)).into_response(),
            Err(error) => {
                tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
                append_runtime_error(&path, &error);
                request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
            }
        },
        Err(err) => {
            tracing::error!(error = %err, path = %path, "route evaluation failed");
            append_runtime_error(&path, &err.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}
'''
text = text[:start] + http_core + text[end:]
old_handler = '''        let takes_request = method_def.param_name.is_some();
        let module_program = module_program.clone();
        let url_path = url_path.clone();
        let verb = method_def.verb.clone();
        let handler_verb = verb.clone();
        let handler = move |State(state): State<AppState>| {
            let inline_file = inline_file.clone();
            let module_program = module_program.clone();
            let path = url_path.clone();
            let method = handler_verb.to_uppercase();
            async move {
                execute(
                    inline_file,
                    module_program,
                    takes_request,
                    state,
                    method,
                    path,
                )
                .await
            }
        };
'''
new_handler = '''        let takes_request = method_def.param_name.is_some();
        let module_program = module_program.clone();
        let verb = method_def.verb.clone();
        let handler = move |
            State(state): State<AppState>,
            AxumPath(params): AxumPath<HashMap<String, String>>,
            Query(query): Query<HashMap<String, String>>,
            request: Request,
        | {
            let inline_file = inline_file.clone();
            let module_program = module_program.clone();
            async move {
                execute(
                    inline_file,
                    module_program,
                    takes_request,
                    state,
                    params,
                    query,
                    request,
                )
                .await
            }
        };
'''
if old_handler not in text:
    raise SystemExit("missing route handler anchor")
text = text.replace(old_handler, new_handler, 1)
# Focused path/descriptor tests appended after build_routes.
text += r'''

#[cfg(test)]
mod http_edge_tests {
    use super::*;

    #[test]
    fn file_route_parameters_lower_to_axum_patterns() {
        let root = PathBuf::from("/tmp/api");
        assert_eq!(
            url_path_for(&root, &root.join("users/[uid].route")),
            "/api/users/:uid"
        );
        assert_eq!(
            url_path_for(&root, &root.join("files/[...path].route")),
            "/api/files/*path"
        );
        assert_eq!(collision_key_for("/api/users/:uid"), "/api/users/:");
        assert_eq!(collision_key_for("/api/users/:name"), "/api/users/:");
    }

    #[test]
    fn response_descriptor_becomes_real_http_response() {
        let value = Value::Object(HashMap::from([
            (HTTP_RESPONSE_MARKER.into(), Value::Bool(true)),
            ("kind".into(), Value::String("text".into())),
            ("status".into(), Value::Number(202.0)),
            ("body".into(), Value::String("accepted".into())),
            ("headers".into(), Value::Object(HashMap::new())),
            ("cookies".into(), Value::Array(Vec::new())),
        ]));
        let response = rel_http_response(&value).unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }
}
'''
write(path, text)

# ---------------------------------------------------------------------------
# Route collision detection consumes the same route lowering as the router and
# canonicalizes parameter names so [id] and [slug] are the same URL shape.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/route_collision.rs"
text = read(path)
text = text.replace(
    "use crate::lexer::Lexer;",
    "use crate::discovery::{collision_key_for, url_path_for};\nuse crate::lexer::Lexer;",
    1,
)
text = text.replace(
    "            let key = (url_path.clone(), verb.clone());",
    "            let key = (collision_key_for(&url_path), verb.clone());",
    1,
)
start = text.index("fn url_path_for(api_dir: &Path, file_path: &Path) -> String {")
end = text.index("\nfn is_in_native_namespace", start)
text = text[:start] + text[end + 1:]
test_anchor = '''    #[test]
    fn catches_native_api_namespace_collision() {
'''
if test_anchor not in text:
    raise SystemExit("missing route collision test anchor")
dynamic_test = r'''    #[test]
    fn catches_same_dynamic_shape_with_different_parameter_names() {
        let root = temp_api_dir();
        fs::create_dir_all(root.join("users")).unwrap();
        fs::write(
            root.join("users/[id].route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();
        fs::write(
            root.join("users/[slug].route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();

        let collisions = find_collisions(&root).unwrap();
        assert_eq!(collisions.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

'''
text = text.replace(test_anchor, dynamic_test + test_anchor, 1)
write(path, text)

# ---------------------------------------------------------------------------
# Canonical Route REL documentation: update the current contract rather than
# leaving the old minimal-request text around.
# ---------------------------------------------------------------------------
path = "doc/x.route/README.md"
text = read(path)
marker = "## Request object"
if marker in text:
    prefix = text.split(marker, 1)[0]
    suffix_marker = "## "
    remainder = text.split(marker, 1)[1]
    next_idx = remainder.find("\n## ")
    suffix = remainder[next_idx + 1:] if next_idx >= 0 else ""
    request_section = r'''## Request object

Route handlers now receive the real request snapshot when they declare a request parameter:

```text
request.method
request.path
request.originalUrl
request.params
request.query
request.headers
request.cookies
request.body
request.rawBody
request.ip
request.forwardedFor
request.protocol
request.host
request.userAgent
request.contentType
request.contentLength
```

JSON bodies are decoded into native REL values. Other body types are exposed as strings in the current v1 transport. Body reads are bounded by the configured security payload limit. Invalid JSON returns HTTP 400 and oversized bodies return HTTP 413 before REL execution.

Proxy-derived client IP/protocol information is used only when `security.trustedProxyHeaders` is enabled. Otherwise forwarded headers are treated as untrusted input and `request.ip` comes from the socket peer.

File-system route parameters are active:

```text
api/users/[uid].route       -> /api/users/:uid
api/files/[...path].route   -> /api/files/*path
```

Parameter names do not make two otherwise-identical route shapes distinct; `[id]` and `[slug]` for the same method/path collide at boot.

Current v1 limitations: duplicate query keys collapse to one value, duplicate request headers are joined for the REL object, and binary body APIs are not yet a first-class byte type.

## Response object

Plain REL return values remain JSON with status 200 for compatibility. Import `response` when explicit HTTP behavior is needed:

```text
:import[response]

class Route {
    post(request) {
        return response.json({ ok: true }, 201);
    }
}
```

Implemented response helpers:

```text
response.json(body, status?)
response.text(text, status?)
response.html(html, status?)
response.status(status, body?)
response.noContent()
response.redirect(location, status?)
response.withHeader(responseValue, name, value)
response.cookie(responseValue, name, value, options?)
response.clearCookie(responseValue, name, options?)
```

Header names/values and cookies are validated again at the Rust HTTP boundary; newline/header injection is rejected. Cookie options currently support `path`, `domain`, `maxAge`, `httpOnly`, `secure`, and `sameSite`.

Streaming bodies, first-class binary responses, file sends and SSE remain later transport extensions rather than being pretended complete.
'''
    text = prefix + request_section + ("\n" + suffix if suffix else "")
else:
    text += "\n\n" + request_section
write(path, text)
