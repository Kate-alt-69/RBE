use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, State};
use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_lib::AppState;
use serde_json::{json, Value};

pub const DASHBOARD_PORT: u16 = 5799;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard.css", get(dashboard_css))
        .route("/api/overview", get(overview))
        .route("/api/backend", get(backend))
        .route("/api/container", get(container))
        .route("/api/security", get(security))
        .route("/api/settings", get(settings))
}

pub fn redirect_routes() -> Router<AppState> {
    Router::new().route("/dashboard", get(dashboard_redirect))
}

fn local_only(peer: SocketAddr) -> Result<(), Response> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "RBE dashboard is loopback-only").into_response())
    }
}

async fn dashboard_redirect(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    let prefix = state.config.dashboards.admin_path_prefix.trim_end_matches('/');
    Redirect::temporary(&format!("http://127.0.0.1:{DASHBOARD_PORT}{prefix}/dashboard")).into_response()
}

async fn dashboard_html(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    Html(DASHBOARD_HTML).into_response()
}

async fn dashboard_css(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], DASHBOARD_CSS).into_response()
}

async fn overview(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    let metrics = state.backend_metrics.snapshot();
    let maintenance = state.maintenance.snapshot();
    let endpoint = state.container.endpoint_snapshot();
    let container_health = state.container.health().await.unwrap_or_else(|error| json!({ "ok": false, "error": error.to_string() }));
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

async fn backend(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
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
    })).into_response()
}

async fn container(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    let endpoint = state.container.endpoint_snapshot();
    match state.container.inspect().await {
        Ok(body) => Json(json!({
            "online": true,
            "pid": endpoint.pid,
            "generation": endpoint.generation,
            "control_address": endpoint.address.to_string(),
            "state": body
        })).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
            "online": false,
            "pid": endpoint.pid,
            "generation": endpoint.generation,
            "error": error.to_string()
        }))).into_response(),
    }
}

async fn security(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    let bans = state.ip_strikes.ban_snapshots().into_iter().map(|entry| json!({
        "ip": entry.ip,
        "age_secs": entry.age_secs,
        "remaining_secs": entry.remaining_secs
    })).collect::<Vec<_>>();
    let strikes = state.ip_strikes.strike_snapshots().into_iter().map(|entry| json!({
        "ip": entry.ip,
        "category": entry.category,
        "count": entry.count,
        "age_secs": entry.age_secs,
        "remaining_window_secs": entry.remaining_window_secs
    })).collect::<Vec<_>>();
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

async fn settings(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Err(response) = local_only(peer) { return response; }
    let endpoint = state.container.endpoint_snapshot();
    let resolved = state.container.inspect().await.ok().and_then(|value| value.get("config").cloned()).unwrap_or(Value::Null);
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
            "admin_path_prefix": state.config.dashboards.admin_path_prefix,
            "host": "127.0.0.1",
            "port": DASHBOARD_PORT
        },
        "maintenance": { "process_refresh_hours": state.config.runtime.process_refresh_hours }
    })).into_response()
}

