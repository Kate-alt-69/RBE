//! Per-request timing and smart structured request auditing.
//!
//! The terminal log stays compact. `request.queue.log` is a deliberately
//! smaller metadata stream: useful request identity, timing, client IP,
//! and only the proxy/geo/CORS metadata that actually exists.
//!
//! Repeated 404s are deduplicated so a bot probing thousands of missing
//! paths cannot turn the audit file into a storage bomb. The first miss
//! for a `(client, method, path)` bucket is recorded immediately; repeats
//! inside the suppression window are counted in memory and emitted as a
//! compact summary when the bucket becomes eligible again.

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

const NOT_FOUND_SUPPRESSION: Duration = Duration::from_secs(10);
const NOT_FOUND_STATE_TTL: Duration = Duration::from_secs(60);
const MAX_NOT_FOUND_BUCKETS: usize = 16_384;

static REQUEST_AUDIT_IO: OnceLock<AtomicIo> = OnceLock::new();
static NOT_FOUND_STATE: OnceLock<Mutex<HashMap<NotFoundKey, NotFoundBucket>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NotFoundKey {
    client_ip: String,
    method: String,
    path: String,
}

#[derive(Debug)]
struct NotFoundBucket {
    last_logged: Instant,
    suppressed: u64,
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

    let should_audit = status != 404 || should_log_not_found(&client_ip.to_string(), method.as_str(), &path);

    if should_audit {
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

fn should_log_not_found(client_ip: &str, method: &str, path: &str) -> bool {
    let state = NOT_FOUND_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = NotFoundKey {
        client_ip: client_ip.to_string(),
        method: method.to_string(),
        path: path.to_string(),
    };

    let now = Instant::now();
    let Ok(mut buckets) = state.lock() else {
        return true;
    };

    buckets.retain(|_, bucket| now.duration_since(bucket.last_logged) <= NOT_FOUND_STATE_TTL);

    if buckets.len() >= MAX_NOT_FOUND_BUCKETS && !buckets.contains_key(&key) {
        // Bound memory during a path-randomizing scan. The durable audit
        // file is the important artifact; the suppression cache is only
        // a traffic-control optimization.
        buckets.clear();
    }

    match buckets.get_mut(&key) {
        None => {
            buckets.insert(
                key,
                NotFoundBucket {
                    last_logged: now,
                    suppressed: 0,
                },
            );
            true
        }
        Some(bucket) if now.duration_since(bucket.last_logged) < NOT_FOUND_SUPPRESSION => {
            bucket.suppressed = bucket.suppressed.saturating_add(1);
            false
        }
        Some(bucket) => {
            let suppressed = bucket.suppressed;
            bucket.last_logged = now;
            bucket.suppressed = 0;

            if suppressed > 0 {
                // Emit the summary in a separate compact record before the
                // next 404 detail record. Returning false here would lose
                // the summary, so write it directly while we have the key.
                let summary = json!({
                    "ts_ms": now_ms(),
                    "kind": "404_summary",
                    "method": method,
                    "path": path,
                    "status": 404,
                    "client_ip": client_ip,
                    "suppressed": suppressed,
                    "window_ms": NOT_FOUND_SUPPRESSION.as_millis() as u64,
                });
                append_audit_value(&summary);
            }

            true
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
    entry.insert("client_ip".into(), json!(client_ip));

    // `client_ip` is the actual remote peer identity after trusted-proxy
    // extraction. A local browser talking to a local server will correctly
    // appear as 127.0.0.1/::1; that is not the server's LAN address.
    if peer.ip().to_string() != entry["client_ip"].as_str().unwrap_or_default() {
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

    // 404s intentionally stay compact; the hot path above suppresses
    // repeated identical misses. Other statuses keep the useful request
    // metadata, with no raw headers/body/cookies/authorization material.
    if status == 404 {
        let compact = json!({
            "ts_ms": entry.remove("ts_ms").unwrap_or_default(),
            "kind": "404",
            "method": method,
            "path": path,
            "status": 404,
            "duration_ms": format!("{duration_ms:.3}"),
            "client_ip": client_ip,
            "host": entry.remove("host").unwrap_or(Value::Null),
            "user_agent": entry.remove("user_agent").unwrap_or(Value::Null),
        });
        append_audit_value(&compact);
        return;
    }

    if config.security.trusted_proxy_headers {
        entry.insert("trusted_proxy_headers".into(), json!(true));
    }

    append_audit_value(&Value::Object(entry));
}

fn append_audit_value(value: &Value) {
    let line = match serde_json::to_vec(value) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(_) => return,
    };

    let path = std::path::Path::new("./data/admin/request.queue.log");
    let io = REQUEST_AUDIT_IO.get_or_init(AtomicIo::new);
    let _ = io.append_locked(path, &line);
}
