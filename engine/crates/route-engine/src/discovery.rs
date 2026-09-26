//! Discovers `.route` files, validates them during boot, generates AOT
//! artifacts, and builds the Axum router. Compiler diagnostics are kept
//! out of normal runtime logs and written to `data/admin/compiler-error.txt`.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::MethodRouter;
use axum::Router;
use core_lib::{
    AppState, ContainerAuthorizedExecution, ContainerCapabilityKind, ContainerExecutionIdentity,
    ContainerWorkCost, CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_MAX_TIMEOUT_MS,
};

use sha2::{Digest, Sha256};

use crate::analyzer::{analyze, Severity};
use crate::ast::{FunctionDef, ModuleFile, RouteFile, Value};
use crate::field_manager::{FieldResolveError, FieldRoutePlan};
use crate::lexer::Lexer;
use crate::module_eval::ModuleExecutor;
use crate::module_runtime::{ModuleProgram, ServiceInterfaces};
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::runtime_image::RuntimeImage;
use crate::source_registry::SourceId;
use crate::terminal::Terminal;
use crate::transpiler::transpile_file;
use crate::video_host::RuntimeHostCapabilities;
use crate::wasm_compiler::{RouteWasmArtifact, RouteWasmInput};

pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

struct CacheEntry {
    hash: [u8; 32],
    file: Arc<RouteFile>,
}

#[derive(Default)]
pub struct RouteCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
}

impl RouteCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_entries(&self) -> MutexGuard<'_, HashMap<PathBuf, CacheEntry>> {
        match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => {
                // Parsed Routes are disposable cache state. If a parser/cache
                // thread panics, discard the possibly-partial cache and reparse
                // from authoritative source instead of poisoning every reload.
                let mut entries = poisoned.into_inner();
                entries.clear();
                self.entries.clear_poison();
                entries
            }
        }
    }

    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {
        let bytes = fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let hash = hash_bytes(&bytes);
        if let Some(entry) = self.lock_entries().get(path) {
            if entry.hash == hash {
                return Ok(entry.file.clone());
            }
        }

        let source = String::from_utf8(bytes)
            .map_err(|e| anyhow::anyhow!("{}: not valid UTF-8: {e}", path.display()))?;
        let tokens = Lexer::new(&source).tokenize().map_err(|e| {
            anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message)
        })?;
        let file = Parser::new(tokens).parse_file().map_err(|e| {
            anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message)
        })?;
        let file = Arc::new(file);
        self.lock_entries().insert(
            path.to_path_buf(),
            CacheEntry {
                hash,
                file: file.clone(),
            },
        );
        Ok(file)
    }
}

#[cfg(test)]
mod route_cache_poison_tests {
    use super::*;

    #[test]
    fn poisoned_route_cache_is_cleared_and_reparsed() {
        let root =
            std::env::temp_dir().join(format!("rbe-route-cache-poison-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("health.route");
        fs::write(&path, "class Route { get(req) { return true; } }").unwrap();

        let cache = RouteCache::new();
        cache.load(&path).unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _entries = cache.entries.lock().unwrap();
            panic!("intentional RouteCache poison");
        }));
        assert!(poisoned.is_err());
        assert!(cache.entries.is_poisoned());

        let changed = b"class Route { get(req) { return false; } }";
        fs::write(&path, changed).unwrap();
        cache.load(&path).unwrap();
        assert!(!cache.entries.is_poisoned());
        let entries = cache.lock_entries();
        assert_eq!(entries.get(&path).unwrap().hash, hash_bytes(changed));
        drop(entries);
        let _ = fs::remove_dir_all(&root);
    }
}

pub(crate) fn collect_files(
    dir: &Path,
    extension: &str,
    out: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, extension, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
            out.push(path);
        }
    }
    Ok(())
}

pub(crate) fn collect_route_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    collect_files(dir, "route", out)
}

fn route_segment(segment: String) -> String {
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

pub(crate) fn url_path_for_logical(logical_name: &str) -> String {
    let mut segments = logical_name
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| route_segment(segment.to_string()))
        .collect::<Vec<_>>();
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

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Null => serde_json::Value::Null,
        Value::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), value_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
    }
}

