from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return source.replace(old, new, 1)


# Dynamic shared Service Mother endpoint so all ServiceManager clones can be
# atomically retargeted after a supervised process replacement.
path = Path("crates/service-runtime/src/mother.rs")
source = path.read_text()
source = replace_once(
    source,
    "use tokio::net::{TcpListener, TcpStream};",
    "use tokio::net::{TcpListener, TcpStream};\nuse tokio::sync::RwLock;",
    "mother RwLock import",
)
old_client = '''#[derive(Clone)]
pub(crate) struct ServiceMotherClient {
    address: SocketAddr,
    token: Arc<str>,
}

impl ServiceMotherClient {
    pub(crate) fn new(address: SocketAddr, token: String) -> Self {
        Self {
            address,
            token: Arc::<str>::from(token),
        }
    }
'''
new_client = '''#[derive(Clone)]
pub(crate) struct ServiceMotherClient {
    endpoint: Arc<RwLock<Option<ServiceMotherEndpoint>>>,
}

#[derive(Clone)]
struct ServiceMotherEndpoint {
    address: SocketAddr,
    token: Arc<str>,
}

impl ServiceMotherClient {
    pub(crate) fn new(address: SocketAddr, token: String) -> Self {
        Self {
            endpoint: Arc::new(RwLock::new(Some(ServiceMotherEndpoint {
                address,
                token: Arc::<str>::from(token),
            }))),
        }
    }

    pub(crate) async fn replace(&self, address: SocketAddr, token: String) -> anyhow::Result<()> {
        validate_mother_endpoint(address, &token)?;
        *self.endpoint.write().await = Some(ServiceMotherEndpoint {
            address,
            token: Arc::<str>::from(token),
        });
        Ok(())
    }

    pub(crate) async fn invalidate(&self) {
        *self.endpoint.write().await = None;
    }

    async fn current(&self) -> Option<ServiceMotherEndpoint> {
        self.endpoint.read().await.clone()
    }
'''
source = replace_once(source, old_client, new_client, "dynamic ServiceMotherClient")

# Rewrite each client operation to snapshot the current endpoint before RPC.
source = replace_once(
    source,
    '''        let response = mother_rpc(
            self.address,
            ServiceMotherRequest::Call {
                token: self.token.to_string(),''',
    '''        let endpoint = self.current().await.ok_or_else(|| ServiceCallError::Unavailable {
            service: service.to_string(),
        })?;
        let response = mother_rpc(
            endpoint.address,
            ServiceMotherRequest::Call {
                token: endpoint.token.to_string(),''',
    "mother call dynamic endpoint",
)
source = replace_once(
    source,
    '''        let response = mother_rpc(
            self.address,
            ServiceMotherRequest::Event {
                token: self.token.to_string(),''',
    '''        let endpoint = self.current().await.ok_or_else(|| ServiceCallError::Unavailable {
            service: service.to_string(),
        })?;
        let response = mother_rpc(
            endpoint.address,
            ServiceMotherRequest::Event {
                token: endpoint.token.to_string(),''',
    "mother event dynamic endpoint",
)
source = replace_once(
    source,
    '''        match mother_rpc(
            self.address,
            ServiceMotherRequest::Snapshot {
                token: self.token.to_string(),''',
    '''        let endpoint = self
            .current()
            .await
            .ok_or_else(|| anyhow::anyhow!("Service Mother is restarting"))?;
        match mother_rpc(
            endpoint.address,
            ServiceMotherRequest::Snapshot {
                token: endpoint.token.to_string(),''',
    "mother snapshot dynamic endpoint",
)
source = replace_once(
    source,
    '''        match mother_rpc(
            self.address,
            ServiceMotherRequest::Shutdown {
                token: self.token.to_string(),''',
    '''        let Some(endpoint) = self.current().await else {
            return Ok(());
        };
        match mother_rpc(
            endpoint.address,
            ServiceMotherRequest::Shutdown {
                token: endpoint.token.to_string(),''',
    "mother shutdown dynamic endpoint",
)

helper_anchor = "pub fn new_service_mother_token() -> String {\n"
helper = '''fn validate_mother_endpoint(address: SocketAddr, token: &str) -> anyhow::Result<()> {
    if !address.ip().is_loopback() {
        anyhow::bail!("Service Mother endpoint must be loopback");
    }
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("Service Mother authentication value must be 256-bit hexadecimal");
    }
    Ok(())
}

'''
source = replace_once(source, helper_anchor, helper + helper_anchor, "mother endpoint validator")

