//! Per-request timing and verbose structured logging. The normal terminal
//! line intentionally stays compact. A second, local-only request audit
//! queue receives richer metadata for later inspection without dumping
//! sensitive request contents into the terminal.

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
use serde_json::{json, Value};

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
    headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string)
}

fn header_list(headers: &HeaderMap, names: &[&str]) -> serde_json::Map<String, Value> {
    let mut result = serde_json::Map::new();
    for name in names {
        if let Some(value) = header(headers, name) {
            result.insert((*name).to_string(), Value::String(value));
        }
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

    let forwarded_chain = json!({
        "forwarded": header(request_headers, "forwarded"),
        "x_forwarded_for": header(request_headers, "x-forwarded-for"),
        "via": header(request_headers, "via"),
        "x_forwarded_host": header(request_headers, "x-forwarded-host"),
        "x_forwarded_proto": header(request_headers, "x-forwarded-proto"),
        "x_forwarded_port": header(request_headers, "x-forwarded-port"),
        "cf_ray": header(request_headers, "cf-ray"),
    });

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

    let request_meta = json!({
        "method": method,
        "path": path,
        "query": query,
        "peer_ip": peer.ip().to_string(),
        "client_ip": client_ip,
        "user_agent": header(request_headers, "user-agent"),
        "referer": header(request_headers, "referer"),
        "host": header(request_headers, "host"),
        "accept": header(request_headers, "accept"),
        "accept_language": header(request_headers, "accept-language"),
        "content_type": header(request_headers, "content-type"),
        "origin": origin,
        "forwarded_chain": forwarded_chain,
        "geo": {
            "country": geo_country,
            "region": geo_region,
            "city": geo_city,
            "source": geo_source,
        },
        "provider_identity": header_list(
            request_headers,
            &[
                "cf-connecting-ip",
                "cf-connecting-ipv6",
                "true-client-ip",
                "x-real-ip",
                "x-azure-clientip",
                "fastly-client-ip",
            ],
        ),
        "request_headers": header_list(
            request_headers,
            &[
                "forwarded",
                "x-forwarded-for",
                "x-forwarded-host",
                "x-forwarded-proto",
                "x-forwarded-port",
                "via",
                "cf-ray",
                "cf-visitor",
                "cf-ipcountry",
                "origin",
                "referer",
                "user-agent",
                "accept",
                "accept-language",
                "content-type",
                "access-control-request-method",
                "access-control-request-headers",
            ],
        ),
    });

    let cors = json!({
        "origin": origin,
        "preflight": preflight,
        "blocked": cors_blocked,
        "request_method": request_cors_method,
        "request_headers": request_cors_headers,
        "response_allow_origin": response_cors_origin,
        "response_allow_methods": response_cors_methods,
        "response_allow_headers": response_cors_headers,
        "response_allow_credentials": response_cors_credentials,
    });

    let response_meta = json!({
        "content_length": header(response_headers, "content-length"),
        "content_type": header(response_headers, "content-type"),
        "cache_control": header(response_headers, "cache-control"),
        "server": header(response_headers, "server"),
        "x_correlation_id": header(response_headers, "x-correlation-id"),
        "server_timing": header(response_headers, "server-timing"),
    });

    let entry = json!({
        "ts_ms": now_ms,
        "method": method,
        "path": path,
        "query": query,
        "status": status,
        "duration_ms": format!("{duration_ms:.3}"),
        "request": request_meta,
        "response": response_meta,
        "cors": cors,
        "trusted_proxy_headers": config.security.trusted_proxy_headers,
    });

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

    if let Ok(line) = serde_json::to_string(&entry) {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}
