use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

use super::{
    read_bounded_line, RestartPolicy, ServiceCatalog, ServiceFabricEndpoint, ServiceFile,
    ServiceMode, ServiceReady, ServiceRequest, ServiceResponse, SERVICE_IPC_REQUEST_MAX_BYTES,
    SERVICE_IPC_RESPONSE_MAX_BYTES, SERVICE_IPC_TIMEOUT,
};
use crate::mother::ServiceMotherClient;

const RESTART_BASE_DELAY_MS: u64 = 250;
const SERVICE_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const SERVICE_SHUTDOWN_DRAIN_POLL: Duration = Duration::from_millis(10);
const SERVICE_STABLE_WINDOW: Duration = Duration::from_secs(60);
const SERVICE_READY_MAX_BYTES: usize = 4 * 1024;
const SERVICE_STDOUT_LINE_MAX_BYTES: usize = 64 * 1024;

struct ServiceProcess {
    child: Child,
    _liveness: ChildStdin,
    alias: PathBuf,
    ready: ServiceReady,
    token: String,
    started_at: Instant,
}

enum ServiceOperation {
    Call { function: String, args: Vec<Value> },
    Event { event: Value },
}

impl ServiceOperation {
    fn into_request(self, token: String) -> ServiceRequest {
        match self {
            Self::Call { function, args } => ServiceRequest::Call {
                token,
                function,
                args,
            },
            Self::Event { event } => ServiceRequest::Event { token, event },
        }
    }
}

struct Managed {
    file: ServiceFile,
    process: Option<ServiceProcess>,
    restart_attempts: u32,
    exit_observed: bool,
    restarting: bool,
    active_calls: Arc<AtomicU32>,
    last_activity: Instant,
}

impl Managed {
    fn dormant(file: ServiceFile) -> Self {
        Self {
            file,
            process: None,
            restart_attempts: 0,
            exit_observed: false,
            restarting: false,
            active_calls: Arc::new(AtomicU32::new(0)),
            last_activity: Instant::now(),
        }
    }

    fn wakeable(&self) -> bool {
        self.file.mode != ServiceMode::Resident
    }

    fn idle_due(&self) -> bool {
        self.wakeable()
            && self.active_calls.load(Ordering::Acquire) == 0
            && self.last_activity.elapsed()
                >= Duration::from_millis(self.file.idle_timeout_ms.max(1))
    }
}

struct ActiveCallGuard {
    counter: Arc<AtomicU32>,
}

impl ActiveCallGuard {
    fn acquire(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        let _ = self
            .counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            });
    }
}

