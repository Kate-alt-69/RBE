//! Per-request timing and smart structured request auditing.
//!
//! The terminal log stays compact. `request.queue.log` is a deliberately
//! smaller metadata stream: useful request identity, timing, client IP,
//! and only the proxy/geo/CORS metadata that actually exists.
//!
//! Repeated HTTP error responses are deduplicated so a bot repeatedly
//! probing or triggering errors cannot turn the audit file into a storage
//! bomb. 4xx responses use a longer suppression window; 5xx responses use
//! a shorter window so server-side failures remain more visible.

use atomic_io::AtomicIo;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use config::Config;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::info;

use crate::real_ip::extract_real_ip;

const CLIENT_ERROR_SUPPRESSION: Duration = Duration::from_secs(10);
const SERVER_ERROR_SUPPRESSION: Duration = Duration::from_secs(3);
const ERROR_STATE_TTL: Duration = Duration::from_secs(60);
const MAX_ERROR_BUCKETS: usize = 16_384;

static REQUEST_AUDIT_IO: OnceLock<AtomicIo> = OnceLock::new();
static ERROR_STATUS_STATE: OnceLock<Mutex<HashMap<ErrorStatusKey, ErrorStatusBucket>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ErrorStatusKey {
    client_ip: String,
    method: String,
    path: String,
    status: u16,
}

#[derive(Debug)]
struct ErrorStatusBucket {
    last_logged: Instant,
    suppressed: u64,
}

enum ErrorAuditDecision {
    Log { suppressed: u64 },
    Suppress,
    NotApplicable,
}

pub async fn request_timing(
    State(config): State<std::sync::Arc<Config>>,
    request: Request,
    next: Next,
) -> Response {
    let started_at = Instant::now();

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));

    let headers = request.headers().clone();
    let client_ip = extract_real_ip(
        &headers,
        peer,
        config.security.trusted_proxy_headers,
    );

    let origin = header(&headers, "origin");
    let preflight = method.as_str().eq_ignore_ascii_case("OPTIONS")
        && headers.contains_key("access-control-request-method");
    let request_cors_method = header(&headers, "access-control-request-method");
    let request_cors_headers = header(&headers, "access-control-request-headers");

    let mut response = next.run(request).await;

    let elapsed = started_at.elapsed();
    let duration_ms = elapsed.as_secs_f64() * 1000.0;
    let status = response.status().as_u16();

    let response_cors_origin = header(response.headers(), "access-control-allow-origin");
    let response_cors_methods = header(response.headers(), "access-control-allow-methods");
    let response_cors_headers = header(response.headers(), "access-control-allow-headers");
    let response_cors_credentials = header(response.headers(), "access-control-allow-credentials");
    let cors_blocked = origin.is_some()
        && response_cors_origin
            .as_deref()
            .map(|value| value != origin.as_deref().unwrap_or_default() && value != "*")
            .unwrap_or(true);

    if debug_enabled() {
        info!(
            method = %method,
            path = %path,
            query = %query,
            status = status,
            duration_ms = format_args!("{duration_ms:.3}"),
            client_ip = %client_ip,
            cors_origin = origin.as_deref().unwrap_or("-"),
            cors_response_origin = response_cors_origin.as_deref().unwrap_or("-"),
            cors_preflight = preflight,
            cors_blocked = cors_blocked,
            "request completed"
        );
    } else {
        info!(
            method = %method,
            path = %path,
            query = %query,
            status = status,
            duration_ms = format_args!("{duration_ms:.3}"),
            client_ip = %client_ip,
            "request completed"
        );
    }

    match error_audit_decision(&client_ip.to_string(), method.as_str(), &path, status) {
        ErrorAuditDecision::Suppress => {}
        ErrorAuditDecision::Log { suppressed } => {
            write_request_audit(
                &config,
                &headers,
                response.headers(),
                &method.to_string(),
                &path,
                &query,
                peer,
                client_ip.to_string(),
                duration_ms,
                status,
                origin,
                preflight,
                request_cors_method,
                request_cors_headers,
                response_cors_origin,
                response_cors_methods,
                response_cors_headers,
                response_cors_credentials,
                cors_blocked,
                suppressed,
            );
        }
        ErrorAuditDecision::NotApplicable => {
            write_request_audit(
                &config,
                &headers,
                response.headers(),
                &method.to_string(),
                &path,
                &query,
                peer,
                client_ip.to_string(),
                duration_ms,
                status,
                origin,
                preflight,
                request_cors_method,
                request_cors_headers,
                response_cors_origin,
                response_cors_methods,
                response_cors_headers,
                response_cors_credentials,
                cors_blocked,
                0,
            );
        }
    }

    if let Ok(value) = HeaderValue::from_str(&format!("total;dur={duration_ms:.3}")) {
        response.headers_mut().insert("server-timing", value);
    }

    response
}

