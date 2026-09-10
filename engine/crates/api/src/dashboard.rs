use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, State};
use axum::http::{
    header::{CACHE_CONTROL, CONTENT_TYPE},
    HeaderMap, HeaderValue, StatusCode,
};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde::Deserialize;
use serde_json::{json, Value};

mod admin_hash {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/admin_hash.rs"));
}

mod build_auth {
    include!(concat!(env!("OUT_DIR"), "/dashboard_auth.rs"));
}

pub const DASHBOARD_PORT: u16 = 5799;
const SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const LOGIN_FAILURE_LIMIT: u32 = 5;
const LOGIN_LOCKOUT: Duration = Duration::from_secs(30);
const MAX_SESSIONS: usize = 8;
const SESSION_HEADER: &str = "x-rbe-admin-session";
const CSRF_HEADER: &str = "x-rbe-admin-csrf";

#[derive(Clone)]
struct AdminSession {
    csrf: String,
    created_at: Instant,
    expires_at: Instant,
    expires_at_ms: u64,
}

#[derive(Default)]
struct AdminAuthState {
    sessions: HashMap<String, AdminSession>,
    failures: u32,
    blocked_until: Option<Instant>,
}

static AUTH_STATE: OnceLock<Mutex<AdminAuthState>> = OnceLock::new();
static SETTINGS_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard.css", get(dashboard_css))
        .route("/dashboard.js", get(dashboard_js))
        .route("/api/session", get(session_status))
        .route("/api/login", axum::routing::post(login))
        .route("/api/logout", axum::routing::post(logout))
        .route("/api/overview", get(overview))
        .route("/api/backend", get(backend))
        .route("/api/container", get(container))
        .route("/api/security", get(security))
        .route("/api/settings", get(settings).put(update_settings))
}

pub fn redirect_routes() -> Router<AppState> {
    Router::new().route("/dashboard", get(dashboard_redirect))
}

fn local_only(peer: SocketAddr) -> Option<Response> {
    if peer.ip().is_loopback() {
        None
    } else {
        Some(json_response(
            StatusCode::FORBIDDEN,
            json!({ "error": "RBE control room is loopback-only" }),
        ))
    }
}

fn json_response(status: StatusCode, value: Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn auth_state() -> &'static Mutex<AdminAuthState> {
    AUTH_STATE.get_or_init(|| Mutex::new(AdminAuthState::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn prune_sessions(state: &mut AdminAuthState) {
    let now = Instant::now();
    state.sessions.retain(|_, session| session.expires_at > now);
    if state.blocked_until.is_some_and(|deadline| deadline <= now) {
        state.blocked_until = None;
        state.failures = 0;
    }
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(SESSION_HEADER)?.to_str().ok().filter(|value| {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn authorized_session(headers: &HeaderMap) -> Option<AdminSession> {
    let token = session_token(headers)?;
    let mut state = auth_state().lock().ok()?;
    prune_sessions(&mut state);
    state.sessions.get(token).cloned()
}

fn require_auth(peer: SocketAddr, headers: &HeaderMap) -> Result<AdminSession, Response> {
    if let Some(response) = local_only(peer) {
        return Err(response);
    }
    if !build_auth::ADMIN_PASSWORD_CONFIGURED {
        return Err(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": "admin password is not configured in this backend build",
                "code": "ADMIN_PASSWORD_UNCONFIGURED"
            }),
        ));
    }
    authorized_session(headers).ok_or_else(|| {
        json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "admin authentication required" }),
        )
    })
}