fn append_runtime_error(path: &str, error: &str) {
    let error_path = compiler_error_path();
    if let Some(parent) = error_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&error_path)
    {
        let _ = writeln!(file, "E4000: route evaluation failed at {path}: {error}");
    }
}

const INLINE_ROUTE_HANDLER: &str = "\0rbe-route-handler";

const HTTP_RESPONSE_MARKER: &str = "__rbeHttpResponse";

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

fn is_json_content_type(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type.to_ascii_lowercase().ends_with("+json")
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
    for raw in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
    {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if !name.is_empty() {
                // Cookie header field-lines are processed in HeaderMap order.
                // Preserve the existing last-value-wins behavior when a client
                // repeats the same cookie name across one or more field-lines.
                cookies.insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }
    Value::Object(cookies)
}

#[cfg(test)]
mod cookie_snapshot_tests {
    use super::*;

    #[test]
    fn cookie_snapshot_consumes_all_header_field_lines_in_order() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("first=one; shared=old"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("second=two; shared=new"),
        );

        let Value::Object(cookies) = cookies_value(&headers) else {
            panic!("expected cookie object");
        };
        assert!(matches!(cookies.get("first"), Some(Value::String(value)) if value == "one"));
        assert!(matches!(cookies.get("second"), Some(Value::String(value)) if value == "two"));
        assert!(matches!(cookies.get("shared"), Some(Value::String(value)) if value == "new"));
    }
}

fn request_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn field_resolution_response(path: &str, error: FieldResolveError) -> Response {
    if error.code.starts_with("FLD4") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "field_validation_failed",
                "code": error.code,
                "field": error.field,
                "message": error.message,
            })),
        )
            .into_response();
    }

    tracing::error!(
        path = %path,
        code = error.code,
        field = %error.field,
        message = %error.message,
        "FieldManager request resolution failed internally"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "field_runtime_failed",
            "code": error.code,
        })),
    )
        .into_response()
}

async fn request_value(
    state: &AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Result<Value, Box<Response>> {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0);
    let (parts, body) = request.into_parts();
    let raw = to_bytes(body, state.config.security.max_json_payload_bytes)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "REL route request body rejected");
            Box::new(request_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured payload limit",
            ))
        })?;

    let content_type = header_string(&parts.headers, header::CONTENT_TYPE);
    let body_value = if raw.is_empty() {
        Value::Null
    } else if is_json_content_type(content_type.as_deref()) {
        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|error| {
            Box::new(request_error(
                StatusCode::BAD_REQUEST,
                format!("invalid JSON request body: {error}"),
            ))
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
        (
            "method".into(),
            Value::String(parts.method.as_str().to_string()),
        ),
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
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
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
                .and_then(|value| match value {
                    Value::Object(value) => Some(value),
                    _ => None,
                })
                .is_some_and(|headers| {
                    headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("content-type"))
                })
            {
                builder = builder.header(header::CONTENT_TYPE, "application/json; charset=utf-8");
            }
            Body::from(
                serde_json::to_vec(&value_to_json(body_value))
                    .map_err(|error| error.to_string())?,
            )
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
    builder
        .body(body)
        .map(Some)
        .map_err(|error| error.to_string())
}

const NATIVE_ROUTE_ENVIRONMENT_PROFILE: &str = "general";
const NATIVE_ROUTE_TIMEOUT_HEADROOM_MS: u64 = 2_000;
const NATIVE_ROUTE_TIMEOUT: Duration = Duration::from_millis(
    PUBLIC_HTTP_MAX_TIMEOUT_MS.saturating_add(NATIVE_ROUTE_TIMEOUT_HEADROOM_MS),
);

#[derive(Debug, Clone)]
struct NativeRoutePlan {
    runtime_image: String,
    source_id: SourceId,
    artifact: RouteWasmArtifact,
}

fn route_value_response(path: &str, value: Value) -> Response {
    match rel_http_response(&value) {
        Ok(Some(response)) => response,
        Ok(None) => Json(value_to_json(&value)).into_response(),
        Err(error) => {
            tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
            append_runtime_error(path, &error);
            request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}

fn request_snapshot_member<'a>(request: Option<&'a Value>, member: &str) -> Option<&'a Value> {
    request.and_then(|request| match request {
        Value::Object(fields) => fields.get(member),
        _ => None,
    })
}