#[derive(Clone, Default)]
pub struct ServiceManager {
    services: Arc<AsyncRwLock<HashMap<String, Arc<Mutex<Managed>>>>>,
    shutting_down: Arc<AtomicBool>,
    mother: Option<ServiceMotherClient>,
    fabric: Option<ServiceFabricEndpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRuntimeState {
    Dormant,
    Running,
    Restarting,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub name: String,
    pub title: String,
    pub pid: Option<u32>,
    pub state: ServiceRuntimeState,
    pub mode: ServiceMode,
    pub restart: RestartPolicy,
    pub restart_attempts: u32,
    pub idle_timeout_ms: u64,
    pub ready: bool,
    pub health_checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceCallError {
    #[error("unknown service {service:?}")]
    Unknown { service: String },
    #[error("service {service:?} is unavailable")]
    Unavailable { service: String },
    #[error("failed to activate service {service:?}: {message}")]
    Activation { service: String, message: String },
    #[error("failed to inspect service {service:?}: {message}")]
    Inspect { service: String, message: String },
    #[error("service {service:?} IPC failed: {message}")]
    Ipc { service: String, message: String },
    #[error("service {service:?} returned {code}: {message}")]
    Remote {
        service: String,
        code: String,
        message: String,
    },
}

impl ServiceManager {
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    pub fn remote(address: SocketAddr, auth: String) -> anyhow::Result<Self> {
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

    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None).await;
        manager.start_prepared(catalog).await?;
        Ok(manager)
    }

    pub async fn prepare_all_with_fabric(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
    ) -> Self {
        Self::prepare_all(catalog, Some(fabric)).await
    }

    async fn prepare_all(catalog: &ServiceCatalog, fabric: Option<ServiceFabricEndpoint>) -> Self {
        let manager = Self {
            fabric,
            ..Self::default()
        };
        let mut services = manager.services.write().await;
        for file in catalog.services() {
            services.insert(
                file.name.clone(),
                Arc::new(Mutex::new(Managed::dormant(file.clone()))),
            );
        }
        drop(services);
        manager
    }

    /// Start resident/hybrid children after every service identity is already
    /// addressable by Mother. Direct service dependencies are started first so
    /// lifecycle hooks can synchronously call an already-running dependency.
    pub async fn start_prepared(&self, catalog: &ServiceCatalog) -> anyhow::Result<()> {
        for file in service_startup_order(catalog)? {
            if file.mode == ServiceMode::OnDemand {
                continue;
            }
            let process = match spawn_process(&file, self.fabric.as_ref()).await {
                Ok(process) => process,
                Err(error) => {
                    self.shutdown_all().await;
                    return Err(error);
                }
            };
            let handle = self
                .services
                .read()
                .await
                .get(&file.name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("prepared service {:?} disappeared", file.name))?;
            let mut managed = handle.lock().await;
            managed.process = Some(process);
            managed.restart_attempts = 0;
            managed.exit_observed = false;
            managed.restarting = false;
            managed.last_activity = Instant::now();
        }
        if !catalog.services().is_empty() {
            self.start_monitor(
                Duration::from_millis(catalog.monitor_interval_ms.max(50)),
                Duration::from_millis(catalog.max_restart_backoff_ms.max(RESTART_BASE_DELAY_MS)),
            );
        }
        Ok(())
    }

    fn start_monitor(&self, interval: Duration, max_restart_backoff: Duration) {
        let manager = self.clone();
        tokio::spawn(async move {
            let handles = manager
                .services
                .read()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for handle in handles {
                let manager = manager.clone();
                tokio::spawn(async move {
                    manager
                        .monitor_service(handle, interval, max_restart_backoff)
                        .await;
                });
            }
        });
    }

    async fn monitor_service(
        &self,
        handle: Arc<Mutex<Managed>>,
        interval: Duration,
        max_restart_backoff: Duration,
    ) {
        loop {
            tokio::time::sleep(interval).await;
            if self.shutting_down.load(Ordering::Acquire) {
                return;
            }

            let mut service = handle.lock().await;
            let status = match service.process.as_mut() {
                None => continue,
                Some(process) => match process.child.try_wait() {
                    Ok(Some(status)) => Some(status),
                    Ok(None) => None,
                    Err(error) => {
                        tracing::warn!(
                            service = %service.file.name,
                            error = %error,
                            "failed to inspect .service child status"
                        );
                        continue;
                    }
                },
            };

            if status.is_none() {
                if service.idle_due() {
                    let name = service.file.name.clone();
                    let pid = service
                        .process
                        .as_ref()
                        .and_then(|process| process.child.id());
                    let idle_timeout_ms = service.file.idle_timeout_ms;
                    let mut process = service
                        .process
                        .take()
                        .expect("running process checked immediately above");
                    service.restart_attempts = 0;
                    service.exit_observed = false;
                    service.restarting = false;
                    stop_process(&name, &mut process).await;
                    tracing::info!(
                        service = %name,
                        ?pid,
                        idle_timeout_ms,
                        "service entered dormant state after idle timeout"
                    );
                }
                continue;
            }

            let status = status.expect("status checked above");
            let old_pid = service
                .process
                .as_ref()
                .map(|process| process.ready.pid)
                .unwrap_or_default();

            if service.exit_observed {
                continue;
            }

            if !should_restart(service.file.restart, status.success()) {
                service.exit_observed = true;
                service.restarting = false;
                if let Some(process) = service.process.as_ref() {
                    let _ = std::fs::remove_file(&process.alias);
                }
                tracing::warn!(
                    service = %service.file.name,
                    pid = old_pid,
                    %status,
                    restart = ?service.file.restart,
                    "service process exited and restart policy leaves it stopped"
                );
                continue;
            }

            let stable = service
                .process
                .as_ref()
                .map(|process| process.started_at.elapsed() >= SERVICE_STABLE_WINDOW)
                .unwrap_or(false);
            service.restart_attempts =
                next_restart_attempt(service.restart_attempts, stable, service.restarting);
            let attempt = service.restart_attempts;
            let delay = restart_delay(attempt, max_restart_backoff);
            let file = service.file.clone();
            service.restarting = true;
            tracing::warn!(
                service = %file.name,
                pid = old_pid,
                %status,
                attempt,
                backoff_ms = delay.as_millis() as u64,
                "service process exited; scheduling restart"
            );
            drop(service);

            tokio::time::sleep(delay).await;
            if self.shutting_down.load(Ordering::Acquire) {
                return;
            }

            let mut service = handle.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return;
            }
            if !service.restarting {
                tracing::debug!(
                    service = %file.name,
                    attempt,
                    "scheduled service restart was superseded"
                );
                continue;
            }

            let running_pid = if let Some(process) = service.process.as_mut() {
                match process.child.try_wait() {
                    Ok(None) => process.child.id(),
                    Ok(Some(_)) => None,
                    Err(error) => {
                        tracing::warn!(
                            service = %file.name,
                            attempt,
                            error = %error,
                            "failed to re-check service before restart"
                        );
                        continue;
                    }
                }
            } else {
                None
            };
            if let Some(pid) = running_pid {
                service.restarting = false;
                tracing::debug!(
                    service = %file.name,
                    pid,
                    attempt,
                    "scheduled service restart found a running replacement"
                );
                continue;
            }

            match spawn_process(&file, self.fabric.as_ref()).await {
                Ok(replacement) => {
                    let new_pid = replacement.ready.pid;
                    service.process = Some(replacement);
                    service.restart_attempts = attempt;
                    service.exit_observed = false;
                    service.restarting = false;
                    service.last_activity = Instant::now();
                    tracing::info!(
                    service = %file.name,
                    old_pid,
                    new_pid,
                    attempt,
                    "service process restarted"
                              );
                }
                Err(error) => {
                    service.restart_attempts = attempt;
                    service.restarting = true;
                    tracing::error!(
                    service = %file.name,
                    attempt,
                    error = %error,
                    "service restart attempt failed"
                              );
                }
            }
        }
    }

