//! RBE `.service` compiler/runtime primitives.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

#[derive(Debug, Clone)]
pub struct ServiceFile {
    pub path: PathBuf,
    pub name: String,
    pub title: String,
    pub restart: RestartPolicy,
    pub memory_limit_mb: u64,
    pub startup_timeout_ms: u64,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct ServiceDefaults {
    pub memory_limit_mb: u64,
    pub startup_timeout_ms: u64,
    pub monitor_interval_ms: u64,
    pub max_restart_backoff_ms: u64,
}

impl Default for ServiceDefaults {
    fn default() -> Self {
        Self {
            memory_limit_mb: 256,
            startup_timeout_ms: 10_000,
            monitor_interval_ms: 1_000,
            max_restart_backoff_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServiceCompileError {
    pub code: &'static str,
    pub path: PathBuf,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ServiceCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}:{}: {}",
            self.code,
            self.path.display(),
            self.line,
            self.message
        )
    }
}

impl std::error::Error for ServiceCompileError {}

#[derive(Debug, Clone)]
pub struct ServiceCompileErrors(pub Vec<ServiceCompileError>);

impl ServiceCompileErrors {
    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn first(&self) -> Option<&ServiceCompileError> {
        self.0.first()
    }
}

impl std::fmt::Display for ServiceCompileErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} service compiler error(s)", self.0.len())
    }
}

impl std::error::Error for ServiceCompileErrors {}

#[derive(Debug, Clone)]
pub struct ServiceCatalog {
    services: Vec<ServiceFile>,
    monitor_interval_ms: u64,
    max_restart_backoff_ms: u64,
}

impl ServiceCatalog {
    pub fn services(&self) -> &[ServiceFile] {
        &self.services
    }

    pub fn compile_dir(
        dir: &Path,
        defaults: ServiceDefaults,
    ) -> Result<Self, ServiceCompileErrors> {
        let mut paths = Vec::new();
        if let Err(error) = collect(dir, &mut paths) {
            return Err(ServiceCompileErrors(vec![compile_error(
                "SVC1000",
                dir,
                1,
                format!("failed to scan service directory: {error}"),
            )]));
        }
        paths.sort();

        let mut names = HashSet::new();
        let mut services = Vec::new();
        let mut errors = Vec::new();
        for path in paths {
            match parse_service(&path, defaults) {
                Ok(service) if names.insert(service.name.clone()) => services.push(service),
                Ok(service) => errors.push(compile_error(
                    "SVC1006",
                    &path,
                    1,
                    format!("duplicate service name {:?}", service.name),
                )),
                Err(error) => errors.push(error),
            }
        }

        if errors.is_empty() {
            Ok(Self {
                services,
                monitor_interval_ms: defaults.monitor_interval_ms,
                max_restart_backoff_ms: defaults.max_restart_backoff_ms,
            })
        } else {
            Err(ServiceCompileErrors(errors))
        }
    }
}

fn compile_error(
    code: &'static str,
    path: &Path,
    line: usize,
    message: String,
) -> ServiceCompileError {
    ServiceCompileError {
        code,
        path: path.to_path_buf(),
        line,
        message,
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("service") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_service(
    path: &Path,
    defaults: ServiceDefaults,
) -> Result<ServiceFile, ServiceCompileError> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| compile_error("SVC1001", path, 1, error.to_string()))?;
    let (body, offset) = directive(&source, ":service[").ok_or_else(|| {
        compile_error(
            "SVC1002",
            path,
            1,
            "missing :service[...] declaration".into(),
        )
    })?;
    let line = source[..offset].lines().count().max(1);
    let fields =
        key_values(&body).map_err(|message| compile_error("SVC1003", path, line, message))?;
    let name = fields
        .get("name")
        .map(|value| unquote(value))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| compile_error("SVC1004", path, line, "service name is required".into()))?;
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(compile_error(
            "SVC1005",
            path,
            line,
            "service name contains unsupported characters".into(),
        ));
    }

    let title = fields
        .get("title")
        .map(|value| unquote(value))
        .unwrap_or_else(|| name.clone());
    let restart = match fields
        .get("restart")
        .map(|value| unquote(value).to_ascii_lowercase())
    {
        None => RestartPolicy::OnFailure,
        Some(value) if value == "always" => RestartPolicy::Always,
        Some(value) if value == "on-failure" || value == "on_failure" => RestartPolicy::OnFailure,
        Some(value) if value == "never" => RestartPolicy::Never,
        Some(value) => {
            return Err(compile_error(
                "SVC1007",
                path,
                line,
                format!("invalid restart policy {value:?}"),
            ));
        }
    };

    let number = |key: &str, fallback: u64| -> Result<u64, ServiceCompileError> {
        match fields.get(key) {
            None => Ok(fallback),
            Some(value) => unquote(value).parse().map_err(|_| {
                compile_error(
                    "SVC1008",
                    path,
                    line,
                    format!("{key} must be an unsigned integer"),
                )
            }),
        }
    };

    let mut imports = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(":import[") {
        let start = cursor + relative;
        if let Some((value, _)) = directive(&source[start..], ":import[") {
            imports.extend(entries(&value).into_iter().map(|entry| unquote(&entry)));
        }
        cursor = (start + 8).min(source.len());
    }

    let exports = source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("export function ")?;
            let name: String = rest
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect();

    Ok(ServiceFile {
        path: path.to_path_buf(),
        name,
        title,
        restart,
        memory_limit_mb: number("memoryLimitMb", defaults.memory_limit_mb)?,
        startup_timeout_ms: number("startupTimeoutMs", defaults.startup_timeout_ms)?,
        imports,
        exports,
    })
}