fn require_mutation_auth(
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<AdminSession, Response> {
    let session = require_auth(peer, headers)?;
    let csrf = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !admin_hash::constant_time_eq(csrf.as_bytes(), session.csrf.as_bytes()) {
        return Err(json_response(
            StatusCode::FORBIDDEN,
            json!({ "error": "admin mutation token rejected" }),
        ));
    }
    Ok(session)
}

fn verify_password(password: &str) -> bool {
    if !build_auth::ADMIN_PASSWORD_CONFIGURED || password.len() > 1024 {
        return false;
    }
    let Some(salt) = admin_hash::hex_decode(build_auth::ADMIN_PASSWORD_SALT_HEX) else {
        return false;
    };
    let Some(expected) = admin_hash::hex_decode(build_auth::ADMIN_PASSWORD_VERIFIER_HEX) else {
        return false;
    };
    let actual = admin_hash::derive(password, &salt, build_auth::ADMIN_PASSWORD_ROUNDS);
    admin_hash::constant_time_eq(&actual, &expected)
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

async fn session_status(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    let session = authorized_session(&headers);
    json_response(
        StatusCode::OK,
        json!({
            "configured": build_auth::ADMIN_PASSWORD_CONFIGURED,
            "authenticated": session.is_some(),
            "expiresAtMs": session.as_ref().map(|value| value.expires_at_ms)
        }),
    )
}

async fn login(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<LoginRequest>,
) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    if !build_auth::ADMIN_PASSWORD_CONFIGURED {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": "This backend was built without an admin password. Rebuild with build.ps1/build.sh.",
                "code": "ADMIN_PASSWORD_UNCONFIGURED"
            }),
        );
    }

    {
        let mut state = match auth_state().lock() {
            Ok(state) => state,
            Err(_) => {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": "admin authentication state is unavailable" }),
                )
            }
        };
        prune_sessions(&mut state);
        if let Some(deadline) = state.blocked_until {
            let retry = deadline.saturating_duration_since(Instant::now()).as_secs().max(1);
            let mut response = json_response(
                StatusCode::TOO_MANY_REQUESTS,
                json!({ "error": "too many failed login attempts", "retryAfterSecs": retry }),
            );
            if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
            return response;
        }
    }

    let password = request.password;
    let verified = tokio::task::spawn_blocking(move || verify_password(&password))
        .await
        .unwrap_or(false);
    if !verified {
        let mut state = auth_state().lock().unwrap_or_else(|error| error.into_inner());
        state.failures = state.failures.saturating_add(1);
        if state.failures >= LOGIN_FAILURE_LIMIT {
            state.blocked_until = Some(Instant::now() + LOGIN_LOCKOUT);
        }
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "incorrect admin password" }),
        );
    }

    let token = service_runtime::new_service_mother_token();
    let csrf = service_runtime::new_service_mother_token();
    let expires_at = Instant::now() + SESSION_TTL;
    let expires_at_ms = now_ms().saturating_add(SESSION_TTL.as_millis() as u64);
    let session = AdminSession {
        csrf: csrf.clone(),
        created_at: Instant::now(),
        expires_at,
        expires_at_ms,
    };

    let mut state = auth_state().lock().unwrap_or_else(|error| error.into_inner());
    prune_sessions(&mut state);
    state.failures = 0;
    state.blocked_until = None;
    if state.sessions.len() >= MAX_SESSIONS {
        if let Some(oldest) = state
            .sessions
            .iter()
            .min_by_key(|(_, value)| value.created_at)
            .map(|(token, _)| token.clone())
        {
            state.sessions.remove(&oldest);
        }
    }
    state.sessions.insert(token.clone(), session);

    json_response(
        StatusCode::OK,
        json!({
            "session": token,
            "csrf": csrf,
            "expiresAtMs": expires_at_ms
        }),
    )
}

async fn logout(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_mutation_auth(peer, &headers) {
        return response;
    }
    if let Some(token) = session_token(&headers) {
        if let Ok(mut state) = auth_state().lock() {
            state.sessions.remove(token);
        }
    }
    json_response(StatusCode::OK, json!({ "ok": true }))
}

async fn dashboard_redirect(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    let prefix = state
        .config
        .dashboards
        .admin_path_prefix
        .trim_end_matches('/');
    Redirect::temporary(&format!(
        "http://127.0.0.1:{DASHBOARD_PORT}{prefix}/dashboard"
    ))
    .into_response()
}

async fn dashboard_html(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    let mut response = Html(include_str!("dashboard.html")).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn dashboard_css(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        include_str!("dashboard.css"),
    )
        .into_response()
}

async fn dashboard_js(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(response) = local_only(peer) {
        return response;
    }
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        include_str!("dashboard.js"),
    )
        .into_response()
}

async fn overview(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(peer, &headers) {
        return response;
    }
    let metrics = state.backend_metrics.snapshot();
    let maintenance = state.maintenance.snapshot();
    let endpoint = state.container.endpoint_snapshot();
    let container_health = state
        .container
        .health()
        .await
        .unwrap_or_else(|error| json!({ "ok": false, "error": error.to_string() }));
    json_response(
        StatusCode::OK,
        json!({
            "backend": {
                "pid": std::process::id(),
                "state": format!("{:?}", state.backend_state()),
                "uptime_secs": metrics.uptime_secs,
                "total_requests": metrics.total_requests,
                "active_requests": metrics.active_requests,
                "average_latency_ms": metrics.average_latency_ms,
                "responses": { "2xx": metrics.responses_2xx, "3xx": metrics.responses_3xx, "4xx": metrics.responses_4xx, "5xx": metrics.responses_5xx }
            },
            "container": {
                "pid": endpoint.pid,
                "control_address": endpoint.address.to_string(),
                "generation": endpoint.generation,
                "health": container_health
            },
            "security": {
                "banned_ips": state.ip_strikes.ban_snapshots().len(),
                "active_strike_buckets": state.ip_strikes.strike_snapshots().len()
            },
            "maintenance": {
                "refresh_interval_hours": maintenance.refresh_interval_hours,
                "container_refreshes": maintenance.container_refreshes,
                "vault_refreshes": maintenance.vault_refreshes,
                "error_reporter_refreshes": maintenance.error_reporter_refreshes,
                "last_container_refresh_ms": maintenance.last_container_refresh_ms,
                "last_vault_refresh_ms": maintenance.last_vault_refresh_ms,
                "last_error_reporter_refresh_ms": maintenance.last_error_reporter_refresh_ms
            }
        }),
    )
}

