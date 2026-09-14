//! Temporary HTTP maintenance responder used while the real backend boots.
//!
//! The normal backend process spawns this same executable in
//! `--maintenance-notice` mode after reclaiming any stale listener. The helper
//! owns the public API port until bootstrap is complete and answers ordinary
//! requests with HTTP 503. When Cloud Node is configured for inbound
//! replication, the same short-lived listener also exposes deliberately hidden
//! mutual-authentication and sync-negotiation endpoints used during recovery.
//! Its stdin is a parent-owned lifetime pipe: if the parent exits or
//! intentionally closes the pipe, the helper shuts down and releases the port.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use cloud_node::{
    CloudNodeAuthenticator, CloudNodeSettings, CloudNodeStore, Frame, FrameKind, KNOCK_PATH,
    MAX_AUTH_PROOF_BYTES, SESSION_PROOF_HEADER, SETTINGS_FILE_NAME, SYNC_PATH,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, Command};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HELPER_LIFETIME: Duration = Duration::from_secs(60 * 60);
const MAX_SYNC_HELLO_BYTES: usize = 4096;
const MAINTENANCE_MARKER: &str = "x-rbe-maintenance";
const BODY: &str =
    r#"{"ok":false,"status":"maintenance","message":"NOT AVAILABLE TRY AGAIN LATER"}"#;

pub struct MaintenanceNoticeProcess {
    child: Child,
    lease: Option<ChildStdin>,
    host: String,
    port: u16,
}

struct CloudNodeRuntime {
    authenticator: CloudNodeAuthenticator,
    store: CloudNodeStore,
}

#[derive(Clone)]
struct MaintenanceState {
    cloud_node: Option<Arc<CloudNodeRuntime>>,
}

impl MaintenanceNoticeProcess {
    pub async fn spawn(host: &str, port: u16) -> anyhow::Result<Self> {
        let exe = std::env::current_exe().map_err(|err| {
            anyhow::anyhow!("could not resolve backend executable for maintenance responder: {err}")
        })?;
        let port_arg = port.to_string();
        let mut child = Command::new(exe)
            .arg("--maintenance-notice")
            .arg("--maintenance-host")
            .arg(host)
            .arg("--maintenance-port")
            .arg(&port_arg)
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!("failed to spawn temporary maintenance responder: {err}")
            })?;
        let lease = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdin lifetime pipe was not created")
        })?;

        let mut process = Self {
            child,
            lease: Some(lease),
            host: host.to_string(),
            port,
        };
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Close the lifetime pipe first so the helper can release the listener
    /// cleanly. If it does not exit quickly, force-kill it rather than delaying
    /// the real backend's bind indefinitely.
    pub async fn stop(mut self) {
        self.lease.take();
        match tokio::time::timeout(HANDOFF_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => {
                if !status.success() {
                    tracing::warn!(%status, "maintenance responder exited non-zero during handoff");
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "failed while waiting for maintenance responder to exit");
            }
            Err(_) => {
                tracing::warn!(
                    "maintenance responder did not release the port in time; force-killing it"
                );
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
    }

    async fn wait_until_ready(&mut self) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!("maintenance responder exited before binding the API port: {status}");
            }

            if probe(&self.host, self.port).await.unwrap_or(false) {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for maintenance responder to bind {}:{}",
                    self.host,
                    self.port
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Entry point for `backend(.exe) --maintenance-notice`.
pub async fn run(host: String, port: u16) -> anyhow::Result<()> {
    let cloud_node = load_cloud_node_runtime()?;
    if let Some(runtime) = &cloud_node {
        eprintln!(
            "backend maintenance responder enabled Cloud Node boot recovery for {} trusted peer(s)",
            runtime.authenticator.trusted_peer_count()
        );
    }
    let state = Arc::new(MaintenanceState { cloud_node });
    let app = Router::new()
        .route(KNOCK_PATH, any(cloud_node_knock))
        .route(SYNC_PATH, any(cloud_node_sync))
        .fallback(maintenance_response)
        .with_state(state);

    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|err| {
            anyhow::anyhow!("maintenance responder failed to bind {host}:{port}: {err}")
        })?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::thread::Builder::new()
        .name("rbe-maintenance-parent-watch".into())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut byte = [0u8; 1];
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    Ok(_) => {}
                }
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to start maintenance parent watcher: {err}"))?;

    eprintln!(
        "backend maintenance responder listening on {} (pid={})",
        listener.local_addr()?,
        std::process::id()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await
        .map_err(|error| anyhow::anyhow!("maintenance responder server failed: {error}"))
}

async fn cloud_node_knock(
    State(state): State<Arc<MaintenanceState>>,
    request: Request,
) -> Response {
    if request.method() != Method::POST {
        return hidden_not_found();
    }
    let Some(runtime) = &state.cloud_node else {
        return hidden_not_found();
    };
    if !octet_stream_request(&request) || advertised_body_too_large(&request, MAX_AUTH_PROOF_BYTES) {
        return hidden_not_found();
    }

    let body = match axum::body::to_bytes(request.into_body(), MAX_AUTH_PROOF_BYTES).await {
        Ok(body) => body,
        Err(_) => return hidden_not_found(),
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return hidden_not_found(),
    };
    let accepted = match runtime.authenticator.accept_knock(&body, now_ms) {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::debug!(error = %error, "rejected hidden Cloud Node boot authentication");
            return hidden_not_found();
        }
    };

    octet_stream_response(accepted.response)
}