fn directive(source: &str, prefix: &str) -> Option<(String, usize)> {
    let start = source.find(prefix)?;
    let begin = start + prefix.len();
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in source[begin..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if character == ']' && quote.is_none() {
            return Some((source[begin..begin + index].to_string(), start));
        }
    }
    None
}

fn entries(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in body.chars() {
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            current.push(character);
            continue;
        }
        if (character == ',' || character == '\n') && quote.is_none() {
            if !current.trim().is_empty() {
                out.push(current.trim().to_string());
            }
            current.clear();
        } else {
            current.push(character);
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn key_values(body: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for entry in entries(body) {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("expected key = value, got {entry:?}"))?;
        if out.insert(key.trim().into(), value.trim().into()).is_some() {
            return Err(format!("duplicate field {:?}", key.trim()));
        }
    }
    Ok(out)
}

fn unquote(input: &str) -> String {
    let value = input.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

#[derive(Default)]
pub struct ServiceMemory {
    values: RwLock<HashMap<String, Value>>,
}

impl ServiceMemory {
    pub fn get(&self, key: &str) -> Option<Value> {
        self.values.read().ok()?.get(key).cloned()
    }

    pub fn set(&self, key: String, value: Value) {
        if let Ok(mut map) = self.values.write() {
            map.insert(key, value);
        }
    }

    pub fn delete(&self, key: &str) -> bool {
        self.values
            .write()
            .map(|mut map| map.remove(key).is_some())
            .unwrap_or(false)
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.values.write() {
            map.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.values.read().map(|map| map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceReady {
    pub service: String,
    pub pid: u32,
    pub address: SocketAddr,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServiceRequest {
    Health {
        token: String,
    },
    Call {
        token: String,
        function: String,
        args: Vec<Value>,
    },
    MemoryGet {
        token: String,
        key: String,
    },
    MemorySet {
        token: String,
        key: String,
        value: Value,
    },
    MemoryDelete {
        token: String,
        key: String,
    },
    MemoryClear {
        token: String,
    },
    Shutdown {
        token: String,
    },
}

impl ServiceRequest {
    fn token(&self) -> &str {
        match self {
            Self::Health { token }
            | Self::Call { token, .. }
            | Self::MemoryGet { token, .. }
            | Self::MemorySet { token, .. }
            | Self::MemoryDelete { token, .. }
            | Self::MemoryClear { token }
            | Self::Shutdown { token } => token,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ServiceResponse {
    Ok { value: Value },
    Error { code: String, message: String },
}

const RESTART_BASE_DELAY_MS: u64 = 250;
const SERVICE_STABLE_WINDOW: Duration = Duration::from_secs(60);

struct Managed {
    file: ServiceFile,
    child: Child,
    alias: PathBuf,
    ready: ServiceReady,
    token: String,
    started_at: Instant,
    restart_attempts: u32,
    exit_observed: bool,
}

#[derive(Clone, Default)]
pub struct ServiceManager {
    services: Arc<AsyncRwLock<HashMap<String, Arc<Mutex<Managed>>>>>,
    shutting_down: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRuntimeState {
    Running,
    Restarting,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub name: String,
    pub title: String,
    pub pid: Option<u32>,
    pub state: ServiceRuntimeState,
    pub restart: RestartPolicy,
    pub restart_attempts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceCallError {
    #[error("unknown service {service:?}")]
    Unknown { service: String },
    #[error("service {service:?} is unavailable")]
    Unavailable { service: String },
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
    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::default();
        for file in catalog.services() {
            let managed = spawn(file.clone()).await?;
            manager
                .services
                .write()
                .await
                .insert(file.name.clone(), Arc::new(Mutex::new(managed)));
        }
        if !catalog.services().is_empty() {
            manager.start_monitor(
                Duration::from_millis(catalog.monitor_interval_ms.max(50)),
                Duration::from_millis(catalog.max_restart_backoff_ms.max(RESTART_BASE_DELAY_MS)),
            );
        }
        Ok(manager)
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
            let status = match service.child.try_wait() {
                Ok(Some(status)) => status,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        service = %service.file.name,
                        error = %error,
                        "failed to inspect .service child status"
                    );
                    continue;
                }
            };

            if service.exit_observed {
                return;
            }

            if !should_restart(service.file.restart, status.success()) {
                service.exit_observed = true;
                tracing::warn!(
                    service = %service.file.name,
                    pid = service.ready.pid,
                    %status,
                    restart = ?service.file.restart,
                    "service process exited and restart policy leaves it stopped"
                );
                return;
            }

            if service.started_at.elapsed() >= SERVICE_STABLE_WINDOW {
                service.restart_attempts = 0;
            }
            service.restart_attempts = service.restart_attempts.saturating_add(1);
            let attempt = service.restart_attempts;
            let delay = restart_delay(attempt, max_restart_backoff);
            let file = service.file.clone();
            let old_pid = service.ready.pid;
            service.started_at = Instant::now();
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

            match spawn(file.clone()).await {
                Ok(mut replacement) => {
                    replacement.restart_attempts = attempt;
                    let new_pid = replacement.ready.pid;
                    let mut service = handle.lock().await;
                    if self.shutting_down.load(Ordering::Acquire) {
                        drop(service);
                        stop_managed(&mut replacement).await;
                        return;
                    }
                    *service = replacement;
                    tracing::info!(
                        service = %file.name,
                        old_pid,
                        new_pid,
                        attempt,
                        "service process restarted"
                    );
                }
                Err(error) => {
                    let mut service = handle.lock().await;
                    if self.shutting_down.load(Ordering::Acquire) {
                        return;
                    }
                    service.restart_attempts = attempt;
                    service.started_at = Instant::now();
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
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ServiceCallError::Unavailable {
                service: service_name.to_string(),
            });
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

        let (address, token) = {
            let mut service = handle.lock().await;
            match service.child.try_wait() {
                Ok(None) => (service.ready.address, service.token.clone()),
                Ok(Some(_)) => {
                    return Err(ServiceCallError::Unavailable {
                        service: service_name.to_string(),
                    });
                }
                Err(error) => {
                    return Err(ServiceCallError::Inspect {
                        service: service_name.to_string(),
                        message: error.to_string(),
                    });
                }
            }
        };

        let response = rpc(
            address,
            ServiceRequest::Call {
                token,
                function: function.to_string(),
                args,
            },
        )
        .await
        .map_err(|error| ServiceCallError::Ipc {
            service: service_name.to_string(),
            message: error.to_string(),
        })?;

        map_call_response(service_name, response)
    }

    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {
        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        for handle in handles {
            let mut service = handle.lock().await;
            let (pid, state) = match service.child.try_wait() {
                Ok(None) => (service.child.id(), ServiceRuntimeState::Running),
                Ok(Some(status))
                    if service.exit_observed
                        || !should_restart(service.file.restart, status.success()) =>
                {
                    (None, ServiceRuntimeState::Stopped)
                }
                Ok(Some(_)) => (None, ServiceRuntimeState::Restarting),
                Err(_) => (None, ServiceRuntimeState::Unknown),
            };
            out.push(ServiceSnapshot {
                name: service.file.name.clone(),
                title: service.file.title.clone(),
                pid,
                state,
                restart: service.file.restart,
                restart_attempts: service.restart_attempts,
            });
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        out
    }

    pub async fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handle in handles {
            let mut service = handle.lock().await;
            stop_managed(&mut service).await;
        }
    }
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

fn restart_delay(attempt: u32, maximum: Duration) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let factor = 1u32 << exponent;
    Duration::from_millis(RESTART_BASE_DELAY_MS)
        .saturating_mul(factor)
        .min(maximum)
}

async fn stop_managed(service: &mut Managed) {
    let _ = rpc(
        service.ready.address,
        ServiceRequest::Shutdown {
            token: service.token.clone(),
        },
    )
    .await;
    match tokio::time::timeout(Duration::from_secs(3), service.child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(
            service = %service.file.name,
            error = %error,
            "failed while waiting for service shutdown"
        ),
        Err(_) => {
            let _ = service.child.kill().await;
            let _ = service.child.wait().await;
        }
    }
    let _ = std::fs::remove_file(&service.alias);
}

async fn spawn(file: ServiceFile) -> anyhow::Result<Managed> {
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
    let mut child = Command::new(&alias)
        .args(["--service-host", "--service-file"])
        .arg(&file.path)
        .arg("--service-token")
        .arg(&token)
        .current_dir(parent)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("service stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let bytes = tokio::time::timeout(
        Duration::from_millis(file.startup_timeout_ms.max(1)),
        reader.read_line(&mut line),
    )
    .await
    .map_err(|_| anyhow::anyhow!("service {:?} startup timeout", file.name))??;
    if bytes == 0 {
        anyhow::bail!("service {:?} exited before readiness", file.name);
    }
    let ready: ServiceReady = serde_json::from_str(line.trim())?;
    if ready.service != file.name {
        anyhow::bail!("service readiness identity mismatch");
    }
    Ok(Managed {
        file,
        child,
        alias,
        ready,
        token,
        started_at: Instant::now(),
        restart_attempts: 0,
        exit_observed: false,
    })
}

fn process_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
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
    let stream = TcpStream::connect(address).await?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
        .await?;
    write.shutdown().await?;
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("service IPC timeout"))??;
    Ok(serde_json::from_str(line.trim())?)
}

pub async fn run_service_host(
    path: PathBuf,
    token: String,
    defaults: ServiceDefaults,
) -> anyhow::Result<()> {
    let file = parse_service(&path, defaults)?;
    apply_memory_limit(file.memory_limit_mb)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let ready = ServiceReady {
        service: file.name.clone(),
        pid: std::process::id(),
        address: listener.local_addr()?,
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    let memory = ServiceMemory::default();

    loop {
        let (stream, _) = listener.accept().await?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let request: ServiceRequest = serde_json::from_str(line.trim())?;
        let (response, shutdown) = dispatch(request, &file, &token, &memory);
        write
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
        write.shutdown().await?;
        if shutdown {
            return Ok(());
        }
    }
}

fn dispatch(
    request: ServiceRequest,
    file: &ServiceFile,
    token: &str,
    memory: &ServiceMemory,
) -> (ServiceResponse, bool) {
    if request.token() != token {
        return (
            ServiceResponse::Error {
                code: "SVC4001".into(),
                message: "service IPC authentication failed".into(),
            },
            false,
        );
    }
    let ok = |value| (ServiceResponse::Ok { value }, false);
    match request {
        ServiceRequest::Health { .. } => ok(serde_json::json!({
            "ok": true,
            "service": &file.name,
            "pid": std::process::id(),
            "memoryEntries": memory.len()
        })),
        ServiceRequest::MemoryGet { key, .. } => ok(memory.get(&key).unwrap_or(Value::Null)),
        ServiceRequest::MemorySet { key, value, .. } => {
            memory.set(key, value);
            ok(Value::Bool(true))
        }
        ServiceRequest::MemoryDelete { key, .. } => ok(Value::Bool(memory.delete(&key))),
        ServiceRequest::MemoryClear { .. } => {
            memory.clear();
            ok(Value::Bool(true))
        }
        ServiceRequest::Call { function, .. }
            if !file.exports.iter().any(|value| value == &function) =>
        {
            (
                ServiceResponse::Error {
                    code: "SVC4101".into(),
                    message: format!("unknown export {function:?}"),
                },
                false,
            )
        }
        ServiceRequest::Call { function, .. } => (
            ServiceResponse::Error {
                code: "SVC4100".into(),
                message: format!(
                    "export {function:?} is addressable; executable service bodies land with the module/service evaluator"
                ),
            },
            false,
        ),
        ServiceRequest::Shutdown { .. } => (
            ServiceResponse::Ok {
                value: Value::Bool(true),
            },
            true,
        ),
    }
}

#[cfg(unix)]
fn apply_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        return Ok(());
    }
    let bytes = memory_limit_mb.saturating_mul(1024 * 1024) as libc::rlim_t;
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(unix))]
fn apply_memory_limit(_memory_limit_mb: u64) -> anyhow::Result<()> {
    Ok(())
}

pub fn pause_for_interactive_exit() {
    if std::io::stdin().is_terminal() {
        eprint!("Exit? : <enter>");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_round_trip() {
        let memory = ServiceMemory::default();
        memory.set("x".into(), serde_json::json!(7));
        assert_eq!(memory.get("x"), Some(serde_json::json!(7)));
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
    fn restart_backoff_is_exponential_and_capped() {
        let maximum = Duration::from_secs(30);
        assert_eq!(restart_delay(1, maximum), Duration::from_millis(250));
        assert_eq!(restart_delay(2, maximum), Duration::from_millis(500));
        assert_eq!(restart_delay(3, maximum), Duration::from_millis(1_000));
        assert_eq!(restart_delay(32, maximum), maximum);
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

    #[test]
    fn call_response_maps_success_and_remote_error() {
        assert_eq!(
            map_call_response(
                "cache",
                ServiceResponse::Ok {
                    value: serde_json::json!(7),
                },
            )
            .unwrap(),
            serde_json::json!(7)
        );

        let error = map_call_response(
            "cache",
            ServiceResponse::Error {
                code: "SVC4100".into(),
                message: "pending evaluator".into(),
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ServiceCallError::Remote { service, code, message }
                if service == "cache"
                    && code == "SVC4100"
                    && message == "pending evaluator"
        ));
    }
}