#[allow(dead_code)]
fn _is_loopback(ip: IpAddr) -> bool { ip.is_loopback() }

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="theme-color" content="#080b12">
<title>RBE Control Room</title>
<link rel="stylesheet" href="dashboard.css">
</head>
<body>
<div class="app-shell">
    <aside class="sidebar">
        <div class="brand">
            <div class="brand-mark"><span></span><span></span><span></span></div>
            <div class="brand-copy"><strong>RBE</strong><small>CONTROL ROOM</small></div>
        </div>

        <div class="nav-label">MONITOR</div>
        <nav aria-label="Dashboard sections">
            <button class="nav-item active" data-tab="overview"><span class="nav-icon">OV</span><span>Overview</span></button>
            <button class="nav-item" data-tab="backend"><span class="nav-icon">BE</span><span>Backend</span></button>
            <button class="nav-item" data-tab="container"><span class="nav-icon">CT</span><span>Container</span></button>
            <button class="nav-item" data-tab="security"><span class="nav-icon">SC</span><span>Security</span></button>
            <button class="nav-item" data-tab="settings"><span class="nav-icon">ST</span><span>Settings</span></button>
        </nav>

        <div class="sidebar-footer">
            <div class="local-pill"><i></i><span>Loopback only</span></div>
            <small>127.0.0.1:5799</small>
        </div>
    </aside>

    <div class="workspace">
        <header class="topbar">
            <div>
                <div class="eyebrow">RUNTIME TELEMETRY</div>
                <h1 id="page-title">Overview</h1>
            </div>
            <div class="live-state"><i></i><span>LIVE</span><small id="last-refresh">waiting…</small></div>
        </header>

        <main>
            <section id="overview" class="tab active"><div id="overview-view" class="loading">Loading telemetry…</div></section>
            <section id="backend" class="tab"><div id="backend-view" class="loading">Loading backend…</div></section>
            <section id="container" class="tab"><div id="container-view" class="loading">Loading container…</div></section>
            <section id="security" class="tab"><div id="security-view" class="loading">Loading security…</div></section>
            <section id="settings" class="tab"><div id="settings-view" class="loading">Loading settings…</div></section>
        </main>
    </div>
</div>
<script>
const base=location.pathname.replace(/\/dashboard\/?$/,'');
let tab='overview';
const $=q=>document.querySelector(q);
const $$=q=>document.querySelectorAll(q);
const esc=v=>String(v??'—').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const title=v=>String(v).replace(/_/g,' ').replace(/\b\w/g,c=>c.toUpperCase());
const num=v=>Number(v||0).toLocaleString();
const ms=v=>`${Number(v||0).toFixed(2)} ms`;
const bytes=v=>{const n=Number(v||0);if(n<1024)return `${n} B`;if(n<1048576)return `${(n/1024).toFixed(1)} KB`;return `${(n/1048576).toFixed(1)} MB`};
const duration=v=>{const s=Math.max(0,Number(v||0)),d=Math.floor(s/86400),h=Math.floor(s%86400/3600),m=Math.floor(s%3600/60),sec=Math.floor(s%60);return d?`${d}d ${h}h`:h?`${h}h ${m}m`:m?`${m}m ${sec}s`:`${sec}s`};

async function api(name){
    const response=await fetch(base+'/api/'+name,{cache:'no-store'});
    const data=await response.json();
    if(!response.ok)throw Error(data.error||JSON.stringify(data));
    return data;
}

function status(label,tone='good'){
    return `<span class="status ${tone}"><i></i>${esc(label)}</span>`;
}

function metric(label,value,meta='',tone='',glyph='•'){
    return `<article class="metric ${tone}"><div class="metric-top"><span class="metric-icon">${esc(glyph)}</span><span class="metric-label">${esc(label)}</span></div><div class="metric-value">${esc(value)}</div>${meta?`<div class="metric-meta">${esc(meta)}</div>`:''}</article>`;
}

function panel(name,subtitle,body,side=''){
    return `<section class="panel"><div class="panel-head"><div><h2>${esc(name)}</h2>${subtitle?`<p>${esc(subtitle)}</p>`:''}</div>${side}</div>${body}</section>`;
}

function valueText(value){
    if(Array.isArray(value))return value.length?value.join(', '):'None';
    if(value&&typeof value==='object')return JSON.stringify(value);
    if(typeof value==='boolean')return value?'Enabled':'Disabled';
    return value??'—';
}

function kvRows(object){
    if(!object||!Object.keys(object).length)return '<div class="empty">No data available.</div>';
    return `<div class="kv">${Object.entries(object).map(([key,value])=>`<div class="kv-row"><span>${esc(title(key))}</span><strong>${esc(valueText(value))}</strong></div>`).join('')}</div>`;
}