async fn cloud_node_sync(
    State(state): State<Arc<MaintenanceState>>,
    request: Request,
) -> Response {
    if request.method() != Method::POST {
        return hidden_not_found();
    }
    let Some(runtime) = &state.cloud_node else {
        return hidden_not_found();
    };
    if !octet_stream_request(&request) || advertised_body_too_large(&request, MAX_SYNC_HELLO_BYTES) {
        return hidden_not_found();
    }

    let proof = match request
        .headers()
        .get(SESSION_PROOF_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if value.len() <= MAX_AUTH_PROOF_BYTES.saturating_mul(2) => value,
        _ => return hidden_not_found(),
    };
    let proof = match hex::decode(proof) {
        Ok(proof) if proof.len() <= MAX_AUTH_PROOF_BYTES => proof,
        _ => return hidden_not_found(),
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return hidden_not_found(),
    };
    let session = match runtime.authenticator.authorize_session_proof(&proof, now_ms) {
        Ok(session) => session,
        Err(error) => {
            tracing::debug!(error = %error, "rejected hidden Cloud Node sync session proof");
            return hidden_not_found();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), MAX_SYNC_HELLO_BYTES).await {
        Ok(body) => body,
        Err(_) => return hidden_not_found(),
    };
    let frame = match Frame::decode(&body) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::debug!(error = %error, "rejected malformed Cloud Node sync negotiation frame");
            return hidden_not_found();
        }
    };
    if frame.kind != FrameKind::SyncHello || frame.session != session.session {
        return hidden_not_found();
    }
    let remote_header = match cloud_node::SyncPlanHeader::decode(&frame.payload) {
        Ok(header) => header,
        Err(error) => {
            tracing::debug!(error = %error, "rejected malformed Cloud Node sync plan header");
            return hidden_not_found();
        }
    };
    let local_header = match runtime.store.sync_plan().and_then(|plan| plan.header()) {
        Ok(header) => header,
        Err(error) => {
            tracing::error!(error = %error, "Cloud Node could not compute local recovery root");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    tracing::info!(
        peer = %session.node_id,
        remote_root = %hex::encode(remote_header.root_sha256),
        local_root = %hex::encode(local_header.root_sha256),
        roots_match = remote_header.root_sha256 == local_header.root_sha256,
        "authenticated Cloud Node sync negotiation completed"
    );

    let response = Frame {
        kind: FrameKind::SyncHello,
        session: session.session,
        payload: local_header.encode(),
    };
    match response.encode() {
        Ok(encoded) => octet_stream_response(encoded),
        Err(error) => {
            tracing::error!(error = %error, "Cloud Node could not encode sync negotiation response");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn maintenance_response() -> Response {
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, BODY).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    response
        .headers_mut()
        .insert(MAINTENANCE_MARKER, HeaderValue::from_static("1"));
    response
        .headers_mut()
        .insert("x-rbe-backend-state", HeaderValue::from_static("starting"));
    response
}

fn octet_stream_response(body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn octet_stream_request(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
}

fn advertised_body_too_large(request: &Request, maximum: usize) -> bool {
    request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > maximum)
}

fn hidden_not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn load_cloud_node_runtime() -> anyhow::Result<Option<Arc<CloudNodeRuntime>>> {
    let (path, explicit) = cloud_node_settings_path()?;
    if !path.is_file() {
        if explicit {
            anyhow::bail!(
                "RBE_CN_SETTINGS points to a missing Cloud Node settings file: {}",
                path.display()
            );
        }
        return Ok(None);
    }

    let settings = CloudNodeSettings::load(&path).map_err(|error| {
        anyhow::anyhow!(
            "failed to load Cloud Node settings {}: {error}",
            path.display()
        )
    })?;
    if settings.replication.targets.is_empty() {
        return Ok(None);
    }
    let authenticator = CloudNodeAuthenticator::from_env(&settings)?;
    let store = CloudNodeStore::open(&settings)?;
    Ok(Some(Arc::new(CloudNodeRuntime {
        authenticator,
        store,
    })))
}

fn cloud_node_settings_path() -> anyhow::Result<(PathBuf, bool)> {
    if let Some(path) = std::env::var_os("RBE_CN_SETTINGS") {
        return Ok((PathBuf::from(path), true));
    }
    let exe = std::env::current_exe().map_err(|error| {
        anyhow::anyhow!("could not resolve backend executable for Cloud Node settings: {error}")
    })?;
    let parent = exe.parent().ok_or_else(|| {
        anyhow::anyhow!("backend executable has no parent directory for Cloud Node settings")
    })?;
    Ok((parent.join(SETTINGS_FILE_NAME), false))
}

async fn shutdown_signal(mut shutdown_rx: tokio::sync::watch::Receiver<bool>) {
    let parent_shutdown = async {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = parent_shutdown => {}
        _ = tokio::time::sleep(MAX_HELPER_LIFETIME) => {
            eprintln!("maintenance responder reached maximum lifetime and is shutting down");
        }
    }
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}

async fn probe(host: &str, port: u16) -> anyhow::Result<bool> {
    let probe_host = match host.trim() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    };
    let mut stream = tokio::time::timeout(
        Duration::from_millis(500),
        TcpStream::connect((probe_host, port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("maintenance readiness connect timed out"))??;

    stream
        .write_all(b"GET /__rbe_maintenance_probe HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = [0u8; 1024];
    let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut response))
        .await
        .map_err(|_| anyhow::anyhow!("maintenance readiness response timed out"))??;
    Ok(String::from_utf8_lossy(&response[..read])
        .to_ascii_lowercase()
        .contains("x-rbe-maintenance: 1"))
}
