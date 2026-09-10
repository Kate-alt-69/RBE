//! Discovers `.route` files, validates them during boot, generates AOT
//! artifacts, and builds the Axum router. Compiler diagnostics are kept
//! out of normal runtime logs and written to `data/admin/compiler-error.txt`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::MethodRouter;
use axum::Router;
use core_lib::AppState;

use crate::analyzer::{analyze, Severity};
use crate::ast::{FunctionDef, ModuleFile, RouteFile, Value};
use crate::lexer::Lexer;
use crate::module_eval::ModuleExecutor;
use crate::module_runtime::{ModuleProgram, ServiceInterfaces};
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::runtime_image::RuntimeImage;
use crate::terminal::Terminal;
use crate::transpiler::transpile_file;
use crate::video_host::RuntimeHostCapabilities;

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

struct CacheEntry {
    hash: u64,
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

    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {
        let bytes = fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let hash = hash_bytes(&bytes);
        if let Some(entry) = self.entries.lock().unwrap().get(path) {
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
        self.entries.lock().unwrap().insert(
            path.to_path_buf(),
            CacheEntry {
                hash,
                file: file.clone(),
            },
        );
        Ok(file)
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
    if let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    {
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
    } else if content_type
        .as_deref()
        .is_some_and(|value| value.contains("application/json") || value.contains("+json"))
    {
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
    let args = if takes_request {
        match request_value(&state, params, query, request).await {
            Ok(request) => vec![request],
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
        Vec::new()
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(RuntimeHostCapabilities::from_state_and_image(&state, image)),
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

fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    _url_path: String,
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
        let takes_request = method_def.param_name.is_some();
        let module_program = module_program.clone();
        let verb = method_def.verb.clone();
        let handler = move |State(state): State<AppState>,
                            AxumPath(params): AxumPath<HashMap<String, String>>,
                            Query(query): Query<HashMap<String, String>>,
                            request: Request| {
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
            build_method_router(&route_file, module_program.clone(), url_path.clone()),
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
        router = router.route(
            &url_path,
            build_method_router(
                route_file.as_ref(),
                module_program.clone(),
                url_path.clone(),
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