async fn backend(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(peer, &headers) {
        return response;
    }
    let metrics = state.backend_metrics.snapshot();
    json_response(
        StatusCode::OK,
        json!({
            "pid": std::process::id(),
            "state": format!("{:?}", state.backend_state()),
            "uptime_secs": metrics.uptime_secs,
            "requests": {
                "total": metrics.total_requests,
                "active": metrics.active_requests,
                "average_latency_ms": metrics.average_latency_ms,
                "2xx": metrics.responses_2xx,
                "3xx": metrics.responses_3xx,
                "4xx": metrics.responses_4xx,
                "5xx": metrics.responses_5xx
            },
            "api": {
                "host": state.config.api.host,
                "port": state.config.api.port,
                "request_timeout_ms": state.config.api.request_timeout_ms,
                "max_body_size_bytes": state.config.api.max_body_size_bytes
            },
            "runtime": {
                "environment": state.config.runtime.environment,
                "worker_threads": state.config.runtime.worker_threads,
                "process_refresh_hours": state.config.runtime.process_refresh_hours
            }
        }),
    )
}

async fn container(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(peer, &headers) {
        return response;
    }
    let endpoint = state.container.endpoint_snapshot();
    match state.container.inspect().await {
        Ok(body) => json_response(
            StatusCode::OK,
            json!({
                "online": true,
                "pid": endpoint.pid,
                "generation": endpoint.generation,
                "control_address": endpoint.address.to_string(),
                "state": body
            }),
        ),
        Err(error) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "online": false,
                "pid": endpoint.pid,
                "generation": endpoint.generation,
                "error": error.to_string()
            }),
        ),
    }
}

async fn security(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(peer, &headers) {
        return response;
    }
    let bans = state
        .ip_strikes
        .ban_snapshots()
        .into_iter()
        .map(|entry| {
            json!({
                "ip": entry.ip,
                "age_secs": entry.age_secs,
                "remaining_secs": entry.remaining_secs
            })
        })
        .collect::<Vec<_>>();
    let strikes = state
        .ip_strikes
        .strike_snapshots()
        .into_iter()
        .map(|entry| {
            json!({
                "ip": entry.ip,
                "category": entry.category,
                "count": entry.count,
                "age_secs": entry.age_secs,
                "remaining_window_secs": entry.remaining_window_secs
            })
        })
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        json!({
            "banned_ips": bans,
            "strikes": strikes,
            "policy": {
                "strike_threshold": state.config.security.ip_ban.strike_threshold,
                "strike_window_secs": state.config.security.ip_ban.strike_window_secs,
                "ban_duration_secs": state.config.security.ip_ban.ban_duration_secs,
                "trusted_proxy_headers": state.config.security.trusted_proxy_headers,
                "global_rate_limit": {
                    "window_secs": state.config.security.global_rate_limit.window_secs,
                    "max_requests": state.config.security.global_rate_limit.max_requests
                },
                "api_rate_limit": {
                    "window_secs": state.config.security.api_rate_limit.window_secs,
                    "max_requests": state.config.security.api_rate_limit.max_requests
                }
            }
        }),
    )
}

fn settings_path() -> PathBuf {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(value) = args
        .windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| pair[1].clone())
    {
        return PathBuf::from(value);
    }
    if args.iter().any(|arg| arg == "--allow-settings-env") {
        if let Ok(value) = std::env::var("SETTINGS_PATH") {
            return PathBuf::from(value);
        }
    }
    PathBuf::from("settings.json")
}

fn read_settings_document(path: &Path) -> Result<(Value, String, Vec<u8>), String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let document = serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let revision = admin_hash::hex_encode(&admin_hash::sha256(&bytes));
    Ok((document, revision, bytes))
}

async fn settings(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(peer, &headers) {
        return response;
    }
    let path = settings_path();
    match read_settings_document(&path) {
        Ok((document, revision, _)) => {
            let endpoint = state.container.endpoint_snapshot();
            json_response(
                StatusCode::OK,
                json!({
                    "document": document,
                    "revision": revision,
                    "source": path.file_name().and_then(|value| value.to_str()).unwrap_or("settings.json"),
                    "runtime": {
                        "containerGeneration": endpoint.generation,
                        "note": "Changes are validated and persisted immediately. Runtime-owned values are applied on the next process refresh/restart unless a subsystem exposes live reconfiguration."
                    }
                }),
            )
        }
        Err(error) => json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error })),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateSettingsRequest {
    revision: String,
    document: Value,
}