tests_anchor = "    #[test]\n    fn mother_tokens_are_256_bit_hex() {"
tests = '''    #[tokio::test]
    async fn mother_client_retarget_is_shared_across_clones() {
        let first = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10001);
        let second = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10002);
        let client = ServiceMotherClient::new(first, "a".repeat(64));
        let clone = client.clone();
        client.invalidate().await;
        assert!(clone.current().await.is_none());
        client.replace(second, "b".repeat(64)).await.unwrap();
        let endpoint = clone.current().await.unwrap();
        assert_eq!(endpoint.address, second);
        assert_eq!(endpoint.token.as_ref(), "b".repeat(64));
    }

    #[test]
    fn mother_tokens_are_256_bit_hex() {'''
source = replace_once(source, tests_anchor, tests, "mother client shared endpoint test")
path.write_text(source)


# Public ServiceManager methods used only by the trusted backend supervisor.
path = Path("crates/service-runtime/src/manager.rs")
source = path.read_text()
remote_anchor = '''    pub fn remote(address: SocketAddr, auth: String) -> anyhow::Result<Self> {
        if !address.ip().is_loopback() {
            anyhow::bail!("Service Mother endpoint must be loopback");
        }
        if auth.len() != 64 || !auth.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("Service Mother authentication value must be 256-bit hexadecimal");
        }
        Ok(Self {
            mother: Some(ServiceMotherClient::new(address, auth)),
            ..Self::default()
        })
    }
'''
remote_replacement = remote_anchor + '''
    pub async fn replace_remote(&self, address: SocketAddr, auth: String) -> anyhow::Result<()> {
        let mother = self
            .mother
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ServiceManager is not backed by Service Mother"))?;
        mother.replace(address, auth).await
    }

    pub async fn invalidate_remote(&self) {
        if let Some(mother) = &self.mother {
            mother.invalidate().await;
        }
    }
'''
source = replace_once(source, remote_anchor, remote_replacement, "ServiceManager remote retarget API")
path.write_text(source)


# Turn the backend-side process handle into a supervised process with bounded
# exponential restart. The raw child spawn can reuse/retarget an existing
# ServiceManager so AppState clones keep working across Mother replacement.
path = Path("crates/backend/src/service_mother.rs")
source = path.read_text()
source = replace_once(
    source,
    "use std::time::Duration;",
    "use std::time::{Duration, Instant};",
    "Service Mother Instant import",
)
source = replace_once(
    source,
    "pub struct ServiceMotherProcess {",
    '''const MOTHER_RESTART_BASE_DELAY: Duration = Duration::from_millis(250);
const MOTHER_RESTART_MAX_DELAY: Duration = Duration::from_secs(30);
const MOTHER_STABLE_WINDOW: Duration = Duration::from_secs(60);

pub struct ServiceMotherProcess {''',
    "Service Mother restart constants",
)
source = replace_once(
    source,
    '''    _liveness: ChildStdin,
    alias: PathBuf,
}''',
    '''    _liveness: ChildStdin,
    alias: PathBuf,
    started_at: Instant,
}

pub struct ServiceMotherSupervisor {
    manager: ServiceManager,
    shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    task: tokio::task::JoinHandle<()>,
}

impl ServiceMotherSupervisor {
    pub fn manager(&self) -> ServiceManager {
        self.manager.clone()
    }

    pub async fn shutdown(mut self, timeout: Duration) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(timeout);
        }
        match tokio::time::timeout(timeout, &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(error = %error, "Service Mother supervisor task failed"),
            Err(_) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "Service Mother supervisor exceeded shutdown budget; aborting task"
                );
                self.task.abort();
            }
        }
    }
}''',
    "ServiceMotherSupervisor type",
)

