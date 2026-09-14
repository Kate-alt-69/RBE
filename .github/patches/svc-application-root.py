from pathlib import Path

def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one replacement, found {count}")
    p.write_text(text.replace(old, new, 1))

# backend service_boot.rs
path = "engine/crates/backend/src/service_boot.rs"
replace_once(path, """    let service_file = value("--service-file")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("service --service-host requires --service-file <path>"))?;
    let token = service_runtime::read_parent_bootstrap_secret_if_configured("service host")?
""", """    let service_file = value("--service-file")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("service --service-host requires --service-file <path>"))?;
    let mother_managed = args.iter().any(|arg| arg == "--service-mother-address");
    let application_root = value("--application-root").map(PathBuf::from);
    if mother_managed && application_root.is_none() {
        anyhow::bail!("Service Mother-managed worker requires --application-root");
    }
    let application_root = application_root
        .map(|path| canonical_runtime_root(&path, "service host application root"))
        .transpose()?;
    let token = service_runtime::read_parent_bootstrap_secret_if_configured("service host")?
""")
replace_once(path, """    let modules = route_engine::ModuleProgram::load_default().map_err(|errors| {
        anyhow::anyhow!(
            "service host module compilation failed:
{}",
            errors.render()
        )
    })?;
""", """    let modules = match application_root.as_ref() {
        Some(root) => route_engine::ModuleProgram::load(&root.join("module")),
        None => route_engine::ModuleProgram::load_default(),
    }
    .map_err(|errors| {
        anyhow::anyhow!(
            "service host module compilation failed:
{}",
            errors.render()
        )
    })?;
""")
replace_once(path, """fn validate_executable_catalog(catalog: &ServiceCatalog) -> Result<(), String> {
""", """fn canonical_runtime_root(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("canonicalize {label} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("{label} {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn validate_executable_catalog(catalog: &ServiceCatalog) -> Result<(), String> {
""")

old_compile = """pub fn compile(
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
"""
new_compile = """pub fn compile(
    settings: &config::ServicesConfig,
    io: &atomic_io::AtomicIo,
) -> anyhow::Result<Option<ServiceCatalog>> {
    let directory = resolve_runtime_path(&settings.directory);
    compile_resolved(settings, io, &directory)
}

pub fn compile_from_root(
    settings: &config::ServicesConfig,
    io: &atomic_io::AtomicIo,
    application_root: &Path,
) -> anyhow::Result<Option<ServiceCatalog>> {
    let directory = resolve_runtime_path_from(application_root, &settings.directory);
    compile_resolved(settings, io, &directory)
}

fn compile_resolved(
    settings: &config::ServicesConfig,
    io: &atomic_io::AtomicIo,
    directory: &Path,
) -> anyhow::Result<Option<ServiceCatalog>> {
    if !settings.enabled {
        tracing::info!("user .service runtime disabled by configuration");
        return Ok(None);
    }

    let defaults = ServiceDefaults {
        memory_limit_mb: settings.default_memory_limit_mb,
        startup_timeout_ms: settings.startup_timeout_ms,
        default_idle_timeout_ms: settings.default_idle_timeout_ms,
        monitor_interval_ms: settings.monitor_interval_ms,
        max_restart_backoff_ms: settings.max_restart_backoff_ms,
    };
    match ServiceCatalog::compile_dir(directory, defaults) {
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
    resolve_runtime_path_from(&runtime_paths::binary_dir(), path)
}

pub fn resolve_runtime_path_from(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}
"""
replace_once(path, old_compile, new_compile)
replace_once(path, """    #[test]
    fn executable_validation_reports_file_and_location() {
        let dir = temp_dir();
        let path = dir.join("broken.service");
        std::fs::write(
            &path,
            ":service[name = broken]\\nexport function run(value) { return value }",
        )
        .unwrap();
        let catalog = ServiceCatalog::compile_dir(&dir, ServiceDefaults::default()).unwrap();
        let rendered = validate_executable_catalog(&catalog).unwrap_err();
        assert!(rendered.contains("SVC2000"));
        assert!(rendered.contains("broken.service"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
""", """    #[test]
    fn executable_validation_reports_file_and_location() {
        let dir = temp_dir();
        let path = dir.join("broken.service");
        std::fs::write(
            &path,
            ":service[name = broken]\\nexport function run(value) { return value }",
        )
        .unwrap();
        let catalog = ServiceCatalog::compile_dir(&dir, ServiceDefaults::default()).unwrap();
        let rendered = validate_executable_catalog(&catalog).unwrap_err();
        assert!(rendered.contains("SVC2000"));
        assert!(rendered.contains("broken.service"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_application_root_owns_relative_service_paths() {
        let root = temp_dir();
        let expected = root.join("service");
        assert_eq!(
            resolve_runtime_path_from(&root, "service"),
            expected,
            "helper executable location must not redefine the application service root"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
""")

