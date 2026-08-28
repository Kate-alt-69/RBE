use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard_html))
        .route("/api/overview", get(overview))
        .route("/api/backend", get(backend))
        .route("/api/container", get(container))
        .route("/api/security", get(security))
        .route("/api/settings", get(settings))
}

fn local_only(peer: SocketAddr) -> Result<(), (StatusCode, &'static str)> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "RBE dashboard is loopback-only"))
    }
}

async fn dashboard_html(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
    }
    Html(DASHBOARD_HTML).into_response()
}

async fn overview(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
    }
    let metrics = state.backend_metrics.snapshot();
    let maintenance = state.maintenance.snapshot();
    let endpoint = state.container.endpoint_snapshot();
    let container_health = state
        .container
        .health()
        .await
        .unwrap_or_else(|error| json!({ "ok": false, "error": error.to_string() }));
    Json(json!({
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
    })).into_response()
}

async fn backend(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
    }
    let metrics = state.backend_metrics.snapshot();
    Json(json!({
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
    }))
    .into_response()
}

async fn container(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
    }
    let endpoint = state.container.endpoint_snapshot();
    match state.container.inspect().await {
        Ok(body) => Json(json!({
            "online": true,
            "pid": endpoint.pid,
            "generation": endpoint.generation,
            "control_address": endpoint.address.to_string(),
            "state": body
        }))
        .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "online": false,
                "pid": endpoint.pid,
                "generation": endpoint.generation,
                "error": error.to_string()
            })),
        )
            .into_response(),
    }
}

async fn security(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
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
    Json(json!({
        "banned_ips": bans,
        "strikes": strikes,
        "policy": {
            "strike_threshold": state.config.security.ip_ban.strike_threshold,
            "strike_window_secs": state.config.security.ip_ban.strike_window_secs,
            "ban_duration_secs": state.config.security.ip_ban.ban_duration_secs,
            "trusted_proxy_headers": state.config.security.trusted_proxy_headers,
            "global_rate_limit": { "window_secs": state.config.security.global_rate_limit.window_secs, "max_requests": state.config.security.global_rate_limit.max_requests },
            "api_rate_limit": { "window_secs": state.config.security.api_rate_limit.window_secs, "max_requests": state.config.security.api_rate_limit.max_requests }
        }
    })).into_response()
}

async fn settings(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(error) = local_only(peer) {
        return error.into_response();
    }
    let endpoint = state.container.endpoint_snapshot();
    let resolved = state
        .container
        .inspect()
        .await
        .ok()
        .and_then(|value| value.get("config").cloned())
        .unwrap_or(Value::Null);
    Json(json!({
        "containers": {
            "general_environments": state.config.containers.environments,
            "payment_environment": "always-on",
            "swamps_per_environment": state.config.containers.swamps_per_environment.label(),
            "workers_per_swamp": state.config.containers.workers_per_swamp.label(),
            "resolved": resolved,
            "container_generation": endpoint.generation,
            "warm_pool_size": state.config.containers.warm_pool_size,
            "max_concurrent": state.config.containers.max_concurrent,
            "default_timeout_ms": state.config.containers.default_timeout_ms,
            "memory_limit_mb": state.config.containers.memory_limit_mb
        },
        "dashboards": {
            "enabled": state.config.dashboards.enabled,
            "auto_open": state.config.dashboards.auto_open,
            "admin_path_prefix": state.config.dashboards.admin_path_prefix
        },
        "maintenance": { "process_refresh_hours": state.config.runtime.process_refresh_hours }
    }))
    .into_response()
}

