use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use container_runtime_core::Runtime;

const EVENT_LOG: &str = "./data/container-runtime/container-events.jsonl";

pub fn spawn(address: String, token: String, runtime: Arc<Runtime>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&address)?;
    println!("container: dashboard listening on http://{}/", listener.local_addr()?);
    thread::Builder::new().name("container-dashboard".into()).spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let token = token.clone();
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    if let Err(error) = handle(stream, &token, &runtime) {
                        tracing::debug!(%error, "dashboard connection closed");
                    }
                });
            }
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
        if line.is_empty() { break; }
        if let Some(value) = line.strip_prefix("Authorization:") {
            authorized = value.trim() == format!("Bearer {token}");
        }
    }
    if !authorized {
        return respond(&mut stream, 401, "Unauthorized", "text/plain; charset=utf-8", "dashboard authentication required\n");
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
    match path {
        "/" => respond(&mut stream, 200, "OK", "text/html; charset=utf-8", &html(token)),
        "/api/state" => respond(&mut stream, 200, "OK", "application/json; charset=utf-8", &state_json(runtime)),
        "/api/events" => respond(&mut stream, 200, "OK", "application/json; charset=utf-8", &events_json()),
        "/healthz" => respond(&mut stream, 200, "OK", "text/plain; charset=utf-8", "ok\n"),
        _ => respond(&mut stream, 404, "Not Found", "text/plain; charset=utf-8", "not found\n"),
    }
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
                "id": s.id, "queued": s.queued, "cost": s.queued_cost,
                "throughput": s.throughput_per_sec, "completed": s.completed,
                "failed": s.failed, "workers": workers
            })
        }).collect::<Vec<_>>();
        serde_json::json!({
            "id": e.id.to_string(), "generation": e.generation, "queued": e.queued,
            "cost": e.queued_cost, "workers": e.worker_count,
            "storage_mib": e.storage_limit_bytes / (1024 * 1024),
            "ephemeral": e.storage_ephemeral, "swamps": swamps
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
    let content = fs::read_to_string(EVENT_LOG).unwrap_or_default();
    let events = content.lines().rev().take(250)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    serde_json::json!({ "events": events.into_iter().rev().collect::<Vec<_>>() }).to_string()
}

fn html(token: &str) -> String {
    let token_json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into());
    format!(r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>RBE Container</title><style>
body{{margin:0;padding:20px;background:#0b1020;color:#e7edf7;font:14px system-ui}}h1{{margin-top:0}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}}.card,.panel{{background:#11182b;border:1px solid #26324a;border-radius:10px;padding:15px;margin-bottom:15px}}.big{{font-size:24px;font-weight:700}}.ok{{color:#63e6be}}table{{width:100%;border-collapse:collapse}}td,th{{padding:8px;border-bottom:1px solid #24304a;text-align:left}}th{{color:#8ea0bb}}pre{{white-space:pre-wrap;max-height:400px;overflow:auto;color:#cdd8e9}}
</style></head><body><h1>RBE Container Runtime</h1><div class="muted">Live scheduler + security telemetry</div><div class="grid" id="cards"></div><div class="panel"><h2>Environments / Swamps / Workers</h2><div id="envs">Loading...</div></div><div class="panel"><h2>Recent Events</h2><pre id="events">Loading...</pre></div><script>
const token=TOKEN_PLACEHOLDER;const H={{Authorization:'Bearer '+token}};
async function get(p){{const r=await fetch(p,{{headers:H}});if(!r.ok)throw Error(r.status);return r.json()}}
function esc(v){{return String(v??'').replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]))}}
function render(s){{document.querySelector('#cards').innerHTML=`<div class=card><b>PID</b><div class=big>${{esc(s.pid)}}</div></div><div class=card><b>Queue</b><div class=big>${{esc(s.queue)}}</div></div><div class=card><b>Artifacts</b><div class=big>${{esc(s.cache_artifacts)}}</div><small>${{esc(s.cache_profiles)}} profiles</small></div><div class=card><b>Sandbox</b><div class="big ok">DENY-BY-DEFAULT</div><small>${{esc(s.security.linux)}}</small></div>`;let h='<table><tr><th>Environment</th><th>Queue</th><th>Swamps</th><th>Workers</th><th>Storage</th></tr>';for(const e of s.environments){{h+=`<tr><td>${{esc(e.id)}} gen=${{esc(e.generation)}}</td><td>${{esc(e.queued)}} / ${{esc(e.cost)}}</td><td>${{esc(e.swamps.length)}}</td><td>${{esc(e.workers)}}</td><td>${{esc(e.storage_mib)}} MiB ${{esc(e.ephemeral)}}</td></tr>`;for(const sw of e.swamps){{h+=`<tr><td colspan=5><small>Swamp ${{esc(sw.id)}} · queue=${{esc(sw.queued)}} · throughput=${{Number(sw.throughput).toFixed(1)}}/s · done=${{esc(sw.completed)}} · failed=${{esc(sw.failed)}}</small>`;for(const w of sw.workers)h+=`<div style="padding-left:20px">Worker-${{esc(w.id)}} · ${{esc(w.state)}} · ${{esc(w.current||'-')}} · avg=${{Number(w.avg_ms).toFixed(1)}}ms · done=${{esc(w.completed)}} · failed=${{esc(w.failed)}}</div>`;h+='</td></tr>'}}}}document.querySelector('#envs').innerHTML=h+'</table>'}}
async function refresh(){{try{{render(await get('/api/state'));const e=await get('/api/events');document.querySelector('#events').textContent=e.events.map(x=>JSON.stringify(x)).join('\\n')||'No events yet.'}}catch(e){{document.querySelector('#events').textContent='Dashboard error: '+e}}}}refresh();setInterval(refresh,1000);
</script></body></html>"#, token_json).replace("TOKEN_PLACEHOLDER", &token_json)
}

fn respond(stream: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str) -> anyhow::Result<()> {
    let header = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}