fn request_snapshot_field<'a>(request: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    request_snapshot_member(request, "fields").and_then(|fields| match fields {
        Value::Object(fields) => fields.get(name),
        _ => None,
    })
}

fn encode_native_route_input(
    path: &str,
    value: &Value,
    label: &str,
    limit: usize,
    too_large_message: &'static str,
) -> Result<Vec<u8>, Box<Response>> {
    let input = serde_json::to_vec(&value_to_json(value)).map_err(|error| {
        tracing::error!(error = %error, path = %path, input = label, "encode native Route-WASM input");
        Box::new(request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "native route input could not be encoded",
        ))
    })?;
    if input.len() > limit {
        return Err(Box::new(request_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            too_large_message,
        )));
    }
    Ok(input)
}

async fn execute_native_route(
    plan: &NativeRoutePlan,
    image: &RuntimeImage,
    state: &AppState,
    path: &str,
    input: Vec<u8>,
) -> Response {
    if image.image_id != plan.runtime_image {
        let error = "native Route-WASM identity no longer matches the active Runtime Image";
        tracing::error!(
            path = %path,
            expected_image = %plan.runtime_image,
            active_image = %image.image_id,
            source = %plan.source_id,
            "native route authority changed underneath the router"
        );
        append_runtime_error(path, error);
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
    }

    let grants = match image.container_capability_grants(&plan.source_id) {
        Ok(grants) => grants,
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                "native route capability lowering failed closed"
            );
            append_runtime_error(path, &error.to_string());
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route capability requirements are not executable",
            );
        }
    };

    let network_cost = u64::from(
        grants
            .iter()
            .any(|grant| grant.kind == ContainerCapabilityKind::Network),
    );
    let identity = ContainerExecutionIdentity {
        runtime_image: &plan.runtime_image,
        source_id: plan.source_id.as_str(),
        environment: NATIVE_ROUTE_ENVIRONMENT_PROFILE,
    };
    let output = match state
        .container
        .execute_authorized(ContainerAuthorizedExecution {
            identity,
            artifact_hash: &plan.artifact.sha256,
            wasm: plan.artifact.bytes.clone(),
            grants,
            input,
            declared_cost: ContainerWorkCost {
                cpu: 1,
                memory: 1,
                io: 0,
                network: network_cost,
            },
            timeout: NATIVE_ROUTE_TIMEOUT,
        })
        .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                image = %plan.runtime_image,
                "native Route-WASM Container execution failed"
            );
            append_runtime_error(path, "native Route-WASM Container execution failed");
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route execution failed",
            );
        }
    };

    let value = match serde_json::from_slice::<serde_json::Value>(&output) {
        Ok(value) => json_to_value(value),
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                "native Route-WASM returned invalid JSON"
            );
            append_runtime_error(path, "native Route-WASM returned invalid JSON");
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route returned an invalid result",
            );
        }
    };
    route_value_response(path, value)
}

#[derive(Clone)]
struct RouteHandlerPlan {
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    field_plan: Arc<FieldRoutePlan>,
    takes_request: bool,
}