    pub async fn call(
        &self,
        service_name: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ServiceCallError> {
        self.invoke(
            service_name,
            ServiceOperation::Call {
                function: function.to_string(),
                args,
            },
        )
        .await
    }

    pub async fn event(&self, service_name: &str, event: Value) -> Result<Value, ServiceCallError> {
        self.invoke(service_name, ServiceOperation::Event { event })
            .await
    }

    async fn invoke(
        &self,
        service_name: &str,
        operation: ServiceOperation,
    ) -> Result<Value, ServiceCallError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ServiceCallError::Unavailable {
                service: service_name.to_string(),
            });
        }
        if let Some(mother) = &self.mother {
            return match operation {
                ServiceOperation::Call { function, args } => {
                    mother.call(service_name, &function, args).await
                }
                ServiceOperation::Event { event } => mother.event(service_name, event).await,
            };
        }

        let handle = self
            .services
            .read()
            .await
            .get(service_name)
            .cloned()
            .ok_or_else(|| ServiceCallError::Unknown {
                service: service_name.to_string(),
            })?;

        let (address, token, active_call) = {
            let mut service = handle.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(ServiceCallError::Unavailable {
                    service: service_name.to_string(),
                });
            }
            self.activate_for_call(&mut service).await?;
            let process =
                service
                    .process
                    .as_ref()
                    .ok_or_else(|| ServiceCallError::Unavailable {
                        service: service_name.to_string(),
                    })?;
            let address = process.ready.address;
            let token = process.token.clone();
            service.last_activity = Instant::now();
            let active_call = ActiveCallGuard::acquire(service.active_calls.clone());
            (address, token, active_call)
        };

        let request = operation.into_request(token);
        let response = rpc(address, request).await;

        {
            let mut service = handle.lock().await;
            service.last_activity = Instant::now();
        }
        drop(active_call);

        let response = response.map_err(|error| ServiceCallError::Ipc {
            service: service_name.to_string(),
            message: error.to_string(),
        })?;
        map_call_response(service_name, response)
    }

    async fn activate_for_call(&self, service: &mut Managed) -> Result<(), ServiceCallError> {
        let service_name = service.file.name.clone();
        let running = match service.process.as_mut() {
            None => false,
            Some(process) => match process.child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) => false,
                Err(error) => {
                    return Err(ServiceCallError::Inspect {
                        service: service_name,
                        message: error.to_string(),
                    });
                }
            },
        };
        if running {
            return Ok(());
        }

        if !service.wakeable() {
            return Err(ServiceCallError::Unavailable {
                service: service.file.name.clone(),
            });
        }

        let file = service.file.clone();
        service.restarting = true;
        match spawn_process(&file, self.fabric.as_ref()).await {
            Ok(process) => {
                let pid = process.ready.pid;
                service.process = Some(process);
                service.restart_attempts = 0;
                service.exit_observed = false;
                service.restarting = false;
                service.last_activity = Instant::now();
                tracing::info!(
                    service = %file.name,
                    pid,
                    mode = ?file.mode,
                    "service activated on demand"
                );
                Ok(())
            }
            Err(error) => {
                service.restarting = false;
                Err(ServiceCallError::Activation {
                    service: file.name,
                    message: error.to_string(),
                })
            }
        }
    }

    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {
        if let Some(mother) = &self.mother {
            return match mother.snapshot().await {
                Ok(services) => services,
                Err(error) => vec![ServiceSnapshot {
                    name: "service-mother".into(),
                    title: "RBE Service Mother".into(),
                    pid: None,
                    state: ServiceRuntimeState::Unknown,
                    mode: ServiceMode::Resident,
                    restart: RestartPolicy::OnFailure,
                    restart_attempts: 0,
                    idle_timeout_ms: 0,
                    ready: false,
                    health_checked: true,
                    health: None,
                    health_error: Some(error.to_string()),
                }],
            };
        }
        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let probe_health = !self.shutting_down.load(Ordering::Acquire);
        let mut snapshots = tokio::task::JoinSet::new();
        for handle in handles {
            snapshots.spawn(snapshot_managed_service(handle, probe_health));
        }
        let mut out = Vec::new();
        while let Some(result) = snapshots.join_next().await {
            match result {
                Ok(snapshot) => out.push(snapshot),
                Err(error) => tracing::warn!(
                    error = %error,
                    "service snapshot task failed"
                ),
            }
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        out
    }

    pub async fn shutdown_all(&self) {
        self.begin_shutdown();
        if let Some(mother) = &self.mother {
            if let Err(error) = mother.shutdown().await {
                tracing::warn!(error = %error, "Service Mother shutdown RPC failed");
            }
            return;
        }
        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();

        // Acquire every service lock once after raising `shutting_down`. This
        // closes the race with a call that passed the outer check but had not
        // yet acquired its activity guard.
        let mut activity = Vec::with_capacity(handles.len());
        for handle in &handles {
            let service = handle.lock().await;
            activity.push(service.active_calls.clone());
        }
        if !wait_for_service_drain(&activity, SERVICE_SHUTDOWN_DRAIN_TIMEOUT).await {
            tracing::warn!(
                active_calls = total_active_calls(&activity),
                timeout_ms = SERVICE_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                "service shutdown drain timed out; forcing remaining service processes to stop"
            );
        }

        let mut stops = tokio::task::JoinSet::new();
        for handle in handles {
            let process = {
                let mut service = handle.lock().await;
                service.exit_observed = true;
                service.restarting = false;
                service
                    .process
                    .take()
                    .map(|process| (service.file.name.clone(), process))
            };
            if let Some((name, mut process)) = process {
                stops.spawn(async move {
                    stop_process(&name, &mut process).await;
                });
            }
        }
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result {
                tracing::warn!(error = %error, "service shutdown task failed");
            }
        }
    }
}

