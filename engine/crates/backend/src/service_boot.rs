use std::path::{Path, PathBuf};

use service_runtime::{ServiceCatalog, ServiceDefaults, ServiceManager};

pub async fn run_host(args: &[String]) -> anyhow::Result<()> {
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let service_file = value("--service-file")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("backend --service-host requires --service-file <path>"))?;
    let token = value("--service-token")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("backend --service-host requires --service-token <token>"))?;

    // Service children load the same typed settings as the mother process so
    // configurable defaults stay consistent even when the .service file omits
    // memoryLimitMb/startupTimeoutMs.
    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".into());
    let config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("service host failed to load {settings_path}: {error}"))?;
    let defaults = ServiceDefaults {
        memory_limit_mb: config.services.default_memory_limit_mb,
        startup_timeout_ms: config.services.startup_timeout_ms,
    };
    service_runtime::run_service_host(service_file, token, defaults).await
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
    };
    match ServiceCatalog::compile_dir(&directory, defaults) {
        Ok(catalog) => {
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
            Err(anyhow::anyhow!(".service compilation failed"))
        }
    }
}

pub async fn start(catalog: Option<&ServiceCatalog>) -> anyhow::Result<ServiceManager> {
    let Some(catalog) = catalog else {
        return Ok(ServiceManager::default());
    };
    let manager = ServiceManager::spawn_all(catalog).await?;
    let snapshots = manager.snapshot().await;
    tracing::info!(
        services = snapshots.len(),
        running = snapshots.iter().filter(|service| service.pid.is_some()).count(),
        "user .service processes ready"
    );
    Ok(manager)
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