#[allow(dead_code)]
fn _is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>RBE Control Room</title><style>
:root{color-scheme:dark;font:14px Inter,system-ui,sans-serif;background:#090d16;color:#e9eef8}*{box-sizing:border-box}body{margin:0;background:#090d16}.top{position:sticky;top:0;z-index:5;background:#101725;border-bottom:1px solid #26334b;padding:18px 24px}.top h1{margin:0;font-size:22px}.muted{color:#91a0b8}.layout{display:grid;grid-template-columns:190px 1fr;min-height:calc(100vh - 73px)}nav{padding:18px 12px;border-right:1px solid #26334b;background:#0d1320}button{display:block;width:100%;text-align:left;margin:4px 0;padding:10px 12px;border:1px solid transparent;border-radius:8px;background:transparent;color:#b9c6d9;cursor:pointer}button.active,button:hover{background:#172238;border-color:#2b3d60;color:white}main{padding:22px;min-width:0}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}.card,.panel{background:#111a2a;border:1px solid #253551;border-radius:11px;padding:15px}.card .v{font-size:28px;font-weight:700;margin-top:5px}.panel{margin-top:14px}.ok{color:#69dbb7}.bad{color:#ff8787}.warn{color:#ffd66b}pre{white-space:pre-wrap;overflow:auto;max-height:65vh;margin:0;font:12px ui-monospace,SFMono-Regular,Consolas,monospace}.tab{display:none}.tab.active{display:block}table{width:100%;border-collapse:collapse}td,th{padding:8px;border-bottom:1px solid #24314a;text-align:left}th{color:#91a0b8}@media(max-width:760px){.layout{grid-template-columns:1fr}nav{display:flex;overflow:auto;border-right:0;border-bottom:1px solid #26334b}button{min-width:max-content}.top{position:static}}
</style></head><body><div class="top"><h1>RBE Control Room</h1><div class="muted">backend.exe · live backend + container telemetry · loopback only</div></div><div class="layout"><nav>
<button class="active" data-tab="overview">Overview</button><button data-tab="backend">Backend</button><button data-tab="container">Container</button><button data-tab="security">Security</button><button data-tab="settings">Settings</button></nav><main>
<section id="overview" class="tab active"><div id="cards" class="grid"></div><div class="panel"><h3>Maintenance</h3><pre id="maintenance"></pre></div></section>
<section id="backend" class="tab"><div class="panel"><h3>Backend</h3><pre id="backend-json"></pre></div></section>
<section id="container" class="tab"><div class="panel"><h3>Environments / Swamps / Workers / Cache</h3><pre id="container-json"></pre></div></section>
<section id="security" class="tab"><div class="panel"><h3>Bans / Strikes / Rate Limits</h3><pre id="security-json"></pre></div></section>
<section id="settings" class="tab"><div class="panel"><h3>Configured vs Resolved</h3><pre id="settings-json"></pre></div></section>
</main></div><script>
const base=location.pathname.replace(/\/dashboard\/?$/,'');let tab='overview',timer;
const esc=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
async function api(name){const r=await fetch(base+'/api/'+name,{cache:'no-store'});const j=await r.json();if(!r.ok)throw Error(JSON.stringify(j));return j}
function card(label,value,cls=''){return `<div class="card"><div class="muted">${esc(label)}</div><div class="v ${cls}">${esc(value)}</div></div>`}
async function refresh(){try{if(tab==='overview'){const d=await api('overview'),b=d.backend,c=d.container,h=c.health||{};document.querySelector('#cards').innerHTML=card('Backend PID',b.pid)+card('Uptime',Math.floor(b.uptime_secs/3600)+'h')+card('Requests',b.total_requests)+card('Active',b.active_requests)+card('Avg latency',Number(b.average_latency_ms).toFixed(2)+'ms')+card('Container PID',c.pid??'-',h.error?'bad':'ok')+card('Container generation',c.generation)+card('Banned IPs',d.security.banned_ips,d.security.banned_ips?'bad':'ok');document.querySelector('#maintenance').textContent=JSON.stringify(d.maintenance,null,2)}else{const d=await api(tab);document.querySelector('#'+tab+'-json').textContent=JSON.stringify(d,null,2)}}catch(e){console.error(e)}}
document.querySelectorAll('nav button').forEach(b=>b.onclick=()=>{document.querySelectorAll('nav button').forEach(x=>x.classList.remove('active'));document.querySelectorAll('.tab').forEach(x=>x.classList.remove('active'));b.classList.add('active');tab=b.dataset.tab;document.querySelector('#'+tab).classList.add('active');refresh()});
async function tick(){if(!document.hidden)await refresh();timer=setTimeout(tick,1000)}document.addEventListener('visibilitychange',()=>{if(!document.hidden)refresh()});tick();
</script></body></html>"#;