async fn snapshot_managed_service(
    handle: Arc<Mutex<Managed>>,
    probe_health: bool,
) -> ServiceSnapshot {
    let (mut snapshot, health_target, active_call) = {
        let mut service = handle.lock().await;
        let restarting = service.restarting;
        let wakeable = service.wakeable();
        let exit_observed = service.exit_observed;
        let restart = service.file.restart;
        let (pid, state, health_target) = match service.process.as_mut() {
            None if restarting => (None, ServiceRuntimeState::Restarting, None),
            None if wakeable && !exit_observed => (None, ServiceRuntimeState::Dormant, None),
            None => (None, ServiceRuntimeState::Stopped, None),
            Some(process) => match process.child.try_wait() {
                Ok(None) => (
                    process.child.id(),
                    ServiceRuntimeState::Running,
                    Some((process.ready.address, process.token.clone())),
                ),
                Ok(Some(_)) if restarting => (None, ServiceRuntimeState::Restarting, None),
                Ok(Some(status)) if exit_observed || !should_restart(restart, status.success()) => {
                    (None, ServiceRuntimeState::Stopped, None)
                }
                Ok(Some(_)) => (None, ServiceRuntimeState::Restarting, None),
                Err(_) => (None, ServiceRuntimeState::Unknown, None),
            },
        };
        let health_target = probe_health.then_some(health_target).flatten();
        let active_call = health_target
            .as_ref()
            .map(|_| ActiveCallGuard::acquire(service.active_calls.clone()));
        (
            ServiceSnapshot {
                name: service.file.name.clone(),
                title: service.file.title.clone(),
                pid,
                state,
                mode: service.file.mode,
                restart: service.file.restart,
                restart_attempts: service.restart_attempts,
                idle_timeout_ms: service.file.idle_timeout_ms,
                ready: state == ServiceRuntimeState::Dormant,
                health_checked: false,
                health: None,
                health_error: None,
            },
            health_target,
            active_call,
        )
    };

    if let Some((address, token)) = health_target {
        snapshot.health_checked = true;
        let response = rpc(address, ServiceRequest::Health { token }).await;
        drop(active_call);
        match response {
            Ok(ServiceResponse::Ok { value }) => {
                snapshot.ready = health_value_ready(&value);
                snapshot.health = Some(value);
            }
            Ok(ServiceResponse::Error { code, message }) => {
                snapshot.health_error = Some(format!("{code}: {message}"));
            }
            Err(error) => {
                snapshot.health_error = Some(error.to_string());
            }
        }
    }
    snapshot
}