# backend service_mother.rs
path = "engine/crates/backend/src/service_mother.rs"
replace_once(path, """    let settings_path = flag_value(args, "--settings").unwrap_or_else(|| "settings.json".into());
    let config = config::Config::load(&settings_path).map_err(|error| {
        anyhow::anyhow!("Service Mother failed to load {settings_path}: {error}")
    })?;
    let runtime_env = Arc::new(match runtime_env_frame {
""", """    let settings_path = flag_value(args, "--settings").unwrap_or_else(|| "settings.json".into());
    let config = config::Config::load(&settings_path).map_err(|error| {
        anyhow::anyhow!("Service Mother failed to load {settings_path}: {error}")
    })?;
    let parent_supervised = std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_some();
    let application_root = flag_value(args, "--application-root").map(PathBuf::from);
    if parent_supervised && application_root.is_none() {
        anyhow::bail!("Service Mother requires the parent application root");
    }
    let application_root = application_root
        .map(|path| {
            let canonical = std::fs::canonicalize(&path).with_context(|| {
                format!(
                    "canonicalize Service Mother application root {}",
                    path.display()
                )
            })?;
            if !canonical.is_dir() {
                anyhow::bail!(
                    "Service Mother application root {} is not a directory",
                    canonical.display()
                );
            }
            Ok::<PathBuf, anyhow::Error>(canonical)
        })
        .transpose()?;
    let runtime_env = Arc::new(match runtime_env_frame {
""")
replace_once(path, """    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile(&config.services, &io)?;
""", """    let io = atomic_io::AtomicIo::new();
    let catalog = match application_root.as_deref() {
        Some(root) => crate::service_boot::compile_from_root(&config.services, &io, root)?,
        None => crate::service_boot::compile(&config.services, &io)?,
    };
""")
replace_once(path, """            ServiceManager::prepare_all_with_fabric_runtime_env_and_restart_authority(
                catalog,
                server.fabric_endpoint(),
                runtime_env.clone(),
                restart_authority,
            )
            .await
""", """            match application_root.clone() {
                Some(application_root) => {
                    ServiceManager::prepare_all_for_mother(
                        catalog,
                        server.fabric_endpoint(),
                        runtime_env.clone(),
                        restart_authority,
                        application_root,
                    )
                    .await
                }
                None => {
                    ServiceManager::prepare_all_with_fabric_runtime_env_and_restart_authority(
                        catalog,
                        server.fabric_endpoint(),
                        runtime_env.clone(),
                        restart_authority,
                    )
                    .await
                }
            }
""")
replace_once(path, """        .arg("--settings")
        .arg(&settings_path)
        .arg("--runtime-env-frame")
""", """        .arg("--settings")
        .arg(&settings_path)
        .arg("--application-root")
        .arg(parent)
        .arg("--runtime-env-frame")
""")
replace_once(path, """        settings = %settings_path.display(),
        "Service Mother process ready"
""", """        settings = %settings_path.display(),
        application_root = %parent.display(),
        "Service Mother process ready"
""")