# Existing public spawn becomes supervisor; raw spawn is moved to spawn_process.
source = replace_once(
    source,
    "pub async fn spawn(settings_path: impl AsRef<Path>) -> anyhow::Result<ServiceMotherProcess> {",
    "async fn spawn_process(\n    settings_path: impl AsRef<Path>,\n    existing_manager: Option<&ServiceManager>,\n) -> anyhow::Result<ServiceMotherProcess> {",
    "raw Service Mother spawn rename",
)
source = replace_once(
    source,
    '''    let manager = ServiceManager::remote(ready.address, token)?;
    tracing::info!(''',
    '''    let manager = match existing_manager {
        Some(manager) => {
            manager.replace_remote(ready.address, token).await?;
            manager.clone()
        }
        None => ServiceManager::remote(ready.address, token)?,
    };
    tracing::info!(''',
    "replacement ServiceManager retarget",
)
source = replace_once(
    source,
    '''        _liveness: liveness,
        alias,
    })
}

fn flag_value''',
    '''        _liveness: liveness,
        alias,
        started_at: Instant::now(),
    })
}

pub async fn spawn(settings_path: impl AsRef<Path>) -> anyhow::Result<ServiceMotherSupervisor> {
    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
        format!(
            "canonicalize Service Mother supervisor settings path {}",
            settings_path.as_ref().display()
        )
    })?;
    let initial = spawn_process(&settings_path, None).await?;
    let manager = initial.manager();
    let supervisor_manager = manager.clone();
    let supervisor_settings = settings_path.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<Duration>();
    let task = tokio::spawn(async move {
        supervise(initial, supervisor_settings, supervisor_manager, &mut shutdown_rx).await;
    });
    Ok(ServiceMotherSupervisor {
        manager,
        shutdown: Some(shutdown_tx),
        task,
    })
}

async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    manager: ServiceManager,
    shutdown_rx: &mut tokio::sync::oneshot::Receiver<Duration>,
) {
    let mut restart_attempts = 0u32;
    loop {
        tokio::select! {
            shutdown = &mut *shutdown_rx => {
                let timeout = shutdown.unwrap_or(Duration::from_secs(5));
                process.shutdown(timeout).await;
                return;
            }
            status = process.child.wait() => {
                let uptime = process.started_at.elapsed();
                let alias = process.alias.clone();
                let _ = std::fs::remove_file(alias);
                manager.invalidate_remote().await;
                match status {
                    Ok(status) => tracing::warn!(%status, uptime_ms = uptime.as_millis(), "Service Mother exited unexpectedly; supervising replacement"),
                    Err(error) => tracing::warn!(error = %error, uptime_ms = uptime.as_millis(), "failed watching Service Mother; supervising replacement"),
                }
                if uptime >= MOTHER_STABLE_WINDOW {
                    restart_attempts = 0;
                }
            }
        }

        loop {
            restart_attempts = restart_attempts.saturating_add(1);
            let delay = mother_restart_delay(restart_attempts);
            tracing::warn!(
                attempt = restart_attempts,
                backoff_ms = delay.as_millis(),
                "scheduling Service Mother replacement"
            );
            tokio::select! {
                shutdown = &mut *shutdown_rx => {
                    let _ = shutdown;
                    return;
                }
                _ = tokio::time::sleep(delay) => {}
            }

            match spawn_process(&settings_path, Some(&manager)).await {
                Ok(replacement) => {
                    tracing::info!(
                        attempt = restart_attempts,
                        "Service Mother replacement ready; shared service endpoint retargeted"
                    );
                    process = replacement;
                    break;
                }
                Err(error) => tracing::error!(
                    attempt = restart_attempts,
                    error = %error,
                    "Service Mother replacement failed"
                ),
            }
        }
    }
}

fn mother_restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = MOTHER_RESTART_BASE_DELAY
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(
        millis
            .saturating_mul(factor)
            .min(MOTHER_RESTART_MAX_DELAY.as_millis() as u64),
    )
}

fn flag_value''',
    "Service Mother supervisor implementation",
)

# Tests for bounded restart timing (no process spawn required).
tests = '''
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mother_restart_backoff_is_exponential_and_capped() {
        assert_eq!(mother_restart_delay(1), Duration::from_millis(250));
        assert_eq!(mother_restart_delay(2), Duration::from_millis(500));
        assert_eq!(mother_restart_delay(3), Duration::from_millis(1000));
        assert_eq!(mother_restart_delay(30), MOTHER_RESTART_MAX_DELAY);
    }
}
'''
source = source.rstrip() + "\n" + tests
path.write_text(source)
