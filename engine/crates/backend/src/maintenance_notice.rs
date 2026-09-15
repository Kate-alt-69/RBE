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

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use cloud_node::{
    AuthenticatedSession, CloudNodeAuthenticator, CloudNodeRecoveryReceiver, CloudNodeSettings,
    CloudNodeStore, Frame, FrameKind, SyncPlanHeader, TransferChunk, KNOCK_PATH,
    MAX_AUTH_PROOF_BYTES, MAX_FRAME_BYTES, SESSION_PROOF_HEADER, SETTINGS_FILE_NAME, SYNC_PATH,
    TRANSFER_PATH,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HELPER_LIFETIME: Duration = Duration::from_secs(60 * 60);
const MAX_SYNC_HELLO_BYTES: usize = 4096;
const MAX_TRANSFER_REQUEST_BYTES: usize = MAX_FRAME_BYTES + 32;
const CONTROL_PREFIX: &str = "RBE-CN-CONTROL/1";
const CONTROL_COMPLETE_PREFIX: &str = "RBE-CN-CONTROL/1 complete ";
const CONTROL_POLICY_TIMEOUT: Duration = Duration::from_secs(5);
const MAINTENANCE_MARKER: &str = "x-rbe-maintenance";
const BODY: &str =
    r#"{"ok":false,"status":"maintenance","message":"NOT AVAILABLE TRY AGAIN LATER"}"#;

#[derive(Debug, Clone, Copy)]
enum BootRecoveryAdmission {
    Disabled,
    Optional,
    Required { timeout: Duration },
}

pub struct MaintenanceNoticeProcess {
    child: Child,
    lease: Option<ChildStdin>,
    control: BufReader<ChildStdout>,
    recovery_admission: BootRecoveryAdmission,
    host: String,
    port: u16,
}

struct RecoveryState {
    expected: SyncPlanHeader,
    receiver: CloudNodeRecoveryReceiver,
}

struct CloudNodeRuntime {
    authenticator: CloudNodeAuthenticator,
    store: CloudNodeStore,
    recoveries: Mutex<HashMap<[u8; 16], RecoveryState>>,
    recovery_admission: BootRecoveryAdmission,
    recovery_complete: AtomicBool,
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
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!("failed to spawn temporary maintenance responder: {err}")
            })?;
        let lease = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdin lifetime pipe was not created")
        })?;
        let control = child.stdout.take().ok_or_else(|| {
            anyhow::anyhow!("maintenance responder stdout control pipe was not created")
        })?;

        let mut process = Self {
            child,
            lease: Some(lease),
            control: BufReader::new(control),
            recovery_admission: BootRecoveryAdmission::Disabled,
            host: host.to_string(),
            port,
        };
        process.recovery_admission = process.read_admission_policy().await?;
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn wait_for_required_cloud_node_recovery(&mut self) -> anyhow::Result<()> {
        let timeout = match self.recovery_admission {
            BootRecoveryAdmission::Disabled | BootRecoveryAdmission::Optional => return Ok(()),
            BootRecoveryAdmission::Required { timeout } => timeout,
        };
        tracing::info!(
            timeout_ms = timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            "waiting for required Cloud Node boot recovery before runtime admission"
        );
        let line = self.read_control_line(timeout).await?;
        let Some(root) = line.strip_prefix(CONTROL_COMPLETE_PREFIX) else {
            anyhow::bail!("maintenance responder returned unexpected control message {line:?}");
        };
        if root.len() != 64 || !root.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("maintenance responder returned an invalid Cloud Node recovery root");
        }
        tracing::info!(root = %root, "required Cloud Node boot recovery completed");
        Ok(())
    }

    async fn read_admission_policy(&mut self) -> anyhow::Result<BootRecoveryAdmission> {
        let line = self.read_control_line(CONTROL_POLICY_TIMEOUT).await?;
        if line == format!("{CONTROL_PREFIX} disabled") {
            return Ok(BootRecoveryAdmission::Disabled);
        }
        if line == format!("{CONTROL_PREFIX} optional") {
            return Ok(BootRecoveryAdmission::Optional);
        }
        if let Some(value) = line.strip_prefix(&format!("{CONTROL_PREFIX} required ")) {
            let timeout_ms = value.parse::<u64>().map_err(|_| {
                anyhow::anyhow!("maintenance responder returned an invalid recovery timeout")
            })?;
            return Ok(BootRecoveryAdmission::Required {
                timeout: Duration::from_millis(timeout_ms),
            });
        }
        anyhow::bail!("maintenance responder returned invalid control policy {line:?}")
    }

    async fn read_control_line(&mut self, timeout: Duration) -> anyhow::Result<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.control.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow::anyhow!("timed out waiting for maintenance responder control message")
            })??;
        if read == 0 {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "maintenance responder exited while waiting for Cloud Node control message: {status}"
                );
            }
            anyhow::bail!("maintenance responder closed its Cloud Node control pipe");
        }
        Ok(line.trim().to_owned())
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
    emit_boot_recovery_policy(cloud_node.as_deref())?;
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
        .route(TRANSFER_PATH, any(cloud_node_transfer))
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
    if !octet_stream_request(&request) || advertised_body_too_large(&request, MAX_AUTH_PROOF_BYTES)
    {
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

async fn cloud_node_sync(State(state): State<Arc<MaintenanceState>>, request: Request) -> Response {
    if request.method() != Method::POST {
        return hidden_not_found();
    }
    let Some(runtime) = &state.cloud_node else {
        return hidden_not_found();
    };
    if !octet_stream_request(&request) || advertised_body_too_large(&request, MAX_SYNC_HELLO_BYTES)
    {
        return hidden_not_found();
    }
    let Some(session) = authorize_cloud_node_session(runtime, &request, "sync") else {
        return hidden_not_found();
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
    let remote_header = match SyncPlanHeader::decode(&frame.payload) {
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
    let mut advertised_header = local_header;
    let local_integrity_ok = if remote_header == local_header {
        match runtime.store.verify() {
            Ok(objects) => {
                tracing::debug!(
                    objects,
                    "Cloud Node equal-root snapshot passed integrity verification"
                );
                true
            }
            Err(error) => {
                advertised_header.root_sha256[0] ^= 0x80;
                tracing::warn!(
                    peer = %session.node_id,
                    error = %error,
                    "Cloud Node local snapshot failed integrity verification; forcing full recovery"
                );
                false
            }
        }
    } else {
        true
    };
    let roots_match = remote_header == local_header && local_integrity_ok;

    if roots_match {
        if let Err(error) = signal_boot_recovery_complete(runtime, local_header) {
            tracing::error!(error = %error, "Cloud Node could not signal completed boot recovery");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }

    if !roots_match {
        let mut recoveries = match runtime.recoveries.lock() {
            Ok(recoveries) => recoveries,
            Err(_) => {
                tracing::error!("Cloud Node recovery session table is poisoned");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
        match recoveries.get(&session.session) {
            Some(existing) if existing.expected != remote_header => {
                tracing::warn!(peer = %session.node_id, "Cloud Node peer changed its negotiated recovery root");
                return StatusCode::CONFLICT.into_response();
            }
            Some(_) => {}
            None => {
                if !recoveries.is_empty() {
                    tracing::warn!(peer = %session.node_id, "Cloud Node recovery is already owned by another authenticated session");
                    return StatusCode::CONFLICT.into_response();
                }
                let receiver = match CloudNodeRecoveryReceiver::open(
                    &runtime.store,
                    session.session,
                ) {
                    Ok(receiver) => receiver,
                    Err(error) => {
                        tracing::error!(error = %error, "Cloud Node could not create recovery staging tree");
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                };
                recoveries.insert(
                    session.session,
                    RecoveryState {
                        expected: remote_header,
                        receiver,
                    },
                );
            }
        }
    }

    tracing::info!(
        peer = %session.node_id,
        remote_root = %hex::encode(remote_header.root_sha256),
        local_root = %hex::encode(local_header.root_sha256),
        advertised_root = %hex::encode(advertised_header.root_sha256),
        local_integrity_ok,
        roots_match,
        "authenticated Cloud Node sync negotiation completed"
    );

    let response = Frame {
        kind: FrameKind::SyncHello,
        session: session.session,
        payload: advertised_header.encode(),
    };
    encode_cloud_node_response(response, "sync negotiation")
}

async fn cloud_node_transfer(
    State(state): State<Arc<MaintenanceState>>,
    request: Request,
) -> Response {
    if request.method() != Method::POST {
        return hidden_not_found();
    }
    let Some(runtime) = &state.cloud_node else {
        return hidden_not_found();
    };
    if !octet_stream_request(&request)
        || advertised_body_too_large(&request, MAX_TRANSFER_REQUEST_BYTES)
    {
        return hidden_not_found();
    }
    let Some(session) = authorize_cloud_node_session(runtime, &request, "transfer") else {
        return hidden_not_found();
    };

    let body = match axum::body::to_bytes(request.into_body(), MAX_TRANSFER_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => return hidden_not_found(),
    };
    let frame = match Frame::decode(&body) {
        Ok(frame) if frame.session == session.session => frame,
        Ok(_) => return hidden_not_found(),
        Err(error) => {
            tracing::debug!(error = %error, "rejected malformed Cloud Node transfer frame");
            return hidden_not_found();
        }
    };

    match frame.kind {
        FrameKind::ObjectChunk => {
            let chunk = match TransferChunk::from_frame(&frame) {
                Ok(chunk) => chunk,
                Err(error) => {
                    tracing::debug!(error = %error, "rejected malformed Cloud Node object chunk");
                    return StatusCode::CONFLICT.into_response();
                }
            };
            let receipt = {
                let mut recoveries = match runtime.recoveries.lock() {
                    Ok(recoveries) => recoveries,
                    Err(_) => {
                        tracing::error!("Cloud Node recovery session table is poisoned");
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                };
                let Some(recovery) = recoveries.get_mut(&session.session) else {
                    return StatusCode::CONFLICT.into_response();
                };
                match recovery.receiver.accept_chunk(&chunk) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        tracing::warn!(
                            peer = %session.node_id,
                            error = %error,
                            "Cloud Node rejected recovery object bytes"
                        );
                        return StatusCode::CONFLICT.into_response();
                    }
                }
            };
            if receipt.committed {
                tracing::debug!(
                    peer = %session.node_id,
                    object = %hex::encode(chunk.object_key),
                    resource = ?chunk.resource,
                    duplicate = receipt.duplicate,
                    video_reconstructed = receipt.video_reconstructed,
                    "Cloud Node committed staged recovery resource"
                );
            }
            encode_cloud_node_response(
                Frame {
                    kind: FrameKind::ObjectChunk,
                    session: session.session,
                    payload: Vec::new(),
                },
                "object transfer acknowledgement",
            )
        }
        FrameKind::SyncComplete => {
            let supplied = match SyncPlanHeader::decode(&frame.payload) {
                Ok(header) => header,
                Err(error) => {
                    tracing::debug!(error = %error, "rejected malformed Cloud Node completion header");
                    return StatusCode::CONFLICT.into_response();
                }
            };
            let actual = {
                let mut recoveries = match runtime.recoveries.lock() {
                    Ok(recoveries) => recoveries,
                    Err(_) => {
                        tracing::error!("Cloud Node recovery session table is poisoned");
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                };
                let Some(recovery) = recoveries.get_mut(&session.session) else {
                    return StatusCode::CONFLICT.into_response();
                };
                if recovery.expected != supplied {
                    tracing::warn!(peer = %session.node_id, "Cloud Node completion root differs from negotiated root");
                    return StatusCode::CONFLICT.into_response();
                }
                match recovery
                    .receiver
                    .complete(&runtime.store, recovery.expected)
                {
                    Ok(actual) => actual,
                    Err(error) => {
                        tracing::error!(
                            peer = %session.node_id,
                            error = %error,
                            "Cloud Node recovery snapshot verification failed"
                        );
                        return StatusCode::CONFLICT.into_response();
                    }
                }
            };
            if let Err(error) = signal_boot_recovery_complete(runtime, actual) {
                tracing::error!(error = %error, "Cloud Node could not signal activated boot recovery");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if let Ok(mut recoveries) = runtime.recoveries.lock() {
                recoveries.remove(&session.session);
            }
            tracing::info!(
                peer = %session.node_id,
                root = %hex::encode(actual.root_sha256),
                "Cloud Node recovery snapshot verified and activated"
            );
            encode_cloud_node_response(
                Frame {
                    kind: FrameKind::SyncComplete,
                    session: session.session,
                    payload: actual.encode(),
                },
                "recovery completion",
            )
        }
        _ => hidden_not_found(),
    }
}

fn authorize_cloud_node_session(
    runtime: &CloudNodeRuntime,
    request: &Request,
    operation: &str,
) -> Option<AuthenticatedSession> {
    let proof = request
        .headers()
        .get(SESSION_PROOF_HEADER)
        .and_then(|value| value.to_str().ok())?;
    if proof.len() > MAX_AUTH_PROOF_BYTES.saturating_mul(2) {
        return None;
    }
    let proof = hex::decode(proof).ok()?;
    if proof.len() > MAX_AUTH_PROOF_BYTES {
        return None;
    }
    let now_ms = now_ms().ok()?;
    match runtime
        .authenticator
        .authorize_session_proof(&proof, now_ms)
    {
        Ok(session) => Some(session),
        Err(error) => {
            tracing::debug!(error = %error, operation, "rejected hidden Cloud Node session proof");
            None
        }
    }
}

fn emit_boot_recovery_policy(runtime: Option<&CloudNodeRuntime>) -> anyhow::Result<()> {
    let line = match runtime.map(|runtime| runtime.recovery_admission) {
        None | Some(BootRecoveryAdmission::Disabled) => format!("{CONTROL_PREFIX} disabled"),
        Some(BootRecoveryAdmission::Optional) => format!("{CONTROL_PREFIX} optional"),
        Some(BootRecoveryAdmission::Required { timeout }) => format!(
            "{CONTROL_PREFIX} required {}",
            timeout.as_millis().min(u128::from(u64::MAX)) as u64
        ),
    };
    write_control_line(&line)
}

fn signal_boot_recovery_complete(
    runtime: &CloudNodeRuntime,
    header: SyncPlanHeader,
) -> anyhow::Result<()> {
    if !matches!(
        runtime.recovery_admission,
        BootRecoveryAdmission::Required { .. }
    ) {
        return Ok(());
    }
    if runtime
        .recovery_complete
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    let result = write_control_line(&format!(
        "{CONTROL_COMPLETE_PREFIX}{}",
        hex::encode(header.root_sha256)
    ));
    if result.is_err() {
        runtime.recovery_complete.store(false, Ordering::Release);
    }
    result
}

fn write_control_line(line: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn encode_cloud_node_response(frame: Frame, operation: &str) -> Response {
    match frame.encode() {
        Ok(encoded) => octet_stream_response(encoded),
        Err(error) => {
            tracing::error!(error = %error, operation, "Cloud Node could not encode response");
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
    let recovery_admission = if settings.replication.require_boot_recovery {
        BootRecoveryAdmission::Required {
            timeout: Duration::from_millis(settings.replication.boot_recovery_timeout_ms),
        }
    } else {
        BootRecoveryAdmission::Optional
    };
    let authenticator = CloudNodeAuthenticator::from_env(&settings)?;
    let store = CloudNodeStore::open(&settings)?;
    Ok(Some(Arc::new(CloudNodeRuntime {
        authenticator,
        store,
        recoveries: Mutex::new(HashMap::new()),
        recovery_admission,
        recovery_complete: AtomicBool::new(false),
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
