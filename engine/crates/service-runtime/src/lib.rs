//! RBE `.service` compiler/runtime primitives.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod manager;
mod mother;

pub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
pub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};
pub use mother::{new_service_mother_token, run_service_mother, ServiceMotherReady};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceMode {
    #[default]
    Resident,
    OnDemand,
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct ServiceFile {
    pub path: PathBuf,
    pub name: String,
    pub title: String,
    pub mode: ServiceMode,
    pub restart: RestartPolicy,
    pub memory_limit_mb: u64,
    pub startup_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
    source_digest: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
pub struct ServiceDefaults {
    pub memory_limit_mb: u64,
    pub startup_timeout_ms: u64,
    pub default_idle_timeout_ms: u64,
    pub monitor_interval_ms: u64,
    pub max_restart_backoff_ms: u64,
}

impl Default for ServiceDefaults {
    fn default() -> Self {
        Self {
            memory_limit_mb: 256,
            startup_timeout_ms: 10_000,
            default_idle_timeout_ms: 300_000,
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

    /// Stable SHA-256 contract for the exact service programs and compiled
    /// policies this backend validated. Service Mother must reproduce this
    /// value before advertising readiness, preventing parent/child boot TOCTOU.
    pub fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"RBE_SERVICE_CATALOG_V1");
        fingerprint_u64(&mut digest, self.monitor_interval_ms);
        fingerprint_u64(&mut digest, self.max_restart_backoff_ms);
        fingerprint_u64(&mut digest, self.services.len() as u64);
        for service in &self.services {
            fingerprint_field(&mut digest, service.name.as_bytes());
            fingerprint_field(&mut digest, service.title.as_bytes());
            fingerprint_field(&mut digest, service_mode_label(service.mode).as_bytes());
            fingerprint_field(
                &mut digest,
                restart_policy_label(service.restart).as_bytes(),
            );
            fingerprint_u64(&mut digest, service.memory_limit_mb);
            fingerprint_u64(&mut digest, service.startup_timeout_ms);
            fingerprint_u64(&mut digest, service.idle_timeout_ms);
            fingerprint_field(&mut digest, &service.source_digest);
        }
        let bytes = digest.finalize();
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
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

fn fingerprint_field(digest: &mut Sha256, value: &[u8]) {
    fingerprint_u64(digest, value.len() as u64);
    digest.update(value);
}

fn fingerprint_u64(digest: &mut Sha256, value: u64) {
    digest.update(value.to_le_bytes());
}

const fn service_mode_label(mode: ServiceMode) -> &'static str {
    match mode {
        ServiceMode::Resident => "resident",
        ServiceMode::OnDemand => "on-demand",
        ServiceMode::Hybrid => "hybrid",
    }
}

const fn restart_policy_label(policy: RestartPolicy) -> &'static str {
    match policy {
        RestartPolicy::Always => "always",
        RestartPolicy::OnFailure => "on-failure",
        RestartPolicy::Never => "never",
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
    let mode = match fields
        .get("mode")
        .map(|value| unquote(value).to_ascii_lowercase())
    {
        None => ServiceMode::Resident,
        Some(value) if value == "resident" => ServiceMode::Resident,
        Some(value) if matches!(value.as_str(), "on-demand" | "on_demand" | "ondemand") => {
            ServiceMode::OnDemand
        }
        Some(value) if value == "hybrid" => ServiceMode::Hybrid,
        Some(value) => {
            return Err(compile_error(
                "SVC1010",
                path,
                line,
                format!("invalid service mode {value:?}"),
            ));
        }
    };

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

    let idle_timeout_ms = number("idleTimeoutMs", defaults.default_idle_timeout_ms)?;
    if mode != ServiceMode::Resident && idle_timeout_ms == 0 {
        return Err(compile_error(
            "SVC1011",
            path,
            line,
            "idleTimeoutMs must be greater than zero for on-demand and hybrid services".into(),
        ));
    }

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
            let line = line.trim_start();
            let rest = line
                .strip_prefix("export function ")
                .or_else(|| line.strip_prefix("export async function "))?;
            let name: String = rest
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect();

    let instances = number("instances", 1)?;
    if instances != 1 {
        return Err(compile_error(
            "SVC1009",
            path,
            line,
            format!("instances={instances} is not supported yet; .service currently requires instances=1"),
        ));
    }

    Ok(ServiceFile {
        path: path.to_path_buf(),
        name,
        title,
        mode,
        restart,
        memory_limit_mb: number("memoryLimitMb", defaults.memory_limit_mb)?,
        startup_timeout_ms: number("startupTimeoutMs", defaults.startup_timeout_ms)?,
        idle_timeout_ms,
        imports,
        exports,
        source_digest: Sha256::digest(source.as_bytes()).into(),
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

#[derive(Clone, Default)]
pub struct ServiceMemory {
    values: Arc<RwLock<HashMap<String, Value>>>,
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
    Event {
        token: String,
        event: Value,
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
            | Self::Event { token, .. }
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

#[derive(Debug, Clone)]
pub struct ServiceExecutionError {
    pub code: String,
    pub message: String,
}

impl ServiceExecutionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub type ServiceExecutionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, ServiceExecutionError>> + Send + 'a>>;

pub type ServiceLifecycleFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Value>, ServiceExecutionError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceLifecycle {
    Start,
    Event,
    Health,
    Stop,
}

impl ServiceLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Event => "event",
            Self::Health => "health",
            Self::Stop => "stop",
        }
    }
}

pub trait ServiceExecutor: Send + Sync {
    fn call<'a>(&'a self, function: &'a str, args: Vec<Value>) -> ServiceExecutionFuture<'a>;

    fn lifecycle<'a>(
        &'a self,
        _phase: ServiceLifecycle,
        _argument: Value,
    ) -> ServiceLifecycleFuture<'a> {
        Box::pin(async { Ok(None) })
    }
}

struct AddressableOnlyExecutor;

impl ServiceExecutor for AddressableOnlyExecutor {
    fn call<'a>(&'a self, function: &'a str, _args: Vec<Value>) -> ServiceExecutionFuture<'a> {
        Box::pin(async move {
            Err(ServiceExecutionError::new(
                "SVC4100",
                format!(
                    "export {function:?} is addressable; executable service bodies require a service executor"
                ),
            ))
        })
    }
}

pub async fn run_service_host(
    path: PathBuf,
    token: String,
    defaults: ServiceDefaults,
) -> anyhow::Result<()> {
    run_service_host_with_executor(path, token, defaults, Arc::new(AddressableOnlyExecutor)).await
}

pub async fn run_service_host_with_executor(
    path: PathBuf,
    token: String,
    defaults: ServiceDefaults,
    executor: Arc<dyn ServiceExecutor>,
) -> anyhow::Result<()> {
    run_service_host_with_executor_and_memory(
        path,
        token,
        defaults,
        ServiceMemory::default(),
        executor,
    )
    .await
}

pub async fn run_service_host_with_executor_and_memory(
    path: PathBuf,
    token: String,
    defaults: ServiceDefaults,
    memory: ServiceMemory,
    executor: Arc<dyn ServiceExecutor>,
) -> anyhow::Result<()> {
    let file = parse_service(&path, defaults)?;
    apply_memory_limit(file.memory_limit_mb)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    if let Err(error) = executor
        .lifecycle(ServiceLifecycle::Start, lifecycle_context(&file))
        .await
    {
        anyhow::bail!(
            "service {:?} start lifecycle failed with {}: {}",
            file.name,
            error.code,
            error.message
        );
    }
    let ready = ServiceReady {
        service: file.name.clone(),
        pid: std::process::id(),
        address: listener.local_addr()?,
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    let mut parent_liveness = parent_liveness_signal_if_configured()?;
    loop {
        let accepted = match parent_liveness.as_mut() {
            Some(parent_liveness) => {
                tokio::select! {
                    accepted = listener.accept() => Some(accepted),
                    _ = parent_liveness => None,
                }
            }
            None => Some(listener.accept().await),
        };
        let Some(accepted) = accepted else {
            tracing::warn!(
                service = %file.name,
                "service parent liveness pipe closed; stopping orphaned worker"
            );
            if let Err(error) = executor
                .lifecycle(ServiceLifecycle::Stop, lifecycle_context(&file))
                .await
            {
                tracing::warn!(
                    service = %file.name,
                    code = %error.code,
                    message = %error.message,
                    "service stop lifecycle failed after parent loss"
                );
            }
            return Ok(());
        };
        let (stream, peer) = accepted?;
        if !peer.ip().is_loopback() {
            tracing::warn!(%peer, service = %file.name, "service IPC rejected non-loopback peer");
            continue;
        }
        let (read, mut write) = stream.into_split();
        let line = match tokio::time::timeout(
            SERVICE_IPC_TIMEOUT,
            read_bounded_line(read, SERVICE_IPC_REQUEST_MAX_BYTES, "service request"),
        )
        .await
        {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                tracing::warn!(service = %file.name, error = %error, "invalid service IPC frame");
                continue;
            }
            Err(_) => {
                tracing::warn!(service = %file.name, "service IPC request timed out");
                continue;
            }
        };
        let request: ServiceRequest = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(service = %file.name, error = %error, "invalid service IPC JSON");
                continue;
            }
        };
        let (response, shutdown) =
            dispatch(request, &file, &token, &memory, executor.as_ref()).await;
        let mut payload = serde_json::to_vec(&response)?;
        if payload.len().saturating_add(1) > SERVICE_IPC_RESPONSE_MAX_BYTES {
            payload = serde_json::to_vec(&ServiceResponse::Error {
                code: "SVC4002".into(),
                message: "service IPC response exceeded frame limit".into(),
            })?;
        }
        payload.push(b'\n');
        if let Err(error) = tokio::time::timeout(SERVICE_IPC_TIMEOUT, async {
            write.write_all(&payload).await?;
            write.shutdown().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("service IPC response write timed out"))
        .and_then(|result| result.map_err(anyhow::Error::from))
        {
            tracing::warn!(service = %file.name, error = %error, "service IPC response write failed");
        }
        if shutdown {
            return Ok(());
        }
    }
}

async fn dispatch(
    request: ServiceRequest,
    file: &ServiceFile,
    token: &str,
    memory: &ServiceMemory,
    executor: &dyn ServiceExecutor,
) -> (ServiceResponse, bool) {
    if !constant_time_eq(request.token().as_bytes(), token.as_bytes()) {
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
        ServiceRequest::Health { .. } => {
            match executor
                .lifecycle(ServiceLifecycle::Health, lifecycle_context(file))
                .await
            {
                Ok(lifecycle) => {
                    let healthy = lifecycle_health_ok(lifecycle.as_ref());
                    ok(serde_json::json!({
                        "ok": healthy,
                        "service": &file.name,
                        "pid": std::process::id(),
                        "memoryEntries": memory.len(),
                        "lifecycle": lifecycle.unwrap_or(Value::Null)
                    }))
                }
                Err(error) => execution_error_response(error, false),
            }
        }
        ServiceRequest::Event { event, .. } => {
            match executor.lifecycle(ServiceLifecycle::Event, event).await {
                Ok(Some(value)) => ok(value),
                Ok(None) => (
                    ServiceResponse::Error {
                        code: "SVC4304".into(),
                        message: "service does not define Service.event()".into(),
                    },
                    false,
                ),
                Err(error) => execution_error_response(error, false),
            }
        }
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
        ServiceRequest::Call { function, args, .. } => match executor.call(&function, args).await {
            Ok(value) => ok(value),
            Err(error) => execution_error_response(error, false),
        },
        ServiceRequest::Shutdown { .. } => {
            match executor
                .lifecycle(ServiceLifecycle::Stop, lifecycle_context(file))
                .await
            {
                Ok(value) => (
                    ServiceResponse::Ok {
                        value: value.unwrap_or(Value::Bool(true)),
                    },
                    true,
                ),
                Err(error) => execution_error_response(error, true),
            }
        }
    }
}

pub(crate) async fn read_bounded_line<R>(
    reader: R,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<String>
where
    R: AsyncRead + Unpin,
{
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = BufReader::new(reader).take(limit);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        anyhow::bail!("{label} is empty");
    }
    if bytes > max_bytes {
        anyhow::bail!("{label} exceeded {max_bytes} bytes");
    }
    if !line.ends_with('\n') {
        anyhow::bail!("{label} is not newline terminated");
    }
    Ok(line)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&left, &right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn execution_error_response(
    error: ServiceExecutionError,
    shutdown: bool,
) -> (ServiceResponse, bool) {
    (
        ServiceResponse::Error {
            code: error.code,
            message: error.message,
        },
        shutdown,
    )
}

fn lifecycle_context(file: &ServiceFile) -> Value {
    serde_json::json!({
        "service": &file.name,
        "pid": std::process::id(),
        "mode": file.mode,
    })
}

fn lifecycle_health_ok(value: Option<&Value>) -> bool {
    match value {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(Value::Object(fields)) => fields.get("ok").and_then(Value::as_bool).unwrap_or(true),
        _ => true,
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

fn validate_parent_bootstrap_secret(secret: &str) -> anyhow::Result<()> {
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("parent bootstrap secret must be a 256-bit hexadecimal value");
    }
    Ok(())
}

fn read_parent_bootstrap_secret<R: BufRead>(reader: &mut R, label: &str) -> anyhow::Result<String> {
    // 64 hex bytes plus optional CR and required LF. `take` prevents a
    // malformed parent from making the child allocate an unbounded line.
    let mut limited = reader.take(66);
    let mut line = String::new();
    let bytes = limited.read_line(&mut line)?;
    if bytes == 0 {
        anyhow::bail!("{label} parent bootstrap pipe closed before authentication");
    }
    if !line.ends_with('\n') {
        anyhow::bail!("{label} parent bootstrap secret is not newline terminated");
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    validate_parent_bootstrap_secret(&line)?;
    Ok(line)
}

/// Read a one-time authentication value from the same inherited stdin pipe
/// that remains open afterward as the parent-liveness signal. Returns `None`
/// for direct/manual process launches that did not configure that pipe.
pub fn read_parent_bootstrap_secret_if_configured(label: &str) -> anyhow::Result<Option<String>> {
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_none() {
        return Ok(None);
    }
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    read_parent_bootstrap_secret(&mut stdin, label).map(Some)
}

/// Send the one-time child authentication value without exposing it in the
/// process command line or environment. The caller must retain `writer` after
/// this returns so EOF continues to mean parent death to the child.
pub async fn write_parent_bootstrap_secret<W>(writer: &mut W, secret: &str) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    validate_parent_bootstrap_secret(secret)?;
    writer.write_all(secret.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

pub fn parent_liveness_signal_if_configured(
) -> anyhow::Result<Option<tokio::sync::oneshot::Receiver<()>>> {
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_none() {
        return Ok(None);
    }

    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("rbe-parent-liveness".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            let mut buffer = [0u8; 64];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        let _ = sender.send(());
                        return;
                    }
                    Ok(_) => {}
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("spawn parent liveness watcher: {error}"))?;
    Ok(Some(receiver))
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

    fn test_service_path(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-service-runtime-{name}-{}-{nonce}.service",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn service_frame_reader_rejects_oversized_and_unterminated_frames() {
        assert!(read_bounded_line(&b"abcd\n"[..], 3, "test").await.is_err());
        assert!(read_bounded_line(&b"abc"[..], 3, "test").await.is_err());
        assert_eq!(
            read_bounded_line(&b"abc\n"[..], 4, "test").await.unwrap(),
            "abc\n"
        );
    }

    #[test]
    fn parent_bootstrap_secret_reader_is_bounded_and_validates_token() {
        let token = "ab".repeat(32);
        let mut valid = std::io::Cursor::new(format!("{token}\n").into_bytes());
        assert_eq!(
            read_parent_bootstrap_secret(&mut valid, "test").unwrap(),
            token
        );

        let mut short = std::io::Cursor::new(b"abcd\n".to_vec());
        assert!(read_parent_bootstrap_secret(&mut short, "test").is_err());

        let mut oversized = std::io::Cursor::new(format!("{}\n", "a".repeat(80)).into_bytes());
        assert!(read_parent_bootstrap_secret(&mut oversized, "test").is_err());
    }

    #[test]
    fn service_token_compare_checks_content_and_length() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu"));
        assert!(!constant_time_eq(b"secret", b"short"));
    }

    #[test]
    fn memory_round_trip() {
        let memory = ServiceMemory::default();
        memory.set("x".into(), serde_json::json!(7));
        assert_eq!(memory.get("x"), Some(serde_json::json!(7)));
    }

    #[test]
    fn parses_hybrid_and_on_demand_modes() {
        let hybrid_path = test_service_path("hybrid");
        std::fs::write(
            &hybrid_path,
            ":service[name = cache, mode = hybrid, idleTimeoutMs = 1234]\nexport function get() {}",
        )
        .unwrap();
        let hybrid = parse_service(&hybrid_path, ServiceDefaults::default()).unwrap();
        assert_eq!(hybrid.mode, ServiceMode::Hybrid);
        assert_eq!(hybrid.idle_timeout_ms, 1234);
        let _ = std::fs::remove_file(&hybrid_path);

        let demand_path = test_service_path("demand");
        std::fs::write(
            &demand_path,
            ":service[name = lazy, mode = on-demand]\nexport function get() {}",
        )
        .unwrap();
        let demand = parse_service(&demand_path, ServiceDefaults::default()).unwrap();
        assert_eq!(demand.mode, ServiceMode::OnDemand);
        assert_eq!(demand.idle_timeout_ms, 300_000);
        let _ = std::fs::remove_file(&demand_path);
    }

    #[test]
    fn resident_is_the_compatibility_default() {
        let path = test_service_path("resident");
        std::fs::write(&path, ":service[name = existing]\nexport function run() {}").unwrap();
        let service = parse_service(&path, ServiceDefaults::default()).unwrap();
        assert_eq!(service.mode, ServiceMode::Resident);
        let _ = std::fs::remove_file(path);
    }

    struct LifecycleTestExecutor;

    impl ServiceExecutor for LifecycleTestExecutor {
        fn call<'a>(&'a self, _function: &'a str, _args: Vec<Value>) -> ServiceExecutionFuture<'a> {
            Box::pin(async { Ok(Value::Null) })
        }

        fn lifecycle<'a>(
            &'a self,
            phase: ServiceLifecycle,
            argument: Value,
        ) -> ServiceLifecycleFuture<'a> {
            Box::pin(async move {
                Ok(Some(serde_json::json!({
                    "phase": phase.as_str(),
                    "argument": argument
                })))
            })
        }
    }

    #[tokio::test]
    async fn dispatch_routes_health_event_and_stop_lifecycle() {
        let path = test_service_path("lifecycle-dispatch");
        std::fs::write(
            &path,
            ":service[name = lifecycle]\nexport function get() {}",
        )
        .unwrap();
        let file = parse_service(&path, ServiceDefaults::default()).unwrap();
        let memory = ServiceMemory::default();
        let executor = LifecycleTestExecutor;

        let (health, shutdown) = dispatch(
            ServiceRequest::Health {
                token: "secret".into(),
            },
            &file,
            "secret",
            &memory,
            &executor,
        )
        .await;
        assert!(!shutdown);
        let ServiceResponse::Ok { value } = health else {
            panic!("health lifecycle should succeed");
        };
        assert_eq!(value["lifecycle"]["phase"], "health");

        let (event, shutdown) = dispatch(
            ServiceRequest::Event {
                token: "secret".into(),
                event: serde_json::json!({"kind": "refresh"}),
            },
            &file,
            "secret",
            &memory,
            &executor,
        )
        .await;
        assert!(!shutdown);
        let ServiceResponse::Ok { value } = event else {
            panic!("event lifecycle should succeed");
        };
        assert_eq!(value["phase"], "event");
        assert_eq!(value["argument"]["kind"], "refresh");

        let (stop, shutdown) = dispatch(
            ServiceRequest::Shutdown {
                token: "secret".into(),
            },
            &file,
            "secret",
            &memory,
            &executor,
        )
        .await;
        assert!(shutdown);
        let ServiceResponse::Ok { value } = stop else {
            panic!("stop lifecycle should succeed");
        };
        assert_eq!(value["phase"], "stop");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn service_catalog_fingerprint_tracks_source_and_compiled_policy() {
        let root = std::env::temp_dir().join(format!(
            "rbe-service-fingerprint-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let service = root.join("demo.service");
        std::fs::write(
            &service,
            ":service[name = demo]\nexport function run() { return 1; }\n",
        )
        .unwrap();
        let first = ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        let same = ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);

        std::fs::write(
            &service,
            ":service[name = demo]\nexport function run() { return 2; }\n",
        )
        .unwrap();
        let changed_source =
            ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        assert_ne!(first.fingerprint(), changed_source.fingerprint());

        let mut defaults = ServiceDefaults::default();
        defaults.monitor_interval_ms += 1;
        let changed_policy = ServiceCatalog::compile_dir(&root, defaults).unwrap();
        assert_ne!(changed_source.fingerprint(), changed_policy.fingerprint());
        let _ = std::fs::remove_dir_all(root);
    }
}