function raw(data){
    return `<details class="raw"><summary>Raw payload</summary><pre>${esc(JSON.stringify(data,null,2))}</pre></details>`;
}

function table(headers,rows){
    if(!rows.length)return '<div class="empty">Nothing here. Nice.</div>';
    return `<div class="table-wrap"><table><thead><tr>${headers.map(h=>`<th>${esc(title(h))}</th>`).join('')}</tr></thead><tbody>${rows.map(row=>`<tr>${headers.map(h=>`<td>${esc(valueText(row[h]))}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}

function errorView(error){
    return `<div class="error-state"><div class="error-badge">!</div><div><strong>Telemetry request failed</strong><p>${esc(error.message||error)}</p></div></div>`;
}

async function renderOverview(){
    const d=await api('overview'),b=d.backend,c=d.container,h=c.health||{},s=d.security,m=d.maintenance,r=b.responses||{};
    const backendGood=/running|ready/i.test(String(b.state));
    const containerGood=!h.error&&h.ok!==false;
    const totalResponses=Object.values(r).reduce((sum,v)=>sum+Number(v||0),0);
    const successRate=totalResponses?`${(Number(r['2xx']||0)/totalResponses*100).toFixed(1)}%`:'—';

    const responseBody=`<div class="response-grid">
        <div><span>2xx success</span><strong class="good-text">${num(r['2xx'])}</strong></div>
        <div><span>3xx redirect</span><strong>${num(r['3xx'])}</strong></div>
        <div><span>4xx client</span><strong class="warn-text">${num(r['4xx'])}</strong></div>
        <div><span>5xx server</span><strong class="bad-text">${num(r['5xx'])}</strong></div>
    </div>`;

    const maintenanceBody=`<div class="maintenance-grid">
        <div><span>Refresh interval</span><strong>${num(m.refresh_interval_hours)}h</strong><small>scheduled process cycle</small></div>
        <div><span>Container</span><strong>${num(m.container_refreshes)}</strong><small>refreshes</small></div>
        <div><span>Vault</span><strong>${num(m.vault_refreshes)}</strong><small>refreshes</small></div>
        <div><span>Error reporter</span><strong>${num(m.error_reporter_refreshes)}</strong><small>refreshes</small></div>
    </div>`;

    $('#overview-view').innerHTML=`
        <div class="page-intro"><div><div class="eyebrow">SYSTEM AT A GLANCE</div><h2>Everything important, minus the JSON soup.</h2><p>Live backend, container, request, and security telemetry. The dashboard itself is isolated on port 5799.</p></div>${status(backendGood&&containerGood?'Systems nominal':'Needs attention',backendGood&&containerGood?'good':'warn')}</div>
        <div class="metric-grid">
            ${metric('Backend',b.state,`PID ${b.pid}`,backendGood?'good':'warn','BE')}
            ${metric('Container',containerGood?'Healthy':'Attention',`PID ${c.pid??'—'} · gen ${c.generation}`,containerGood?'good':'bad','CT')}
            ${metric('Uptime',duration(b.uptime_secs),'backend.exe','','UP')}
            ${metric('Requests',num(b.total_requests),`${num(b.active_requests)} active`,'','RQ')}
            ${metric('Avg latency',ms(b.average_latency_ms),'rolling average',Number(b.average_latency_ms)>100?'warn':'good','MS')}
            ${metric('Success rate',successRate,`${num(r['5xx'])} server errors`,Number(r['5xx'])?'warn':'good','2X')}
            ${metric('Banned IPs',num(s.banned_ips),`${num(s.active_strike_buckets)} strike buckets`,s.banned_ips?'bad':'good','IP')}
            ${metric('Generation',num(c.generation),c.control_address||'container IPC','','GN')}
        </div>
        <div class="two-col">${panel('HTTP responses','Response families since process start',responseBody)}${panel('Maintenance','Rolling process refresh activity',maintenanceBody)}</div>`;
}

async function renderBackend(){
    const d=await api('backend'),r=d.requests||{};
    const good=/running|ready/i.test(String(d.state));
    const requestView={total:r.total,active:r.active,average_latency_ms:r.average_latency_ms,'2xx':r['2xx'],'3xx':r['3xx'],'4xx':r['4xx'],'5xx':r['5xx']};
    const apiView={...d.api,max_body_size:bytes(d.api.max_body_size_bytes)};delete apiView.max_body_size_bytes;
    $('#backend-view').innerHTML=`
        <div class="page-intro"><div><div class="eyebrow">BACKEND.EXE</div><h2>Backend process</h2><p>Runtime health, traffic counters, and effective API limits.</p></div>${status(d.state,good?'good':'warn')}</div>
        <div class="metric-grid compact">
            ${metric('Process ID',d.pid,'backend.exe','','ID')}
            ${metric('Uptime',duration(d.uptime_secs),'process age','','UP')}
            ${metric('Requests',num(r.total),`${num(r.active)} active`,'','RQ')}
            ${metric('Latency',ms(r.average_latency_ms),'average',Number(r.average_latency_ms)>100?'warn':'good','MS')}
        </div>
        <div class="three-col">${panel('Requests','Traffic counters',kvRows(requestView))}${panel('API','Listener and limits',kvRows(apiView))}${panel('Runtime','Process configuration',kvRows(d.runtime))}</div>${raw(d)}`;
}

async function renderContainer(){
    const d=await api('container'),good=d.online!==false,state=d.state||{};
    const config=state.config||{};
    const runtime={...state};delete runtime.config;
    const runtimeBody=Object.keys(runtime).length?kvRows(runtime):'<div class="empty">No additional runtime fields reported.</div>';
    $('#container-view').innerHTML=`
        <div class="page-intro"><div><div class="eyebrow">CONTAINER RUNTIME</div><h2>Execution container</h2><p>IPC endpoint, generation, environments, swamps, workers, and cache state.</p></div>${status(good?'Online':'Offline',good?'good':'bad')}</div>
        <div class="metric-grid compact">
            ${metric('Status',good?'Online':'Offline',d.error||'control channel',good?'good':'bad','CT')}
            ${metric('Process ID',d.pid??'—','container process','','ID')}
            ${metric('Generation',d.generation??'—','rolling generation','','GN')}
            ${metric('Control',d.control_address||'—','IPC address','','IP')}
        </div>
        <div class="two-col">${panel('Resolved config','What the container is actually using',kvRows(config))}${panel('Runtime state','Live container internals',runtimeBody)}</div>${raw(d)}`;
}

async function renderSecurity(){
    const d=await api('security'),p=d.policy||{},bans=d.banned_ips||[],strikes=d.strikes||[];
    const global=p.global_rate_limit||{},apiLimit=p.api_rate_limit||{};
    $('#security-view').innerHTML=`
        <div class="page-intro"><div><div class="eyebrow">SECURITY</div><h2>Abuse controls</h2><p>IP bans, strike buckets, trusted proxy behavior, and rate-limit policy.</p></div>${status(bans.length?'Active bans':'Clear',bans.length?'warn':'good')}</div>
        <div class="metric-grid compact">
            ${metric('Banned IPs',bans.length,`${strikes.length} strike buckets`,bans.length?'bad':'good','IP')}
            ${metric('Strike threshold',p.strike_threshold??'—',`${p.strike_window_secs??'—'}s window`,'','ST')}
            ${metric('Ban duration',duration(p.ban_duration_secs),`${p.ban_duration_secs??'—'} seconds`,'','BN')}
            ${metric('Proxy headers',p.trusted_proxy_headers?'Trusted':'Ignored','real IP policy',p.trusted_proxy_headers?'warn':'good','PX')}
        </div>
        <div class="two-col">${panel('Global rate limit','All requests',kvRows(global))}${panel('API rate limit','API requests',kvRows(apiLimit))}</div>
        <div class="two-col">${panel('Bans',`${bans.length} active`,table(['ip','age_secs','remaining_secs'],bans))}${panel('Strikes',`${strikes.length} active buckets`,table(['ip','category','count','age_secs','remaining_window_secs'],strikes))}</div>${raw(d)}`;
}

async function renderSettings(){
    const d=await api('settings'),c=d.containers||{},dash=d.dashboards||{},maintenance=d.maintenance||{};
    const resolved=c.resolved;
    const configured={...c};delete configured.resolved;
    $('#settings-view').innerHTML=`
        <div class="page-intro"><div><div class="eyebrow">SETTINGS</div><h2>Configured vs resolved</h2><p>Effective container topology plus the local dashboard configuration.</p></div>${status(dash.enabled?'Dashboard enabled':'Dashboard disabled',dash.enabled?'good':'warn')}</div>
        <div class="two-col">${panel('Configured containers','Backend configuration',kvRows(configured))}${panel('Dashboard','Local control-room listener',kvRows(dash))}</div>
        <div class="two-col">${panel('Resolved container config','Runtime-resolved values',resolved&&typeof resolved==='object'?kvRows(resolved):`<div class="empty">${esc(valueText(resolved))}</div>`)}${panel('Maintenance','Process refresh policy',kvRows(maintenance))}</div>${raw(d)}`;
}

const renderers={overview:renderOverview,backend:renderBackend,container:renderContainer,security:renderSecurity,settings:renderSettings};

async function refresh(){
    const view=$(`#${tab}-view`);
    try{
        await renderers[tab]();
        $('#last-refresh').textContent=new Date().toLocaleTimeString();
        $('.live-state').classList.remove('offline');
    }catch(error){
        console.error(error);
        view.innerHTML=errorView(error);
        $('.live-state').classList.add('offline');
        $('#last-refresh').textContent='request failed';
    }
}

$$('.nav-item').forEach(button=>button.addEventListener('click',()=>{
    $$('.nav-item').forEach(x=>x.classList.remove('active'));
    $$('.tab').forEach(x=>x.classList.remove('active'));
    button.classList.add('active');
    tab=button.dataset.tab;
    $(`#${tab}`).classList.add('active');
    $('#page-title').textContent=title(tab);
    refresh();
}));

async function tick(){
    if(!document.hidden)await refresh();
    setTimeout(tick,1000);
}
document.addEventListener('visibilitychange',()=>{if(!document.hidden)refresh()});
tick();
</script>
</body>
</html>"#;

const DASHBOARD_CSS: &str = r#"
:root{
    color-scheme:dark;
    --bg:#080b12;
    --surface:#0d121c;
    --surface-2:#111824;
    --surface-3:#151e2d;
    --line:#202b3d;
    --line-strong:#2b3a51;
    --text:#eef4ff;
    --muted:#8492a8;
    --faint:#56657b;
    --accent:#7c8cff;
    --accent-soft:rgba(124,140,255,.13);
    --good:#57d6a2;
    --good-soft:rgba(87,214,162,.10);
    --warn:#f3bf69;
    --warn-soft:rgba(243,191,105,.10);
    --bad:#ff747f;
    --bad-soft:rgba(255,116,127,.10);
    font:14px Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
    background:var(--bg);
    color:var(--text);
}
*{box-sizing:border-box}
html,body{margin:0;min-height:100%;background:var(--bg)}
body{min-height:100vh;background:radial-gradient(circle at 70% -20%,rgba(80,94,180,.12),transparent 33%),var(--bg)}
button,input{font:inherit}
button{color:inherit}
.app-shell{display:grid;grid-template-columns:228px minmax(0,1fr);min-height:100vh}
.sidebar{position:sticky;top:0;height:100vh;display:flex;flex-direction:column;padding:20px 14px;border-right:1px solid var(--line);background:rgba(9,13,21,.94);backdrop-filter:blur(18px)}
.brand{display:flex;align-items:center;gap:11px;padding:4px 8px 22px}
.brand-mark{position:relative;width:35px;height:35px;border:1px solid #33425c;border-radius:10px;background:linear-gradient(145deg,#182234,#0d121c);box-shadow:inset 0 0 0 1px rgba(255,255,255,.02),0 8px 24px rgba(0,0,0,.22)}
.brand-mark span{position:absolute;width:6px;height:6px;border-radius:2px;background:var(--accent);box-shadow:0 0 14px rgba(124,140,255,.65)}
.brand-mark span:nth-child(1){left:8px;top:8px}.brand-mark span:nth-child(2){right:8px;top:8px}.brand-mark span:nth-child(3){left:8px;bottom:8px}
.brand-copy{display:flex;flex-direction:column;line-height:1}.brand-copy strong{font-size:17px;letter-spacing:.08em}.brand-copy small{margin-top:5px;color:var(--muted);font-size:9px;letter-spacing:.2em}
.nav-label{padding:8px 10px;color:var(--faint);font-size:10px;font-weight:700;letter-spacing:.18em}
nav{display:flex;flex-direction:column;gap:4px}
.nav-item{display:flex;align-items:center;gap:10px;width:100%;padding:9px 10px;border:1px solid transparent;border-radius:9px;background:transparent;color:#a7b2c3;text-align:left;cursor:pointer;transition:background .16s ease,border-color .16s ease,color .16s ease,transform .16s ease}
.nav-item:hover{background:#111925;color:#eaf0fb}.nav-item:active{transform:translateY(1px)}
.nav-item.active{border-color:#2a3850;background:linear-gradient(90deg,var(--accent-soft),rgba(124,140,255,.035));color:white}
.nav-icon{display:grid;place-items:center;width:27px;height:27px;border:1px solid #2a374b;border-radius:7px;background:#0c111a;color:#71819a;font:700 9px ui-monospace,SFMono-Regular,Consolas,monospace;letter-spacing:.05em}
.nav-item.active .nav-icon{border-color:#475896;background:#171e36;color:#aab5ff}
.sidebar-footer{margin-top:auto;padding:14px 8px 4px;border-top:1px solid var(--line);color:var(--muted)}
.sidebar-footer small{display:block;margin-top:8px;color:var(--faint);font:11px ui-monospace,SFMono-Regular,Consolas,monospace}
.local-pill{display:flex;align-items:center;gap:8px;font-size:12px}.local-pill i,.status i,.live-state i{width:7px;height:7px;border-radius:50%;background:var(--good);box-shadow:0 0 10px rgba(87,214,162,.55)}
.workspace{min-width:0}.topbar{height:76px;display:flex;align-items:center;justify-content:space-between;padding:0 28px;border-bottom:1px solid var(--line);background:rgba(8,11,18,.82);backdrop-filter:blur(18px);position:sticky;top:0;z-index:10}
.eyebrow{color:#697990;font-size:10px;font-weight:800;letter-spacing:.16em}.topbar h1{margin:4px 0 0;font-size:20px;letter-spacing:-.02em}
.live-state{display:grid;grid-template-columns:auto auto;align-items:center;column-gap:7px;row-gap:1px;padding:8px 11px;border:1px solid #243348;border-radius:10px;background:#0d131d}.live-state span{color:#b8c5d8;font-size:10px;font-weight:800;letter-spacing:.14em}.live-state small{grid-column:2;color:var(--faint);font:10px ui-monospace,SFMono-Regular,Consolas,monospace}.live-state.offline i{background:var(--bad);box-shadow:0 0 10px rgba(255,116,127,.55)}
main{max-width:1540px;margin:0 auto;padding:28px}.tab{display:none}.tab.active{display:block}.loading{display:grid;place-items:center;min-height:260px;color:var(--muted)}
.page-intro{display:flex;align-items:flex-end;justify-content:space-between;gap:24px;margin-bottom:21px}.page-intro h2{margin:7px 0 6px;font-size:24px;letter-spacing:-.035em}.page-intro p{max-width:720px;margin:0;color:var(--muted);line-height:1.55}
.status{display:inline-flex;align-items:center;gap:8px;padding:7px 10px;border:1px solid #27354a;border-radius:999px;background:#0e151f;color:#b8c4d4;font-size:11px;font-weight:700;white-space:nowrap}.status.good{border-color:rgba(87,214,162,.22);background:var(--good-soft);color:#9be7c8}.status.warn{border-color:rgba(243,191,105,.23);background:var(--warn-soft);color:#f5d79e}.status.bad{border-color:rgba(255,116,127,.23);background:var(--bad-soft);color:#ffadb4}.status.warn i{background:var(--warn);box-shadow:0 0 10px rgba(243,191,105,.5)}.status.bad i{background:var(--bad);box-shadow:0 0 10px rgba(255,116,127,.5)}
.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:12px}.metric-grid.compact{grid-template-columns:repeat(4,minmax(0,1fr));margin-bottom:14px}
.metric{min-width:0;min-height:132px;padding:15px;border:1px solid var(--line);border-radius:12px;background:linear-gradient(155deg,rgba(19,27,40,.92),rgba(12,17,26,.94));box-shadow:0 10px 28px rgba(0,0,0,.09)}
.metric.good{border-color:rgba(87,214,162,.18)}.metric.warn{border-color:rgba(243,191,105,.2)}.metric.bad{border-color:rgba(255,116,127,.2)}
.metric-top{display:flex;align-items:center;gap:9px}.metric-icon{display:grid;place-items:center;width:27px;height:27px;border:1px solid #2b394e;border-radius:7px;background:#0b111a;color:#71829a;font:700 9px ui-monospace,SFMono-Regular,Consolas,monospace}.metric-label{color:var(--muted);font-size:11px;font-weight:650;text-transform:uppercase;letter-spacing:.08em}.metric-value{overflow:hidden;margin-top:16px;color:#f3f6fc;font-size:23px;font-weight:720;letter-spacing:-.035em;text-overflow:ellipsis;white-space:nowrap}.metric-meta{overflow:hidden;margin-top:5px;color:var(--faint);font:11px ui-monospace,SFMono-Regular,Consolas,monospace;text-overflow:ellipsis;white-space:nowrap}
.two-col,.three-col{display:grid;gap:12px;margin-top:12px}.two-col{grid-template-columns:repeat(2,minmax(0,1fr))}.three-col{grid-template-columns:repeat(3,minmax(0,1fr))}
.panel{min-width:0;border:1px solid var(--line);border-radius:12px;background:rgba(14,20,30,.9);overflow:hidden}.panel-head{display:flex;align-items:center;justify-content:space-between;gap:18px;padding:15px 16px;border-bottom:1px solid var(--line);background:rgba(18,25,37,.7)}.panel-head h2{margin:0;font-size:13px;letter-spacing:-.01em}.panel-head p{margin:4px 0 0;color:var(--faint);font-size:11px}
.response-grid{display:grid;grid-template-columns:repeat(4,1fr);padding:12px}.response-grid div{padding:10px;border-right:1px solid var(--line)}.response-grid div:last-child{border-right:0}.response-grid span{display:block;color:var(--faint);font-size:10px;text-transform:uppercase;letter-spacing:.07em}.response-grid strong{display:block;margin-top:7px;font-size:20px}.good-text{color:var(--good)!important}.warn-text{color:var(--warn)!important}.bad-text{color:var(--bad)!important}
.maintenance-grid{display:grid;grid-template-columns:repeat(2,1fr);padding:7px}.maintenance-grid div{padding:10px}.maintenance-grid span{display:block;color:var(--muted);font-size:10px;text-transform:uppercase;letter-spacing:.07em}.maintenance-grid strong{display:block;margin-top:5px;font-size:18px}.maintenance-grid small{display:block;margin-top:3px;color:var(--faint);font-size:10px}
.kv{padding:5px 15px 8px}.kv-row{display:grid;grid-template-columns:minmax(130px,.8fr) minmax(0,1.2fr);gap:18px;align-items:center;padding:10px 1px;border-bottom:1px solid rgba(32,43,61,.72)}.kv-row:last-child{border-bottom:0}.kv-row span{color:var(--muted);font-size:11px}.kv-row strong{min-width:0;color:#d9e2ef;font:500 11px ui-monospace,SFMono-Regular,Consolas,monospace;text-align:right;overflow-wrap:anywhere}
.table-wrap{overflow:auto;max-height:390px}table{width:100%;border-collapse:collapse;font-size:11px}th,td{padding:10px 12px;border-bottom:1px solid var(--line);text-align:left;white-space:nowrap}th{position:sticky;top:0;background:#121a26;color:var(--muted);font-size:9px;text-transform:uppercase;letter-spacing:.08em}td{color:#c4cfde;font-family:ui-monospace,SFMono-Regular,Consolas,monospace}tbody tr:last-child td{border-bottom:0}
.empty{padding:22px;color:var(--faint);font-size:12px;text-align:center}.raw{margin-top:12px;border:1px solid var(--line);border-radius:11px;background:#0b1018;overflow:hidden}.raw summary{cursor:pointer;padding:12px 14px;color:var(--muted);font-size:11px;font-weight:650;user-select:none}.raw[open] summary{border-bottom:1px solid var(--line)}pre{max-height:55vh;overflow:auto;margin:0;padding:14px;color:#9fafc6;background:#090e15;font:11px/1.6 ui-monospace,SFMono-Regular,Consolas,monospace;white-space:pre-wrap;overflow-wrap:anywhere}
.error-state{display:flex;align-items:flex-start;gap:12px;padding:18px;border:1px solid rgba(255,116,127,.25);border-radius:12px;background:var(--bad-soft)}.error-badge{display:grid;place-items:center;flex:0 0 28px;height:28px;border:1px solid rgba(255,116,127,.3);border-radius:8px;color:var(--bad);font-weight:800}.error-state strong{color:#ffc2c7}.error-state p{margin:5px 0 0;color:#c6878d;font:11px ui-monospace,SFMono-Regular,Consolas,monospace;overflow-wrap:anywhere}
@media(max-width:1180px){.metric-grid{grid-template-columns:repeat(2,minmax(0,1fr))}.three-col{grid-template-columns:1fr}.metric-grid.compact{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media(max-width:820px){.app-shell{grid-template-columns:1fr}.sidebar{position:sticky;z-index:20;height:auto;padding:10px;border-right:0;border-bottom:1px solid var(--line)}.brand,.nav-label,.sidebar-footer{display:none}nav{flex-direction:row;overflow:auto}.nav-item{min-width:max-content;width:auto;padding:7px 9px}.nav-icon{display:none}.topbar{height:66px;padding:0 16px}main{padding:18px 14px}.page-intro{align-items:flex-start}.two-col{grid-template-columns:1fr}.response-grid{grid-template-columns:repeat(2,1fr)}.response-grid div:nth-child(2){border-right:0}.response-grid div:nth-child(-n+2){border-bottom:1px solid var(--line)}}
@media(max-width:520px){.metric-grid,.metric-grid.compact{grid-template-columns:1fr}.page-intro{flex-direction:column;gap:12px}.page-intro h2{font-size:21px}.metric{min-height:118px}.live-state small{display:none}.maintenance-grid{grid-template-columns:1fr}.kv-row{grid-template-columns:1fr;gap:4px}.kv-row strong{text-align:left}.topbar h1{font-size:18px}}
"#;