async fn execute(
    plan: RouteHandlerPlan,
    state: AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
    let RouteHandlerPlan {
        inline_file,
        module_program,
        native_plan,
        field_plan,
        takes_request,
    } = plan;
    let path = request.uri().path().to_string();
    let image = match request
        .extensions()
        .get::<Arc<crate::runtime_image::RuntimeImageSlot>>()
    {
        Some(slot) => slot.snapshot(),
        None => {
            let error = "Runtime Image extension is unavailable";
            tracing::error!(path = %path, error, "REL request has no active Runtime Image");
            return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
        }
    };
    let needs_snapshot = takes_request || field_plan.is_active();
    let mut request_snapshot = if needs_snapshot {
        match request_value(&state, params, query, request).await {
            Ok(request) => Some(request),
            Err(response) => return *response,
        }
    } else {
        // Even handlers without a request parameter must consume the request
        // body so connection reuse/backpressure behavior remains predictable.
        let _ = to_bytes(
            request.into_body(),
            state.config.security.max_json_payload_bytes,
        )
        .await;
        None
    };

    let field_context = if field_plan.is_active() {
        let snapshot = request_snapshot
            .as_ref()
            .expect("FieldManager-active Route always builds a request snapshot");
        match field_plan.resolve(snapshot, module_program.as_ref()).await {
            Ok(context) => Some(context),
            Err(error) => return field_resolution_response(&path, error),
        }
    } else {
        None
    };

    if let (Some(Value::Object(request)), Some(fields)) =
        (request_snapshot.as_mut(), field_context.as_ref())
    {
        request.insert("fields".into(), fields.resolved_object());
    }
    if let Some(plan) = native_plan.as_deref() {
        let snapshot = request_snapshot.as_ref();
        let input = match &plan.artifact.input {
            RouteWasmInput::None => Vec::new(),
            RouteWasmInput::JsonBody => {
                let body = request_snapshot_member(snapshot, "body").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    body,
                    "req.body",
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route body exceeds the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return *response,
                }
            }
            RouteWasmInput::JsonFields => {
                let fields = request_snapshot_member(snapshot, "fields").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    fields,
                    "req.fields",
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route fields exceed the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return *response,
                }
            }
            RouteWasmInput::JsonField { name } => {
                let value = request_snapshot_field(snapshot, name).unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    value,
                    name,
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route field exceeds the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return *response,
                }
            }
            RouteWasmInput::JsonBodyCapability { max_bytes } => {
                let body = request_snapshot_member(snapshot, "body").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    body,
                    "capability req.body",
                    (*max_bytes).min(CONTAINER_MAX_EXECUTION_INPUT_BYTES),
                    "native route body exceeds the capability payload limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return *response,
                }
            }
        };
        return execute_native_route(plan, image.as_ref(), &state, &path, input).await;
    }
    let args = if takes_request {
        vec![request_snapshot.take().unwrap_or(Value::Null)]
    } else {
        Vec::new()
    };
    let host = match field_context {
        Some(fields) => RuntimeHostCapabilities::from_state_image_and_fields(&state, image, fields),
        None => RuntimeHostCapabilities::from_state_and_image(&state, image),
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(host),
    );
    match executor
        .call_inline(inline_file, INLINE_ROUTE_HANDLER, args)
        .await
    {
        Ok(value) => route_value_response(&path, value),
        Err(err) if err.code.starts_with("FLD4") => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "field_validation_failed",
                "code": err.code,
                "message": err.message,
            })),
        )
            .into_response(),
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

fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    field_plan: Arc<FieldRoutePlan>,
) -> MethodRouter<AppState> {
    let mut router = MethodRouter::<AppState>::new();
    for method_def in &file.methods {
        let mut functions = file.functions.clone();
        functions.push(FunctionDef {
            name: INLINE_ROUTE_HANDLER.to_string(),
            params: method_def.param_name.clone().into_iter().collect(),
            body: method_def.body.clone(),
        });
        let inline_file = Arc::new(ModuleFile {
            imports: file.imports.clone(),
            functions,
            exports: Vec::new(),
        });
        let handler_plan = RouteHandlerPlan {
            inline_file,
            module_program: module_program.clone(),
            native_plan: native_plan.clone(),
            field_plan: field_plan.clone(),
            takes_request: method_def.param_name.is_some(),
        };
        let verb = method_def.verb.clone();
        let handler = move |State(state): State<AppState>,
                            AxumPath(params): AxumPath<HashMap<String, String>>,
                            Query(query): Query<HashMap<String, String>>,
                            request: Request| {
            let handler_plan = handler_plan.clone();
            async move { execute(handler_plan, state, params, query, request).await }
        };
        router = match verb.as_str() {
            "get" => router.get(handler),
            "post" => router.post(handler),
            "put" => router.put(handler),
            "delete" => router.delete(handler),
            "patch" => router.patch(handler),
            "head" => router.head(handler),
            "options" => router.options(handler),
            _ => router,
        };
    }
    router
}

fn compiler_error_path() -> PathBuf {
    PathBuf::from("data")
        .join("admin")
        .join("compiler-error.txt")
}