fn total_active_calls(activity: &[Arc<AtomicU32>]) -> u64 {
    activity
        .iter()
        .map(|counter| u64::from(counter.load(Ordering::Acquire)))
        .sum()
}

async fn wait_for_service_drain(activity: &[Arc<AtomicU32>], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if total_active_calls(activity) == 0 {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(SERVICE_SHUTDOWN_DRAIN_POLL.min(deadline - now)).await;
    }
}

fn health_value_ready(value: &Value) -> bool {
    value.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

fn map_call_response(
    service_name: &str,
    response: ServiceResponse,
) -> Result<Value, ServiceCallError> {
    match response {
        ServiceResponse::Ok { value } => Ok(value),
        ServiceResponse::Error { code, message } => Err(ServiceCallError::Remote {
            service: service_name.to_string(),
            code,
            message,
        }),
    }
}

fn should_restart(policy: RestartPolicy, success: bool) -> bool {
    match policy {
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => !success,
        RestartPolicy::Never => false,
    }
}

fn next_restart_attempt(current: u32, stable: bool, retry_in_progress: bool) -> u32 {
    let base = if stable && !retry_in_progress {
        0
    } else {
        current
    };
    base.saturating_add(1)
}

fn restart_delay(attempt: u32, maximum: Duration) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let factor = 1u32 << exponent;
    Duration::from_millis(RESTART_BASE_DELAY_MS)
        .saturating_mul(factor)
        .min(maximum)
}

fn direct_service_dependency(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("service:")?;
    let name = rest
        .split(|character: char| character == '.' || character.is_whitespace())
        .next()
        .unwrap_or_default()
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn service_startup_order(catalog: &ServiceCatalog) -> anyhow::Result<Vec<ServiceFile>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Visiting,
        Done,
    }

    fn visit(
        name: &str,
        files: &HashMap<String, ServiceFile>,
        state: &mut HashMap<String, Visit>,
        stack: &mut Vec<String>,
        out: &mut Vec<ServiceFile>,
    ) -> anyhow::Result<()> {
        match state.get(name) {
            Some(Visit::Done) => return Ok(()),
            Some(Visit::Visiting) => {
                let start = stack.iter().position(|item| item == name).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(name.to_string());
                anyhow::bail!(
                    "synchronous Service Fabric dependency cycle would deadlock: {}",
                    cycle.join(" -> ")
                );
            }
            None => {}
        }
        let file = files
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown service dependency {name:?}"))?;
        state.insert(name.to_string(), Visit::Visiting);
        stack.push(name.to_string());
        for dependency in file
            .imports
            .iter()
            .filter_map(|raw| direct_service_dependency(raw))
        {
            if !files.contains_key(&dependency) {
                anyhow::bail!(
                    "service {:?} imports unknown service {dependency:?}",
                    file.name
                );
            }
            visit(&dependency, files, state, stack, out)?;
        }
        stack.pop();
        state.insert(name.to_string(), Visit::Done);
        out.push(file.clone());
        Ok(())
    }

    let files = catalog
        .services()
        .iter()
        .cloned()
        .map(|file| (file.name.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut names = files.keys().cloned().collect::<Vec<_>>();
    names.sort();
    let mut state = HashMap::new();
    let mut stack = Vec::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        visit(&name, &files, &mut state, &mut stack, &mut out)?;
    }
    Ok(out)
}