# service-runtime manager.rs
path = "engine/crates/service-runtime/src/manager.rs"
replace_once(path, """#[cfg(test)]
use std::path::PathBuf;
""", """use std::path::{Path, PathBuf};
""")
replace_once(path, """    runtime_env: Option<Arc<Value>>,
    restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,
}
""", """    runtime_env: Option<Arc<Value>>,
    restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,
    application_root: Option<Arc<PathBuf>>,
}
""")
replace_once(path, """    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None, None, None).await;
""", """    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None, None, None, None).await;
""")
replace_once(path, """        Self::prepare_all(catalog, Some(fabric), None, None).await
""", """        Self::prepare_all(catalog, Some(fabric), None, None, None).await
""")
replace_once(path, """        Self::prepare_all(catalog, Some(fabric), Some(runtime_env), None).await
""", """        Self::prepare_all(catalog, Some(fabric), Some(runtime_env), None, None).await
""")
replace_once(path, """        Self::prepare_all(catalog, Some(fabric), Some(runtime_env), restart_authority).await
    }

    async fn prepare_all(
        catalog: &ServiceCatalog,
        fabric: Option<ServiceFabricEndpoint>,
        runtime_env: Option<Arc<Value>>,
        restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,
    ) -> Self {
        let manager = Self {
            fabric,
            runtime_env,
            restart_authority,
            ..Self::default()
        };
""", """        Self::prepare_all(
            catalog,
            Some(fabric),
            Some(runtime_env),
            restart_authority,
            None,
        )
        .await
    }

    pub async fn prepare_all_for_mother(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
        runtime_env: Arc<Value>,
        restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,
        application_root: PathBuf,
    ) -> Self {
        Self::prepare_all(
            catalog,
            Some(fabric),
            Some(runtime_env),
            restart_authority,
            Some(Arc::new(application_root)),
        )
        .await
    }

    async fn prepare_all(
        catalog: &ServiceCatalog,
        fabric: Option<ServiceFabricEndpoint>,
        runtime_env: Option<Arc<Value>>,
        restart_authority: Option<Arc<dyn ServiceRestartAuthority>>,
        application_root: Option<Arc<PathBuf>>,
    ) -> Self {
        let manager = Self {
            fabric,
            runtime_env,
            restart_authority,
            application_root,
            ..Self::default()
        };
""")
replace_once(path, """                match spawn_process(&file, self.fabric.as_ref(), self.runtime_env.as_deref()).await
""", """                match spawn_process(
                    &file,
                    self.fabric.as_ref(),
                    self.runtime_env.as_deref(),
                    self.application_root.as_deref().map(PathBuf::as_path),
                )
                .await
""")
replace_once(path, """            match spawn_process(&file, self.fabric.as_ref(), self.runtime_env.as_deref()).await {
""", """            match spawn_process(
                &file,
                self.fabric.as_ref(),
                self.runtime_env.as_deref(),
                self.application_root.as_deref().map(PathBuf::as_path),
            )
            .await
            {
""")
replace_once(path, """        match spawn_process(&file, self.fabric.as_ref(), self.runtime_env.as_deref()).await {
""", """        match spawn_process(
            &file,
            self.fabric.as_ref(),
            self.runtime_env.as_deref(),
            self.application_root.as_deref().map(PathBuf::as_path),
        )
        .await
        {
""")
replace_once(path, """async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
    runtime_env: Option<&Value>,
) -> anyhow::Result<ServiceProcess> {
""", """async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
    runtime_env: Option<&Value>,
    application_root: Option<&Path>,
) -> anyhow::Result<ServiceProcess> {
""")
replace_once(path, """        .arg("--service-source-digest")
        .arg(file.source_digest_hex());
    if let Some(fabric) = fabric {
""", """        .arg("--service-source-digest")
        .arg(file.source_digest_hex());
    if let Some(application_root) = application_root {
        command.arg("--application-root").arg(application_root);
    }
    if let Some(fabric) = fabric {
""")
print("BUG-SVC-001 patch applied")
