//! Per-request timing and structured logging. The normal terminal line
//! stays compact. The local request audit queue records only useful
//! request metadata, omitting empty/duplicated fields so long-running
//! services do not turn routine traffic into a multi-GB log explosion.

use std::collections::BTreeMap;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use config::Config;
use serde_json::{json, Map, Value};

use crate::real_ip::extract_real_ip;

static REQUEST_AUDIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub async fn request_timing(
    State(config): State<Arc<Config>>,
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
        tracing::info!(
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
        tracing::info!(
            method = %method,
            path = %path,
            query = %query,
            status = status,
            duration_ms = format_args!("{duration_ms:.3}"),
            client_ip = %client_ip,
            "request completed"
        );
    }

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
    );

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

fn insert_if_some(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        map.insert(key.to_string(), Value::String(value));
    }
}

fn insert_if_string_map_nonempty(target: &mut Map<String, Value>, key: &str, value: Map<String, Value>) {
    if !value.is_empty() {
        target.insert(key.to_string(), Value::Object(value));
    }
}

fn selected_headers(headers: &HeaderMap, names: &[&str]) -> Map<String, Value> {
    let mut result = Map::new();
    for name in names {
        insert_if_some(&mut result, name, header(headers, name));
    }
    result
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
) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();

    let peer_ip = peer.ip().to_string();

    let mut entry = Map::new();
    entry.insert("ts_ms".into(), json!(now_ms));
    entry.insert("method".into(), json!(method));
    entry.insert("path".into(), json!(path));
    if !query.is_empty() {
        entry.insert("query".into(), json!(query));
    }
    entry.insert("status".into(), json!(status));
    entry.insert("duration_ms".into(), json!(duration_ms));
    entry.insert("client_ip".into(), json!(client_ip));
    if peer_ip != client_ip {
        entry.insert("peer_ip".into(), json!(peer_ip));
    }

    let mut request = Map::new();
    insert_if_some(&mut request, "host", header(request_headers, "host"));
    insert_if_some(&mut request, "user_agent", header(request_headers, "user-agent"));
    insert_if_some(&mut request, "referer", header(request_headers, "referer"));
    insert_if_some(&mut request, "origin", origin.clone());

    let mut proxy = BTreeMap::<String, Value>::new();
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-port",
        "via",
        "cf-ray",
    ] {
        if let Some(value) = header(request_headers, name) {
            proxy.insert(name.replace('-', "_"), Value::String(value));
        }
    }
    if !proxy.is_empty() {
        request.insert("proxy".into(), Value::Object(proxy.into_iter().collect()));
    }

    let mut provider = selected_headers(
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
    insert_if_string_map_nonempty(&mut request, "provider", std::mem::take(&mut provider));

    let geo_country = header(request_headers, "cf-ipcountry")
        .or_else(|| header(request_headers, "cloudfront-viewer-country"))
        .or_else(|| header(request_headers, "x-geo-country"));
    let geo_region = header(request_headers, "x-geo-region");
    let geo_city = header(request_headers, "x-geo-city");
    let geo_source = if request_headers.contains_key("cf-ipcountry") {
        Some("cloudflare")
    } else if request_headers.contains_key("cloudfront-viewer-country") {
        Some("cloudfront")
    } else if request_headers.contains_key("x-geo-country") {
        Some("upstream-proxy")
    } else {
        None
    };
    let mut geo = Map::new();
    insert_if_some(&mut geo, "country", geo_country);
    insert_if_some(&mut geo, "region", geo_region);
    insert_if_some(&mut geo, "city", geo_city);
    insert_if_some(&mut geo, "source", geo_source.map(str::to_string));
    insert_if_string_map_nonempty(&mut request, "geo", geo);

    // Keep only headers that materially help diagnose request routing or CORS.
    let diagnostic_headers = selected_headers(
        request_headers,
        &[
            "accept",
            "accept-language",
            "content-type",
            "access-control-request-method",
            "access-control-request-headers",
        ],
    );
    insert_if_string_map_nonempty(&mut request, "headers", diagnostic_headers);

    if !request.is_empty() {
        entry.insert("request".into(), Value::Object(request));
    }

    let mut response = Map::new();
    insert_if_some(&mut response, "content_length", header(response_headers, "content-length"));
    insert_if_some(&mut response, "content_type", header(response_headers, "content-type"));
    insert_if_some(&mut response, "x_correlation_id", header(response_headers, "x-correlation-id"));
    insert_if_some(&mut response, "server_timing", header(response_headers, "server-timing"));
    insert_if_string_map_nonempty(&mut entry, "response", response);

    // CORS is omitted entirely for ordinary requests with no CORS signal.
    if origin.is_some()
        || preflight
        || request_cors_method.is_some()
        || request_cors_headers.is_some()
        || response_cors_origin.is_some()
        || response_cors_methods.is_some()
        || response_cors_headers.is_some()
        || response_cors_credentials.is_some()
        || cors_blocked
    {
        let mut cors = Map::new();
        insert_if_some(&mut cors, "origin", origin);
        if preflight {
            cors.insert("preflight".into(), json!(true));
        }
        cors.insert("blocked".into(), json!(cors_blocked));
        insert_if_some(&mut cors, "request_method", request_cors_method);
        insert_if_some(&mut cors, "request_headers", request_cors_headers);
        insert_if_some(&mut cors, "allow_origin", response_cors_origin);
        insert_if_some(&mut cors, "allow_methods", response_cors_methods);
        insert_if_some(&mut cors, "allow_headers", response_cors_headers);
        insert_if_some(&mut cors, "allow_credentials", response_cors_credentials);
        entry.insert("cors".into(), Value::Object(cors));
    }

    // Preserve whether proxy-derived identity was trusted; useful during incident review.
    if config.security.trusted_proxy_headers {
        entry.insert("trusted_proxy_headers".into(), json!(true));
    }

    let path = PathBuf::from("./data/admin/request.queue.log");
    if let Some(parent) = path.parent() {
        if create_dir_all(parent).is_err() {
            return;
        }
    }

    let lock = REQUEST_AUDIT_LOCK.get_or_init(|| Mutex::new(()));
    let Ok(_guard) = lock.lock() else {
        return;
    };

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    if let Ok(line) = serde_json::to_string(&Value::Object(entry)) {
        let _ = writeln!(file, "{line}");
    }
}