async fn stop_process(service_name: &str, process: &mut ServiceProcess) {
    let _ = rpc(
        process.ready.address,
        ServiceRequest::Shutdown {
            token: process.token.clone(),
        },
    )
    .await;
    match tokio::time::timeout(Duration::from_secs(3), process.child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(
            service = %service_name,
            error = %error,
            "failed while waiting for service shutdown"
        ),
        Err(_) => {
            let _ = process.child.kill().await;
            let _ = process.child.wait().await;
        }
    }
    let _ = std::fs::remove_file(&process.alias);
}

async fn read_bounded_buffered_line<R>(
    reader: &mut R,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(1024));
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("{label} is not newline terminated");
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if bytes.len().saturating_add(consumed) > max_bytes {
            reader.consume(consumed);
            anyhow::bail!("{label} exceeded {max_bytes} bytes");
        }
        bytes.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|error| anyhow::anyhow!("{label} is not valid UTF-8: {error}"));
        }
    }
}

enum ServiceStdoutLine {
    Line(String),
    Oversized,
    InvalidUtf8,
}

async fn read_service_stdout_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<ServiceStdoutLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(1024));
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if bytes.is_empty() && !oversized {
                return Ok(None);
            }
            if oversized {
                return Ok(Some(ServiceStdoutLine::Oversized));
            }
            return Ok(Some(match String::from_utf8(bytes) {
                Ok(line) => ServiceStdoutLine::Line(line),
                Err(_) => ServiceStdoutLine::InvalidUtf8,
            }));
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if !oversized {
            if bytes.len().saturating_add(consumed) > max_bytes {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..consumed]);
            }
        }
        reader.consume(consumed);

        if newline.is_some() {
            if oversized {
                return Ok(Some(ServiceStdoutLine::Oversized));
            }
            return Ok(Some(match String::from_utf8(bytes) {
                Ok(line) => ServiceStdoutLine::Line(line),
                Err(_) => ServiceStdoutLine::InvalidUtf8,
            }));
        }
    }
}