fn debug_enabled() -> bool {
    std::env::args().any(|arg| arg == "-debug" || arg == "--debug")
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn collect_optional_headers(headers: &HeaderMap, names: &[&str]) -> Map<String, Value> {
    let mut result = Map::new();
    for name in names {
        if let Some(value) = header(headers, name) {
            result.insert((*name).to_string(), Value::String(value));
        }
    }
    result
}

fn suppression_window(status: u16) -> Option<Duration> {
    match status {
        400..=499 => Some(CLIENT_ERROR_SUPPRESSION),
        500..=599 => Some(SERVER_ERROR_SUPPRESSION),
        _ => None,
    }
}

fn error_audit_decision(
    client_ip: &str,
    method: &str,
    path: &str,
    status: u16,
) -> ErrorAuditDecision {
    let Some(window) = suppression_window(status) else {
        return ErrorAuditDecision::NotApplicable;
    };

    let state = ERROR_STATUS_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = ErrorStatusKey {
        client_ip: client_ip.to_string(),
        method: method.to_string(),
        path: path.to_string(),
        status,
    };

    let now = Instant::now();
    let Ok(mut buckets) = state.lock() else {
        return ErrorAuditDecision::Log { suppressed: 0 };
    };

    buckets.retain(|_, bucket| now.duration_since(bucket.last_logged) <= ERROR_STATE_TTL);

    if buckets.len() >= MAX_ERROR_BUCKETS && !buckets.contains_key(&key) {
        buckets.clear();
    }

    match buckets.get_mut(&key) {
        None => {
            buckets.insert(
                key,
                ErrorStatusBucket {
                    last_logged: now,
                    suppressed: 0,
                },
            );
            ErrorAuditDecision::Log { suppressed: 0 }
        }
        Some(bucket) if now.duration_since(bucket.last_logged) < window => {
            bucket.suppressed = bucket.suppressed.saturating_add(1);
            ErrorAuditDecision::Suppress
        }
        Some(bucket) => {
            let suppressed = bucket.suppressed;
            bucket.last_logged = now;
            bucket.suppressed = 0;
            ErrorAuditDecision::Log { suppressed }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn write_request_audit(
    config: &Config,
    request_headers: &HeaderMap,
    response_headers: &HeaderMap,
    method: &str,
    path: &str,
    query: &str,
    peer: SocketAddr,
    client_ip: String,
    duration_ms: f64,
    status: u16,
    origin: Option<String>,
    preflight: bool,
    request_cors_method: Option<String>,
    request_cors_headers: Option<String>,
    response_cors_origin: Option<String>,
    response_cors_methods: Option<String>,
    response_cors_headers: Option<String>,
    response_cors_credentials: Option<String>,
    cors_blocked: bool,
    suppressed: u64,
) {
    let mut entry = Map::new();
    entry.insert("ts_ms".into(), json!(now_ms()));
    entry.insert("method".into(), json!(method));
    entry.insert("path".into(), json!(path));
    if !query.is_empty() {
        entry.insert("query".into(), json!(query));
    }
    entry.insert("status".into(), json!(status));
    entry.insert("duration_ms".into(), json!(format!("{duration_ms:.3}")));
    entry.insert("client_ip".into(), json!(client_ip.clone()));

    if peer.ip().to_string() != client_ip {
        entry.insert("peer_ip".into(), json!(peer.ip().to_string()));
    }

    if let Some(host) = header(request_headers, "host") {
        entry.insert("host".into(), json!(host));
    }
    if let Some(user_agent) = header(request_headers, "user-agent") {
        entry.insert("user_agent".into(), json!(user_agent));
    }
    if let Some(referer) = header(request_headers, "referer") {
        entry.insert("referer".into(), json!(referer));
    }
    if let Some(accept) = header(request_headers, "accept") {
        entry.insert("accept".into(), json!(accept));
    }
    if let Some(accept_language) = header(request_headers, "accept-language") {
        entry.insert("accept_language".into(), json!(accept_language));
    }

    if let Some(suppression_window) = suppression_window(status) {
        if suppressed > 0 {
            entry.insert("kind".into(), json!("status_summary"));
            entry.insert("suppressed".into(), json!(suppressed));
            entry.insert(
                "suppression_window_ms".into(),
                json!(suppression_window.as_millis() as u64),
            );
        } else {
            entry.insert("kind".into(), json!(
                if status >= 500 { "5xx" } else { "4xx" }
            ));
        }

        // Error-status records intentionally stay compact. No raw request
        // headers, response headers, geo bundles, or proxy bundles are
        // persisted unless they provide a concrete extra debugging signal
        // below.
        if let Some(correlation_id) = header(response_headers, "x-correlation-id") {
            entry.insert("correlation_id".into(), json!(correlation_id));
        }
        if let Some(server_timing) = header(response_headers, "server-timing") {
            entry.insert("server_timing".into(), json!(server_timing));
        }

        append_audit_value(&Value::Object(entry));
        return;
    }

    let forwarded_chain = collect_optional_headers(
        request_headers,
        &[
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "via",
            "cf-ray",
        ],
    );
    if !forwarded_chain.is_empty() {
        entry.insert("proxy".into(), Value::Object(forwarded_chain));
    }

    let provider_identity = collect_optional_headers(
        request_headers,
        &[
            "cf-connecting-ip",
            "cf-connecting-ipv6",
            "true-client-ip",
            "x-real-ip",
            "x-azure-clientip",
            "fastly-client-ip",
        ],
    );
    if !provider_identity.is_empty() {
        entry.insert("provider".into(), Value::Object(provider_identity));
    }

    let geo_country = header(request_headers, "cf-ipcountry")
        .or_else(|| header(request_headers, "cloudfront-viewer-country"))
        .or_else(|| header(request_headers, "x-geo-country"));
    let geo_region = header(request_headers, "x-geo-region");
    let geo_city = header(request_headers, "x-geo-city");
    if geo_country.is_some() || geo_region.is_some() || geo_city.is_some() {
        entry.insert(
            "geo".into(),
            json!({
                "country": geo_country,
                "region": geo_region,
                "city": geo_city,
            }),
        );
    }

    if let Some(correlation_id) = header(response_headers, "x-correlation-id") {
        entry.insert("correlation_id".into(), json!(correlation_id));
    }
    if let Some(server_timing) = header(response_headers, "server-timing") {
        entry.insert("server_timing".into(), json!(server_timing));
    }

    let cors_has_data = origin.is_some()
        || preflight
        || request_cors_method.is_some()
        || request_cors_headers.is_some()
        || response_cors_origin.is_some()
        || response_cors_methods.is_some()
        || response_cors_headers.is_some()
        || response_cors_credentials.is_some()
        || cors_blocked;

    if cors_has_data {
        entry.insert(
            "cors".into(),
            json!({
                "origin": origin,
                "preflight": preflight,
                "blocked": cors_blocked,
                "request_method": request_cors_method,
                "request_headers": request_cors_headers,
                "response_allow_origin": response_cors_origin,
                "response_allow_methods": response_cors_methods,
                "response_allow_headers": response_cors_headers,
                "response_allow_credentials": response_cors_credentials,
            }),
        );
    }

    if config.security.trusted_proxy_headers {
        entry.insert("trusted_proxy_headers".into(), json!(true));
    }

    append_audit_value(&Value::Object(entry));
}

fn append_audit_value(value: &Value) {
    let mut line = match serde_json::to_vec(value) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    line.push(b'\n');

    let path = std::path::Path::new("./data/admin/request.queue.log");
    let io = REQUEST_AUDIT_IO.get_or_init(AtomicIo::new);
    let _ = io.append_locked(path, &line);
}