fn find_symbol_location(source: &str, symbol: Option<&str>) -> (usize, usize) {
    let Some(symbol) = symbol.filter(|value| !value.is_empty()) else {
        return (1, 1);
    };
    for (line_idx, line) in source.lines().enumerate() {
        if let Some(byte_idx) = line.find(symbol) {
            let column = line[..byte_idx].chars().count() + 1;
            return (line_idx + 1, column);
        }
    }
    (1, 1)
}

fn frame_diagnostic_with_symbol(
    path: &Path,
    source: &str,
    fallback_line: usize,
    fallback_column: usize,
    message: &str,
    symbol: Option<&str>,
    terminal_width: usize,
) -> String {
    let (line, column) = if symbol.is_some() {
        find_symbol_location(source, symbol)
    } else {
        (fallback_line, fallback_column)
    };
    let lines: Vec<&str> = source.lines().collect();
    let start = line.saturating_sub(1);
    let end = (start + 4).min(lines.len());
    let line_numbers = if end > start {
        end.to_string().len()
    } else {
        1
    };
    let content_width = terminal_width.saturating_sub(line_numbers + 8).max(24);
    let border = "#".repeat(content_width + line_numbers + 7);
    let mut out = String::new();

    out.push_str(&format!("{}\n{}\n", path.display(), border));
    for (idx, text) in lines.iter().enumerate().take(end).skip(start) {
        let number = idx + 1;
        let clipped: String = text.chars().take(content_width).collect();
        let marker = if number == line {
            let used = format!("|{number:>width$}| {clipped}", width = line_numbers)
                .chars()
                .count();
            let remaining = border.len().saturating_sub(used + 1);
            format!(" {}", "<".repeat(remaining.max(2)))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "|{number:>width$}| {clipped}{marker}\n",
            width = line_numbers
        ));
    }
    out.push_str(&format!("{}\n", border));

    let pointer_indent = line_numbers + 4 + column.saturating_sub(1);
    out.push_str(&format!(
        "{}^\n{}\nline {}, column {}\n\n",
        " ".repeat(pointer_indent),
        message,
        line,
        column
    ));
    out
}

struct FileDiagnosticReport {
    path: PathBuf,
    errors: usize,
    warnings: usize,
    details: Vec<String>,
    error_details: Vec<String>,
}

fn render_diagnostic_reports(reports: &[FileDiagnosticReport]) {
    if reports.is_empty() {
        return;
    }

    for report in reports {
        println!(
            "{} error{}, {} warning{} in file {}",
            report.errors,
            if report.errors == 1 { "" } else { "s" },
            report.warnings,
            if report.warnings == 1 { "" } else { "s" },
            report.path.display()
        );
        for detail in &report.details {
            println!("{}", detail);
        }
    }
}