async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
) -> anyhow::Result<ServiceProcess> {
    let exe = std::env::current_exe().context("resolve backend executable")?;
    let parent = exe.parent().context("backend executable has no parent")?;
    let dir = parent.join(".runtime/process");
    std::fs::create_dir_all(&dir)?;
    let extension = exe
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let alias = dir.join(format!(
        "rbe-service-{}-parent-{}{}",
        process_name(&file.name),
        std::process::id(),
        extension
    ));
    let _ = std::fs::remove_file(&alias);
    if std::fs::hard_link(&exe, &alias).is_err() {
        std::fs::copy(&exe, &alias)?;
    }

    let token = random_token();
    let mut command = Command::new(&alias);
    command
        .args(["--service-host", "--service-file"])
        .arg(&file.path);
    if let Some(fabric) = fabric {
        command
            .arg("--service-mother-address")
            .arg(fabric.address().to_string());
    }
    let mut child = match command
        .current_dir(parent)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&alias);
            return Err(error.into());
        }
    };

    let mut liveness = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("service {:?} parent liveness pipe unavailable", file.name);
        }
    };
    if let Err(error) = super::write_parent_bootstrap_secret(&mut liveness, &token).await {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send service {:?} parent bootstrap secret: {error}",
            file.name
        ));
    }
    if let Some(fabric) = fabric {
        if let Err(error) = super::write_parent_bootstrap_secret(&mut liveness, fabric.auth()).await
        {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(anyhow::anyhow!(
                "send service {:?} Service Fabric bootstrap secret: {error}",
                file.name
            ));
        }
    }
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("service {:?} stdout unavailable", file.name);
        }
    };
    let mut reader = BufReader::new(stdout);
    let line = match tokio::time::timeout(
        Duration::from_millis(file.startup_timeout_ms.max(1)),
        read_bounded_buffered_line(&mut reader, SERVICE_READY_MAX_BYTES, "service readiness"),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("service {:?} exited before readiness", file.name);
        }
        Ok(Err(error)) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("service {:?} startup timeout", file.name);
        }
    };
    let ready: ServiceReady = match serde_json::from_str(line.trim()) {
        Ok(ready) => ready,
        Err(error) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(error.into());
        }
    };
    if ready.service != file.name {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("service readiness identity mismatch");
    }
    if !ready.address.ip().is_loopback() {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("service readiness advertised a non-loopback endpoint");
    }
    if child.id() != Some(ready.pid) {
        cleanup_failed_spawn(&alias, &mut child).await;
        anyhow::bail!("service readiness PID does not match child process");
    }

    let service_name = file.name.clone();
    tokio::spawn(async move {
        loop {
            match read_service_stdout_line(&mut reader, SERVICE_STDOUT_LINE_MAX_BYTES).await {
                Ok(None) => return,
                Ok(Some(ServiceStdoutLine::Line(line))) => {
                    let output = line.trim_end();
                    if !output.is_empty() {
                        tracing::info!(service = %service_name, %output, ".service stdout");
                    }
                }
                Ok(Some(ServiceStdoutLine::Oversized)) => tracing::warn!(
                    service = %service_name,
                    max_bytes = SERVICE_STDOUT_LINE_MAX_BYTES,
                    "discarded oversized .service stdout line"
                ),
                Ok(Some(ServiceStdoutLine::InvalidUtf8)) => tracing::warn!(
                    service = %service_name,
                    "discarded non-UTF-8 .service stdout line"
                ),
                Err(error) => {
                    tracing::warn!(service = %service_name, %error, "failed to drain .service stdout");
                    return;
                }
            }
        }
    });

    Ok(ServiceProcess {
        child,
        _liveness: liveness,
        alias,
        ready,
        token,
        started_at: Instant::now(),
    })
}

async fn cleanup_failed_spawn(alias: &Path, child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(alias);
}