async fn update_settings(
    State(_state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<UpdateSettingsRequest>,
) -> Response {
    if let Err(response) = require_mutation_auth(peer, &headers) {
        return response;
    }

    let _guard = SETTINGS_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let path = settings_path();
    let (current_document, current_revision, current_bytes) = match read_settings_document(&path) {
        Ok(value) => value,
        Err(error) => {
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }))
        }
    };

    if request.revision != current_revision {
        return json_response(
            StatusCode::CONFLICT,
            json!({
                "error": "settings changed since this editor loaded them",
                "code": "SETTINGS_REVISION_CONFLICT",
                "revision": current_revision
            }),
        );
    }

    let mut encoded = match serde_json::to_vec_pretty(&request.document) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("serialize settings: {error}") }),
            )
        }
    };
    encoded.push(b'\n');

    let temp = temporary_settings_path(&path);
    if let Err(error) = write_candidate(&path, &temp, &encoded) {
        let _ = fs::remove_file(&temp);
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("stage settings update: {error}") }),
        );
    }

    if let Err(error) = core_lib::validate_settings_file(&temp) {
        let _ = fs::remove_file(&temp);
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "error": error,
                "code": "SETTINGS_VALIDATION_FAILED"
            }),
        );
    }

    if let Err(error) = replace_settings_file(&path, &temp) {
        let _ = fs::remove_file(&temp);
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("commit settings update: {error}") }),
        );
    }

    let revision = admin_hash::hex_encode(&admin_hash::sha256(&encoded));
    let mut changed = Vec::new();
    collect_changed_paths(&current_document, &request.document, "", &mut changed);
    changed.sort();
    changed.dedup();
    if changed.len() > 64 {
        changed.truncate(64);
    }

    // Keep `current_bytes` alive until after replacement so the old revision was
    // computed from exactly the bytes we guarded above.
    drop(current_bytes);

    let restart_required = !changed.is_empty();
    let message = if restart_required {
        "Saved and validated. Changed runtime-owned values take effect on the next process refresh/restart."
    } else {
        "No configuration values changed."
    };
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "revision": revision,
            "changedPaths": changed.clone(),
            "restartRequired": restart_required,
            "pendingRestartPaths": changed,
            "message": message
        }),
    )
}

fn temporary_settings_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce))
}

fn write_candidate(original: &Path, temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(temp)?;
    if let Ok(metadata) = fs::metadata(original) {
        let _ = file.set_permissions(metadata.permissions());
    }
    file.write_all(bytes)?;
    file.sync_all()
}

fn replace_settings_file(path: &Path, temp: &Path) -> std::io::Result<()> {
    match fs::rename(temp, path) {
        Ok(()) => return Ok(()),
        Err(_first_error) if path.exists() => {
            let backup = path.with_extension(format!(
                "rbe-admin-backup-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            fs::rename(path, &backup)?;
            match fs::rename(temp, path) {
                Ok(()) => {
                    let _ = fs::remove_file(backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::rename(&backup, path);
                    Err(error)
                }
            }
        }
        Err(error) => Err(error),
    }
}

fn collect_changed_paths(old: &Value, new: &Value, path: &str, out: &mut Vec<String>) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys = left.keys().chain(right.keys()).collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            for key in keys {
                let next = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => collect_changed_paths(left, right, &next, out),
                    _ => out.push(next),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            let count = left.len().max(right.len());
            for index in 0..count {
                let next = format!("{path}[{index}]");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => collect_changed_paths(left, right, &next, out),
                    _ => out.push(next),
                }
            }
        }
        _ => out.push(if path.is_empty() {
            "<root>".to_string()
        } else {
            path.to_string()
        }),
    }
}

#[allow(dead_code)]
fn _is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_paths_are_precise_for_nested_arrays() {
        let old = json!({"runtimeEnv":{"groups":[{"name":"a"}]}});
        let new = json!({"runtimeEnv":{"groups":[{"name":"b"},{"name":"c"}]}});
        let mut changed = Vec::new();
        collect_changed_paths(&old, &new, "", &mut changed);
        assert!(changed.contains(&"runtimeEnv.groups[0].name".to_string()));
        assert!(changed.contains(&"runtimeEnv.groups[1]".to_string()));
    }

    #[test]
    fn generated_verifier_is_well_formed_when_configured() {
        if build_auth::ADMIN_PASSWORD_CONFIGURED {
            assert_eq!(build_auth::ADMIN_PASSWORD_SALT_HEX.len(), 16);
            assert_eq!(build_auth::ADMIN_PASSWORD_VERIFIER_HEX.len(), 64);
            assert!(build_auth::ADMIN_PASSWORD_ROUNDS >= 1);
        }
    }
}
