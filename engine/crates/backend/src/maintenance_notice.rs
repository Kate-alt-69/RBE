//! Temporary HTTP maintenance responder used while the real backend boots.
//!
//! The normal backend process spawns this same executable in
//! `--maintenance-notice` mode after reclaiming any stale listener. The helper
//! owns the public API port until bootstrap is complete and answers every
//! request with HTTP 503. Its stdin is a parent-owned lifetime pipe: if the
//! parent exits or intentionally closes the pipe, the helper shuts down and
//! releases the port.

use std::io::Read as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, Command};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_HELPER_LIFETIME: Duration = Duration::from_secs(60 * 60);
const MAINTENANCE_MARKER: &str = "X-RBE-Maintenance: 1";
const BODY: &str = r#"{"ok":false,"status":"maintenance","message":"NOT AVAILABLE TRY AGAIN LATER"}"#;

pub struct MaintenanceNoticeProcess {
    child: Child,
    lease: Option<ChildStdin>,
    host: String,
    port: u16,
}

impl MaintenanceNoticeProcess {
    pub async fn spawn(host: &str, port: u16) -> anyhow::Result<Self> {
        let exe = std::env::current_exe()
            .map_err(|err| anyhow::anyhow!("could not resolve backend executable for maintenance responder: {err}"))?;
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
            .map_err(|err| anyhow::anyhow!("failed to spawn temporary maintenance responder: {err}"))?;
        let lease = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("maintenance responder stdin lifetime pipe was not created"))?;

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
                tracing::warn!("maintenance responder did not release the port in time; force-killing it");
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
    let listener = TcpListener::bind((host.as_str(), port)).await.map_err(|err| {
        anyhow::anyhow!("maintenance responder failed to bind {host}:{port}: {err}")
    })?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
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

    let lifetime = tokio::time::sleep(MAX_HELPER_LIFETIME);
    tokio::pin!(lifetime);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(async move {
                    if let Err(err) = serve_maintenance_response(stream).await {
                        tracing::debug!(error = %err, "maintenance response connection ended with error");
                    }
                });
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = &mut lifetime => {
                tracing::warn!("maintenance responder reached maximum lifetime and is shutting down");
                break;
            }
        }
    }

    Ok(())
}

async fn serve_maintenance_response(mut stream: TcpStream) -> std::io::Result<()> {
    // Read enough to let normal HTTP clients finish sending their request line
    // and headers, but never let a slow client hold a temporary responder task.
    let mut request = [0u8; 2048];
    let _ = tokio::time::timeout(REQUEST_READ_TIMEOUT, stream.read(&mut request)).await;

    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nRetry-After: 2\r\n{}\r\nX-RBE-Backend-State: starting\r\nConnection: close\r\n\r\n{}",
        BODY.len(),
        MAINTENANCE_MARKER,
        BODY
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
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
    Ok(String::from_utf8_lossy(&response[..read]).contains(MAINTENANCE_MARKER))
}
