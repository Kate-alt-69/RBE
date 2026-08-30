use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use container_runtime_core::Runtime;

pub fn spawn(address: String, token: String, runtime: Arc<Runtime>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&address)?;
    println!("container: dashboard listening on http://{}/", listener.local_addr()?);
    thread::Builder::new()
        .name("container-dashboard".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let token = token.clone();
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    if let Err(error) = handle(stream, &token, &runtime) {
                        tracing::debug!(%error, "dashboard connection closed");
                    }
                });
            }
        })?;
    Ok(())
}

fn handle(mut stream: TcpStream, token: &str, runtime: &Runtime) -> anyhow::Result<()> {
    let mut buffer = [0_u8; 8192];
    let size = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..size]);
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or_default();

    let mut authorized = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Authorization:") {
            authorized = value.trim() == format!("Bearer {token}");
        }
    }

    if !authorized {
        return respond(
            &mut stream,
            401,
            "Unauthorized",
            "text/plain; charset=utf-8",
            "dashboard authentication required\n",
        );
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");

    match path {
        "/" => respond(
            &mut stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            &html(token),
        ),
        "/api/overview" => respond(
            &mut stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            &overview_json(runtime),
        ),
        "/api/state" => respond(
            &mut stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            &state_json(runtime),
        ),
        "/api/events" => respond(
            &mut stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            &events_json(),
        ),
        "/healthz" => respond(
            &mut stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            "ok\n",
        ),
        _ => respond(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "not found\n",
        ),
    }
}

fn overview_json(runtime: &Runtime) -> String {
    let environments = runtime.snapshots();
    let mut workers_total = 0usize;
    let mut workers_in_use = 0usize;
    let mut swamps_total = 0usize;
    let mut swamps_active = 0usize;
    let mut throughput_per_sec = 0.0f64;
    let mut completed = 0u64;
    let mut failed = 0u64;

    for environment in &environments {
        swamps_total += environment.swamps.len();
        for swamp in &environment.swamps {
            throughput_per_sec += swamp.throughput_per_sec;
            completed += swamp.completed;
            failed += swamp.failed;

            if swamp.queued > 0 || swamp.workers.iter().any(|worker| worker.current.is_some()) {
                swamps_active += 1;
            }

            workers_total += swamp.workers.len();
            workers_in_use += swamp
                .workers
                .iter()
                .filter(|worker| worker.current.is_some())
                .count();
        }
    }

    serde_json::json!({
        "pid": std::process::id(),
        "queue": runtime.global_queue_len(),
        "environments": environments.len(),
        "workers_in_use": workers_in_use,
        "workers_total": workers_total,
        "swamps_active": swamps_active,
        "swamps_total": swamps_total,
        "throughput_per_sec": throughput_per_sec,
        "completed": completed,
        "failed": failed,
        "cache_profiles": runtime.cache().len(),
        "cache_artifacts": runtime.cache().artifact_count(),
        "security": {
            "policy": "deny-by-default",
            "wasm": "wasmtime",
            "linux": "namespaces + no_new_privs + seccomp + cgroup-v2 + timeout"
        }
    })
    .to_string()
}

fn state_json(runtime: &Runtime) -> String {
    let environments = runtime.snapshots().into_iter().map(|e| {
        let swamps = e.swamps.into_iter().map(|s| {
            let workers = s.workers.into_iter().map(|w| serde_json::json!({
                "id": w.id,
                "state": format!("{:?}", w.state),
                "current": w.current.map(|id| id.to_string()),
                "completed": w.completed,
                "failed": w.failed,
                "avg_ms": if w.completed == 0 { 0.0 } else { w.total_ms as f64 / w.completed as f64 },
            })).collect::<Vec<_>>();
            serde_json::json!({
                "id": s.id,
                "queued": s.queued,
                "cost": s.queued_cost,
                "throughput": s.throughput_per_sec,
                "completed": s.completed,
                "failed": s.failed,
                "workers": workers
            })
        }).collect::<Vec<_>>();
        serde_json::json!({
            "id": e.id.to_string(),
            "generation": e.generation,
            "queued": e.queued,
            "cost": e.queued_cost,
            "workers": e.worker_count,
            "storage_mib": e.storage_limit_bytes / (1024 * 1024),
            "ephemeral": e.storage_ephemeral,
            "swamps": swamps
        })
    }).collect::<Vec<_>>();

    serde_json::json!({
        "pid": std::process::id(),
        "queue": runtime.global_queue_len(),
        "cache_profiles": runtime.cache().len(),
        "cache_artifacts": runtime.cache().artifact_count(),
        "security": {
            "policy": "deny-by-default",
            "wasm": "wasmtime",
            "linux": "namespaces + no_new_privs + seccomp + cgroup-v2 + timeout"
        },
        "environments": environments
    }).to_string()
}