fn process_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn rpc(address: SocketAddr, request: ServiceRequest) -> anyhow::Result<ServiceResponse> {
    if !address.ip().is_loopback() {
        anyhow::bail!("service IPC endpoint must be loopback");
    }
    let mut payload = serde_json::to_vec(&request)?;
    if payload.len().saturating_add(1) > SERVICE_IPC_REQUEST_MAX_BYTES {
        anyhow::bail!(
            "service IPC request exceeded {} bytes",
            SERVICE_IPC_REQUEST_MAX_BYTES
        );
    }
    payload.push(b'\n');

    let stream = tokio::time::timeout(SERVICE_IPC_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| anyhow::anyhow!("service IPC connect timeout"))??;
    let (read, mut write) = stream.into_split();
    tokio::time::timeout(SERVICE_IPC_TIMEOUT, async {
        write.write_all(&payload).await?;
        write.shutdown().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("service IPC write timeout"))??;
    let line = tokio::time::timeout(
        SERVICE_IPC_TIMEOUT,
        read_bounded_line(read, SERVICE_IPC_RESPONSE_MAX_BYTES, "service response"),
    )
    .await
    .map_err(|_| anyhow::anyhow!("service IPC response timeout"))??;
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn begin_shutdown_closes_service_call_admission() {
        let manager = ServiceManager::default();
        manager.begin_shutdown();
        let error = manager
            .call("missing", "run", Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ServiceCallError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn service_shutdown_drain_waits_for_active_call() {
        let counter = Arc::new(AtomicU32::new(1));
        let released = counter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            released.store(0, Ordering::Release);
        });
        assert!(wait_for_service_drain(&[counter], Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn service_shutdown_drain_is_bounded() {
        let counter = Arc::new(AtomicU32::new(1));
        assert!(!wait_for_service_drain(&[counter], Duration::from_millis(20)).await);
    }

    #[test]
    fn active_call_guard_releases_counter_when_dropped() {
        let counter = Arc::new(AtomicU32::new(0));
        {
            let _guard = ActiveCallGuard::acquire(counter.clone());
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn service_process_alias_preserves_distinct_legal_names() {
        assert_eq!(process_name("cache.v1"), "cache.v1");
        assert_eq!(process_name("cache-v1"), "cache-v1");
        assert_ne!(process_name("cache.v1"), process_name("cache-v1"));
    }

    #[test]
    fn restart_policy_distinguishes_clean_and_failed_exits() {
        assert!(should_restart(RestartPolicy::Always, true));
        assert!(should_restart(RestartPolicy::Always, false));
        assert!(!should_restart(RestartPolicy::OnFailure, true));
        assert!(should_restart(RestartPolicy::OnFailure, false));
        assert!(!should_restart(RestartPolicy::Never, false));
    }

    #[test]
    fn restart_attempt_only_resets_on_new_stable_exit() {
        assert_eq!(next_restart_attempt(4, true, false), 1);
        assert_eq!(next_restart_attempt(1, true, true), 2);
        assert_eq!(next_restart_attempt(2, false, true), 3);
    }

    #[test]
    fn restart_backoff_is_exponential_and_capped() {
        let maximum = Duration::from_secs(30);
        assert_eq!(restart_delay(1, maximum), Duration::from_millis(250));
        assert_eq!(restart_delay(2, maximum), Duration::from_millis(500));
        assert_eq!(restart_delay(3, maximum), Duration::from_millis(1_000));
        assert_eq!(restart_delay(32, maximum), maximum);
    }

    #[tokio::test]
    async fn service_readiness_reader_is_bounded_and_preserves_following_output() {
        let payload = b"{\"service\":\"demo\"}\nfirst-log-line\n";
        let mut reader = BufReader::new(&payload[..]);
        let readiness = read_bounded_buffered_line(&mut reader, 64, "readiness")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(readiness, "{\"service\":\"demo\"}\n");

        let mut next = String::new();
        reader.read_line(&mut next).await.unwrap();
        assert_eq!(next, "first-log-line\n");

        let oversized = format!("{}\n", "x".repeat(65));
        let mut oversized_reader = BufReader::new(oversized.as_bytes());
        let error = read_bounded_buffered_line(&mut oversized_reader, 64, "readiness")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded 64 bytes"));
    }

    #[tokio::test]
    async fn service_stdout_reader_discards_oversized_lines_and_keeps_draining() {
        let payload = format!("{}\nnext-line\n", "x".repeat(65));
        let mut reader = BufReader::new(payload.as_bytes());
        assert!(matches!(
            read_service_stdout_line(&mut reader, 64).await.unwrap(),
            Some(ServiceStdoutLine::Oversized)
        ));
        match read_service_stdout_line(&mut reader, 64).await.unwrap() {
            Some(ServiceStdoutLine::Line(line)) => assert_eq!(line, "next-line\n"),
            _ => panic!("stdout reader should continue with the next bounded line"),
        }
    }

    #[tokio::test]
    async fn unknown_service_call_is_typed() {
        let error = ServiceManager::default()
            .call("missing", "get", Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceCallError::Unknown { service } if service == "missing"
        ));
    }

    #[tokio::test]
    async fn on_demand_service_snapshot_is_dormant() {
        let manager = ServiceManager::default();
        let file = ServiceFile {
            path: PathBuf::from("test.service"),
            name: "lazy".into(),
            title: "Lazy".into(),
            mode: ServiceMode::OnDemand,
            restart: RestartPolicy::OnFailure,
            memory_limit_mb: 64,
            startup_timeout_ms: 1_000,
            idle_timeout_ms: 5_000,
            imports: Vec::new(),
            exports: vec!["get".into()],
            source_digest: [0; 32],
        };
        manager.services.write().await.insert(
            file.name.clone(),
            Arc::new(Mutex::new(Managed::dormant(file))),
        );

        let snapshots = manager.snapshot().await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].state, ServiceRuntimeState::Dormant);
        assert_eq!(snapshots[0].mode, ServiceMode::OnDemand);
        assert!(snapshots[0].pid.is_none());
        assert!(snapshots[0].ready);
        assert!(!snapshots[0].health_checked);
        assert!(snapshots[0].health.is_none());
        assert!(snapshots[0].health_error.is_none());
    }

    #[test]
    fn health_value_requires_explicit_ok_true() {
        assert!(health_value_ready(&serde_json::json!({"ok": true})));
        assert!(!health_value_ready(&serde_json::json!({"ok": false})));
        assert!(!health_value_ready(&serde_json::json!({})));
    }
}