/// Run the route compiler as a boot-owned terminal session. Every file gets
/// three work units: parse, semantic analysis, and Rust artifact generation.
/// Syntax/semantic errors are accumulated across the entire tree and written
/// to `data/admin/compiler-error.txt` before boot is allowed to continue.
fn boot_compile(
    _api_dir: &Path,
    files: &[PathBuf],
) -> anyhow::Result<Vec<(PathBuf, Arc<RouteFile>)>> {
    let error_path = compiler_error_path();
    if let Some(parent) = error_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut error_file = fs::File::create(&error_path)?;

    let terminal = Terminal::new();
    let terminal_width = terminal.width();
    let total_units = files.len().saturating_mul(3);
    let mut done = 0usize;
    let mut valid = Vec::with_capacity(files.len());
    let mut reports = Vec::new();

    terminal.begin_boot();
    terminal.render(files.len(), None, "Parsing", 0, total_units);

    for path in files {
        let display_path = path.to_string_lossy();
        let mut report = FileDiagnosticReport {
            path: path.clone(),
            errors: 0,
            warnings: 0,
            details: Vec::new(),
            error_details: Vec::new(),
        };

        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.errors = 1;
                let detail = format!("{}: failed to read file: {}\n", path.display(), error);
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let source = match String::from_utf8(bytes) {
            Ok(source) => source,
            Err(error) => {
                report.errors = 1;
                let detail = format!("{}: invalid UTF-8: {}\n", path.display(), error);
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let tokens = match Lexer::new(&source).tokenize() {
            Ok(tokens) => tokens,
            Err(error) => {
                report.errors = 1;
                let detail = frame_diagnostic_with_symbol(
                    path,
                    &source,
                    error.line,
                    error.column,
                    &error.message,
                    None,
                    terminal_width,
                );
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let (file_opt, parse_errors) = Parser::new(tokens).parse_file_collecting();
        done += 1;
        terminal.render(
            files.len(),
            Some(&display_path),
            "Parsing",
            done,
            total_units,
        );

        for error in &parse_errors {
            let detail = frame_diagnostic_with_symbol(
                path,
                &source,
                error.line,
                error.column,
                &error.message,
                None,
                terminal_width,
            );
            report.errors += 1;
            report.details.push(detail.clone());
            report.error_details.push(detail);
        }

        let Some(file) = file_opt else {
            done += 2;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Parsing",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        };

        if report.errors > 0 {
            done += 2;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Parsing",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        }

        let file = Arc::new(file);
        terminal.render(
            files.len(),
            Some(&display_path),
            "Semantic",
            done,
            total_units,
        );

        let diagnostics = analyze(&file);
        report.errors += diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        report.warnings = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();

        for diagnostic in &diagnostics {
            let message = if matches!(diagnostic.code, "E3010" | "E3011") {
                diagnostic
                    .message
                    .replace("from the route file", &format!("from {}", path.display()))
            } else {
                diagnostic.message.clone()
            };
            let detail = frame_diagnostic_with_symbol(
                path,
                &source,
                1,
                1,
                &message,
                diagnostic.symbol.as_deref(),
                terminal_width,
            );
            report.details.push(detail.clone());
            if diagnostic.severity == Severity::Error {
                report.error_details.push(detail);
            }
        }

        done += 1;
        terminal.render(
            files.len(),
            Some(&display_path),
            "Semantic",
            done,
            total_units,
        );

        if report.errors > 0 {
            done += 1;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Semantic",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        }

        terminal.render(
            files.len(),
            Some(&display_path),
            "Generating",
            done,
            total_units,
        );
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        match transpile_file(&file, &path.to_string_lossy(), &module_names) {
            Ok(_) => {
                done += 1;
                valid.push((path.clone(), file));
            }
            Err(error) => {
                report.errors += 1;
                let detail = frame_diagnostic_with_symbol(
                    path,
                    &source,
                    1,
                    1,
                    &error.message,
                    None,
                    terminal_width,
                );
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 1;
            }
        }
        terminal.render(
            files.len(),
            Some(&display_path),
            "Generating",
            done,
            total_units,
        );

        if report.errors > 0 || report.warnings > 0 {
            reports.push(report);
        }
    }

    let total_errors: usize = reports.iter().map(|report| report.errors).sum();
    if total_errors > 0 {
        for report in &reports {
            for detail in &report.error_details {
                error_file.write_all(detail.as_bytes())?;
            }
        }
        render_diagnostic_reports(&reports);
        terminal.end_boot();
        return Err(anyhow::anyhow!(
            "route compiler found {} error(s); see {}",
            total_errors,
            error_path.display()
        ));
    }

    if !reports.is_empty() {
        render_diagnostic_reports(&reports);
    }

    terminal.render(files.len(), None, "Ready", total_units, total_units);
    terminal.end_boot();
    Ok(valid)
}

/// Scans `api_dir`, validates every `.route` file before routing starts,
/// and only then constructs the Axum router. A broken route fails the boot,
/// but errors from every file are collected into compiler-error.txt first.
pub fn build_routes(
    api_dir: &Path,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<Router<AppState>> {
    let module_program = Arc::new(ModuleProgram::load_default_with_services(
        service_interfaces,
    )?);
    tracing::info!(
        modules = module_program.len(),
        module_dir = %module_program.module_dir().display(),
        "validated .module files"
    );

    let mut files = Vec::new();
    collect_route_files(api_dir, &mut files)?;
    files.sort();

    let compiled = boot_compile(api_dir, &files)?;
    let mut router: Router<AppState> = Router::new();

    for (path, route_file) in compiled {
        let url_path = url_path_for(api_dir, &path);

        tracing::info!(
            path = %path.display(),
            url = %url_path,
            methods = ?route_file.methods.iter().map(|m| &m.verb).collect::<Vec<_>>(),
            "registered .route file"
        );

        router = router.route(
            &url_path,
            build_method_router(
                &route_file,
                module_program.clone(),
                None,
                Arc::new(FieldRoutePlan::default()),
            ),
        );
    }

    Ok(router)
}

/// Build the executable REL router from the exact immutable ASTs linked by
/// RELC. Disk files are deployment inputs, not runtime authorities.
pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<Router<AppState>> {
    crate::route_collision::validate_image(image)?;
    let module_program = Arc::new(ModuleProgram::from_runtime_image_with_services(
        image,
        service_interfaces,
    )?);
    tracing::info!(
        modules = module_program.len(),
        image = %image.image_id,
        "using Runtime Image module snapshots"
    );

    let mut router: Router<AppState> = Router::new();
    for id in &image.routes {
        let manifest = image
            .source(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no manifest"))?;
        let route_file = image.route_file(id).ok_or_else(|| {
            anyhow::anyhow!("Runtime Image route {id} has no executable snapshot")
        })?;
        let url_path = manifest
            .route_path
            .clone()
            .unwrap_or_else(|| url_path_for_logical(&manifest.logical_name));
        tracing::info!(
            source = %id,
            url = %url_path,
            methods = ?route_file.methods.iter().map(|method| &method.verb).collect::<Vec<_>>(),
            "registered Runtime Image Route REL"
        );
        let field_plan = Arc::new(
            FieldRoutePlan::from_route(image, route_file.as_ref(), &manifest.logical_name)
                .map_err(anyhow::Error::msg)?,
        );
        // FLD-005 lets native Route-WASM consume the exact host-resolved
        // FieldManager context. Unsupported route shapes still have no artifact
        // and therefore fall back to the linked evaluator as before.
        let native_plan = image.route_wasm_artifact(id).map(|artifact| {
            Arc::new(NativeRoutePlan {
                runtime_image: image.image_id.clone(),
                source_id: id.clone(),
                artifact: artifact.clone(),
            })
        });
        router = router.route(
            &url_path,
            build_method_router(
                route_file.as_ref(),
                module_program.clone(),
                native_plan,
                field_plan,
            ),
        );
    }
    Ok(router)
}

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
    fn json_content_type_detection_matches_structured_media_types() {
        for content_type in [
            "application/json",
            "Application/JSON; Charset=UTF-8",
            "application/problem+json",
            "APPLICATION/VND.API+JSON; charset=utf-8",
        ] {
            assert!(is_json_content_type(Some(content_type)), "{content_type}");
        }

        for content_type in [
            "text/plain",
            "text/application/jsonish",
            "application/problem+jsonx",
            "application/jsonish",
        ] {
            assert!(!is_json_content_type(Some(content_type)), "{content_type}");
        }
        assert!(!is_json_content_type(None));
    }

    #[test]
    fn field_resolution_errors_keep_client_and_internal_boundaries_separate() {
        let validation = field_resolution_response(
            "/api/test",
            FieldResolveError {
                code: "FLD4002",
                field: "count".into(),
                message: "invalid integer".into(),
            },
        );
        assert_eq!(validation.status(), StatusCode::BAD_REQUEST);

        let internal = field_resolution_response(
            "/api/test",
            FieldResolveError {
                code: "FLD5000",
                field: "request".into(),
                message: "request snapshot is invalid".into(),
            },
        );
        assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn native_route_wait_budget_exceeds_public_http_broker_timeout() {
        assert!(
            NATIVE_ROUTE_TIMEOUT.as_millis() > u128::from(PUBLIC_HTTP_MAX_TIMEOUT_MS),
            "outer Container wait must leave IPC/cancellation headroom after broker timeout"
        );
        assert_eq!(
            NATIVE_ROUTE_TIMEOUT.as_millis(),
            u128::from(PUBLIC_HTTP_MAX_TIMEOUT_MS + NATIVE_ROUTE_TIMEOUT_HEADROOM_MS)
        );
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
