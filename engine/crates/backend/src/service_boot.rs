use std::path::{Path, PathBuf};
use std::sync::Arc;

use service_runtime::{ServiceCatalog, ServiceDefaults, ServiceMemory};

pub async fn run_host(args: &[String]) -> anyhow::Result<()> {
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let service_file = value("--service-file")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("backend --service-host requires --service-file <path>"))?;
    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("service host")? {
        Some(token) => token,
        None => value("--service-token")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("backend --service-host requires parent authentication")
            })?,
    };

    // Service children load the same typed settings as the mother process so
    // configurable defaults stay consistent even when the .service file omits
    // memoryLimitMb/startupTimeoutMs.
    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".into());
    let config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("service host failed to load {settings_path}: {error}"))?;
    let defaults = ServiceDefaults {
        memory_limit_mb: config.services.default_memory_limit_mb,
        startup_timeout_ms: config.services.startup_timeout_ms,
        default_idle_timeout_ms: config.services.default_idle_timeout_ms,
        monitor_interval_ms: config.services.monitor_interval_ms,
        max_restart_backoff_ms: config.services.max_restart_backoff_ms,
    };
    let source = std::fs::read_to_string(&service_file).map_err(|error| {
        anyhow::anyhow!(
            "service host failed to read executable body {}: {error}",
            service_file.display()
        )
    })?;
    let program = route_engine::parse_service_source(&source).map_err(|error| {
        anyhow::anyhow!(
            "service host failed to parse {}:{}:{}: {}",
            service_file.display(),
            error.line,
            error.column,
            error.message
        )
    })?;
    let modules = route_engine::ModuleProgram::load_default().map_err(|errors| {
        anyhow::anyhow!(
            "service host module compilation failed:
{}",
            errors.render()
        )
    })?;
    let memory = ServiceMemory::default();
    let executor = route_engine::ServiceProgramExecutor::new(program, modules, memory.clone());
    service_runtime::run_service_host_with_executor_and_memory(
        service_file,
        token,
        defaults,
        memory,
        Arc::new(executor),
    )
    .await
}

fn validate_executable_catalog(catalog: &ServiceCatalog) -> Result<(), String> {
    let mut diagnostics = Vec::new();
    for service in catalog.services() {
        let source = match std::fs::read_to_string(&service.path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(format!(
                    "SVC2001 {}:1:1 failed to read executable service body: {error}",
                    service.path.display()
                ));
                continue;
            }
        };
        if let Err(error) = route_engine::parse_service_source(&source) {
            diagnostics.push(format!(
                "SVC2000 {}:{}:{} {}",
                service.path.display(),
                error.line,
                error.column,
                error.message
            ));
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics.join("\n"))
    }
}

fn report_compile_failure(rendered: &str, io: &atomic_io::AtomicIo) {
    let error_path = compiler_error_path();
    if let Err(write_error) = io.write_atomic(&error_path, rendered.as_bytes()) {
        tracing::error!(
            error = %write_error,
            path = %error_path.display(),
            "failed to persist service compiler diagnostics"
        );
    }
    eprintln!("backend couldn't start because a .service file failed to compile:\n{rendered}");
    eprintln!("compiler log: {}", error_path.display());
    service_runtime::pause_for_interactive_exit();
}

pub fn compile(
    settings: &config::ServicesConfig,
    io: &atomic_io::AtomicIo,
) -> anyhow::Result<Option<ServiceCatalog>> {
    if !settings.enabled {
        tracing::info!("user .service runtime disabled by configuration");
        return Ok(None);
    }

    let directory = resolve_runtime_path(&settings.directory);
    let defaults = ServiceDefaults {
        memory_limit_mb: settings.default_memory_limit_mb,
        startup_timeout_ms: settings.startup_timeout_ms,
        default_idle_timeout_ms: settings.default_idle_timeout_ms,
        monitor_interval_ms: settings.monitor_interval_ms,
        max_restart_backoff_ms: settings.max_restart_backoff_ms,
    };
    match ServiceCatalog::compile_dir(&directory, defaults) {
        Ok(catalog) => {
            if let Err(rendered) = validate_executable_catalog(&catalog) {
                report_compile_failure(&rendered, io);
                return Err(anyhow::anyhow!(".service executable compilation failed"));
            }
            let error_path = compiler_error_path();
            if error_path.exists() {
                let _ = io.write_atomic(&error_path, b"");
            }
            tracing::info!(
                directory = %directory.display(),
                services = catalog.services().len(),
                "compiled .service catalog"
            );
            Ok(Some(catalog))
        }
        Err(errors) => {
            let rendered = errors.render();
            report_compile_failure(&rendered, io);
            Err(anyhow::anyhow!(".service compilation failed"))
        }
    }
}

pub fn resolve_runtime_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        runtime_paths::binary_dir().join(path)
    }
}

fn compiler_error_path() -> PathBuf {
    runtime_paths::default_admin_dir().join("service-compiler-error.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-service-boot-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn executable_validation_reports_file_and_location() {
        let dir = temp_dir();
        let path = dir.join("broken.service");
        std::fs::write(
            &path,
            ":service[name = broken]\nexport function run(value) { return value }",
        )
        .unwrap();
        let catalog = ServiceCatalog::compile_dir(&dir, ServiceDefaults::default()).unwrap();
        let rendered = validate_executable_catalog(&catalog).unwrap_err();
        assert!(rendered.contains("SVC2000"));
        assert!(rendered.contains("broken.service"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
