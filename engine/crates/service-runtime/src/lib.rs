//! RBE `.service` compiler/runtime primitives.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod manager;
pub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};

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

pub trait ServiceExecutor: Send + Sync {
    fn call<'a>(&'a self, function: &'a str, args: Vec<Value>) -> ServiceExecutionFuture<'a>;
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
    let ready = ServiceReady {
        service: file.name.clone(),
        pid: std::process::id(),
        address: listener.local_addr()?,
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let request: ServiceRequest = serde_json::from_str(line.trim())?;
        let (response, shutdown) =
            dispatch(request, &file, &token, &memory, executor.as_ref()).await;
        write
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
        write.shutdown().await?;
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
        ServiceRequest::Call { function, args, .. } => match executor.call(&function, args).await {
            Ok(value) => ok(value),
            Err(error) => (
                ServiceResponse::Error {
                    code: error.code,
                    message: error.message,
                },
                false,
            ),
        },
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
}