fn events_json() -> String {
    let content = fs::read_to_string(crate::event_log_path()).unwrap_or_default();
    let events = content.lines().rev().take(250)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    serde_json::json!({ "events": events.into_iter().rev().collect::<Vec<_>>() }).to_string()
}

fn html(token: &str) -> String {
    let token_json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into());
    let template = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>RBE Container</title>
<style>
:root{color-scheme:dark;font:14px system-ui,sans-serif;background:#0b1020;color:#e7edf7}
body{margin:0;background:#0b1020}.top{padding:18px 22px;background:#11182b;border-bottom:1px solid #26324a;position:sticky;top:0;z-index:2}
h1{margin:0 0 5px;font-size:22px}.muted{color:#8ea0bb}
nav{display:flex;gap:8px;padding:12px 22px;background:#0f1627;border-bottom:1px solid #26324a;position:sticky;top:69px;z-index:2}
button{background:#17223a;color:#dce7f7;border:1px solid #2a3a59;border-radius:7px;padding:7px 12px;cursor:pointer}
button.active{background:#263957;border-color:#4b668f}
main{padding:20px 22px}.tab{display:none}.tab.active{display:block}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}.card,.panel{background:#11182b;border:1px solid #26324a;border-radius:10px;padding:15px;margin-bottom:15px}
.big{font-size:28px;font-weight:700;margin-top:5px}.ok{color:#63e6be}.warn{color:#ffd43b}.bad{color:#ff8787}
table{width:100%;border-collapse:collapse}td,th{padding:8px;border-bottom:1px solid #24304a;text-align:left}th{color:#8ea0bb}pre{white-space:pre-wrap;max-height:500px;overflow:auto;color:#cdd8e9}
.progress{height:8px;background:#202d46;border-radius:999px;overflow:hidden}.fill{height:100%;background:#63e6be;width:0}
</style></head>
<body>
<div class="top"><h1>RBE Container Runtime</h1><div class="muted">Live scheduler + security telemetry</div></div>
<nav>
<button id="tab-overview" class="active" onclick="selectTab('overview')">Overview</button>
<button id="tab-environments" onclick="selectTab('environments')">Environments</button>
<button id="tab-events" onclick="selectTab('events')">Events</button>
</nav>
<main>
<section id="overview" class="tab active">
<div class="grid" id="overview-cards">Loading...</div>
<div class="panel"><h2>Worker Utilization</h2><div id="worker-util">Loading...</div></div>
<div class="panel"><h2>Swamp Utilization</h2><div id="swamp-util">Loading...</div></div>
</section>
<section id="environments" class="tab">
<div class="panel"><h2>Environments / Swamps / Workers</h2><div id="envs">Loading...</div></div>
</section>
<section id="events" class="tab">
<div class="panel"><h2>Recent Events</h2><pre id="events-log">Loading...</pre></div>
</section>
</main>
<script>
const token=TOKEN_PLACEHOLDER;
const H={Authorization:'Bearer '+token};
let activeTab='overview';
let timer=null;

async function get(path){const r=await fetch(path,{headers:H,cache:'no-store'});if(!r.ok)throw Error(r.status+' '+r.statusText);return r.json()}
function esc(v){return String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
function pct(a,b){return b<=0?0:Math.min(100,Math.round((a/b)*100))}

function selectTab(tab){
 activeTab=tab;
 document.querySelectorAll('.tab').forEach(el=>el.classList.toggle('active',el.id===tab));
 document.querySelectorAll('nav button').forEach(el=>el.classList.remove('active'));
 document.querySelector('#tab-'+tab).classList.add('active');
 refresh();
 schedule();
}

function schedule(){
 if(timer){clearTimeout(timer);timer=null}
 if(document.hidden)return;
 const delay=activeTab==='overview'?500:1000;
 timer=setTimeout(async()=>{await refresh();schedule()},delay);
}

document.addEventListener('visibilitychange',()=>{if(document.hidden){if(timer)clearTimeout(timer);timer=null}else{refresh();schedule()}});

function renderOverview(s){
 const workerPct=pct(s.workers_in_use,s.workers_total);
 const swampPct=pct(s.swamps_active,s.swamps_total);
 document.querySelector('#overview-cards').innerHTML=`
 <div class="card"><div class="muted">Workers In Use</div><div class="big">${esc(s.workers_in_use)} / ${esc(s.workers_total)}</div><div class="progress"><div class="fill" style="width:${workerPct}%"></div></div></div>
 <div class="card"><div class="muted">Workers Idle</div><div class="big">${esc(s.workers_total-s.workers_in_use)}</div></div>
 <div class="card"><div class="muted">Swamps Active</div><div class="big">${esc(s.swamps_active)} / ${esc(s.swamps_total)}</div><div class="progress"><div class="fill" style="width:${swampPct}%"></div></div></div>
 <div class="card"><div class="muted">Global Queue</div><div class="big">${esc(s.queue)}</div></div>
 <div class="card"><div class="muted">Throughput</div><div class="big">${esc(Number(s.throughput_per_sec).toFixed(1))}<span class="muted"> /s</span></div></div>
 <div class="card"><div class="muted">Completed</div><div class="big ok">${esc(s.completed)}</div></div>
 <div class="card"><div class="muted">Failed</div><div class="big ${s.failed?'bad':'ok'}">${esc(s.failed)}</div></div>
 <div class="card"><div class="muted">Environments</div><div class="big">${esc(s.environments)}</div></div>`;
 document.querySelector('#worker-util').innerHTML=`<div><b>${esc(s.workers_in_use)} / ${esc(s.workers_total)}</b> workers currently executing · ${workerPct}% in use</div>`;
 document.querySelector('#swamp-util').innerHTML=`<div><b>${esc(s.swamps_active)} / ${esc(s.swamps_total)}</b> Swamps currently active · ${swampPct}% active</div>`;
}

function renderEnvironments(s){
 let h='<table><tr><th>Environment</th><th>Queue</th><th>Swamps</th><th>Workers</th><th>Storage</th></tr>';
 for(const e of s.environments){
   h+=`<tr><td>${esc(e.id)} gen=${esc(e.generation)}</td><td>${esc(e.queued)} / ${esc(e.cost)}</td><td>${esc(e.swamps.length)}</td><td>${esc(e.workers)}</td><td>${esc(e.storage_mib)} MiB ${esc(e.ephemeral)}</td></tr>`;
   for(const sw of e.swamps){
     h+=`<tr><td colspan=5><small>Swamp ${esc(sw.id)} · queue=${esc(sw.queued)} · throughput=${Number(sw.throughput).toFixed(1)}/s · done=${esc(sw.completed)} · failed=${esc(sw.failed)}</small>`;
     for(const w of sw.workers)h+=`<div style="padding-left:20px">Worker-${esc(w.id)} · ${esc(w.state)} · ${esc(w.current||'-')} · avg=${Number(w.avg_ms).toFixed(1)}ms · done=${esc(w.completed)} · failed=${esc(w.failed)}</div>`;
     h+='</td></tr>';
   }
 }
 document.querySelector('#envs').innerHTML=h+'</table>';
}

async function refresh(){
 if(document.hidden)return;
 try{
   if(activeTab==='overview'){renderOverview(await get('/api/overview'))}
   else if(activeTab==='environments'){renderEnvironments(await get('/api/state'))}
   else {const e=await get('/api/events');document.querySelector('#events-log').textContent=e.events.map(x=>JSON.stringify(x)).join('\\n')||'No events yet.'}
 }catch(e){console.error('dashboard:',e)}
}

refresh();schedule();
</script></body></html>"#;
    template.replace("TOKEN_PLACEHOLDER", &token_json)
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> anyhow::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}
