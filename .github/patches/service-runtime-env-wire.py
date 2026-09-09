from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:180]!r}")
    write(path, text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# Generic bounded bootstrap JSON frame. The inherited stdin channel already
# carries authentication and remains open for liveness; Runtime ENV uses that
# channel rather than argv or OS environment variables.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/lib.rs"
text = read(path)
const_anchor = '''pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
'''
if const_anchor not in text:
    raise SystemExit("missing service-runtime constants anchor")
text = text.replace(
    const_anchor,
    const_anchor + "const PARENT_BOOTSTRAP_JSON_MAX_BYTES: usize = 1024 * 1024;\n",
    1,
)
secret_anchor = '''/// Send the one-time child authentication value without exposing it in the
/// process command line or environment. The caller must retain `writer` after
/// this returns so EOF continues to mean parent death to the child.
pub async fn write_parent_bootstrap_secret<W>(writer: &mut W, secret: &str) -> anyhow::Result<()>
'''
if secret_anchor not in text:
    raise SystemExit("missing bootstrap secret writer anchor")
json_helpers = r'''fn read_parent_bootstrap_json<R: BufRead>(reader: &mut R, label: &str) -> anyhow::Result<Value> {
    let limit = u64::try_from(PARENT_BOOTSTRAP_JSON_MAX_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut limited = reader.take(limit);
    let mut line = String::new();
    let bytes = limited.read_line(&mut line)?;
    if bytes == 0 {
        anyhow::bail!("{label} parent bootstrap pipe closed before JSON frame");
    }
    if bytes > PARENT_BOOTSTRAP_JSON_MAX_BYTES {
        anyhow::bail!(
            "{label} parent bootstrap JSON exceeded {PARENT_BOOTSTRAP_JSON_MAX_BYTES} bytes"
        );
    }
    if !line.ends_with('\n') {
        anyhow::bail!("{label} parent bootstrap JSON is not newline terminated");
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    serde_json::from_str(&line)
        .map_err(|error| anyhow::anyhow!("{label} parent bootstrap JSON is invalid: {error}"))
}

pub fn read_parent_bootstrap_json_if_configured(
    label: &str,
) -> anyhow::Result<Option<Value>> {
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_none() {
        return Ok(None);
    }
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    read_parent_bootstrap_json(&mut stdin, label).map(Some)
}

pub async fn write_parent_bootstrap_json<W>(
    writer: &mut W,
    value: &Value,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len().saturating_add(1) > PARENT_BOOTSTRAP_JSON_MAX_BYTES {
        anyhow::bail!(
            "parent bootstrap JSON exceeded {PARENT_BOOTSTRAP_JSON_MAX_BYTES} bytes"
        );
    }
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

'''
text = text.replace(secret_anchor, json_helpers + secret_anchor, 1)

test_anchor = '''    #[test]
    fn parent_bootstrap_secret_reader_is_bounded_and_validates_token() {
'''
if test_anchor not in text:
    raise SystemExit("missing bootstrap tests anchor")
json_test = r'''    #[tokio::test]
    async fn parent_bootstrap_json_is_bounded_and_round_trips_types() {
        let value = serde_json::json!({
            "APP_NAME": "RBE",
            "COUNT": 7,
            "FLAGS": [true, false]
        });
        let mut output = Vec::new();
        write_parent_bootstrap_json(&mut output, &value).await.unwrap();
        let mut cursor = std::io::Cursor::new(output);
        let decoded = read_parent_bootstrap_json(&mut cursor, "test").unwrap();
        assert_eq!(decoded, value);

        let mut unterminated = std::io::Cursor::new(b"{\"A\":1}".to_vec());
        assert!(read_parent_bootstrap_json(&mut unterminated, "test").is_err());
    }

'''
text = text.replace(test_anchor, json_test + test_anchor, 1)
write(path, text)


# ---------------------------------------------------------------------------
# RuntimeEnv can reconstruct an immutable typed view from a parent Runtime Image
# snapshot without consulting server.server or settings.json again.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/runtime_env.rs"
text = read(path)
text = text.replace(
    '''    ForcedServer,
}''',
    '''    ForcedServer,
    ImageSnapshot,
}''',
    1,
)
anchor = '''    pub fn empty() -> Self {
        Self {
            values: Arc::new(BTreeMap::new()),
            origins: Arc::new(BTreeMap::new()),
        }
    }

'''
if anchor not in text:
    raise SystemExit("missing RuntimeEnv empty anchor")
from_snapshot = r'''    pub fn from_snapshot(snapshot: JsonValue) -> Result<Self, RuntimeEnvError> {
        let JsonValue::Object(values) = snapshot else {
            return Err(RuntimeEnvError::InvalidServerEntry(
                "Runtime ENV image snapshot must be a JSON object".into(),
            ));
        };
        let values = values.into_iter().collect::<BTreeMap<_, _>>();
        let origins = values
            .keys()
            .map(|name| (name.clone(), RuntimeEnvOrigin::ImageSnapshot))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            values: Arc::new(values),
            origins: Arc::new(origins),
        })
    }

'''
text = text.replace(anchor, anchor + from_snapshot, 1)
test_anchor = '''    #[test]
    fn routes_do_not_receive_runtime_env_by_default() {
'''
if test_anchor not in text:
    raise SystemExit("missing RuntimeEnv tests anchor")
snapshot_test = r'''    #[test]
    fn image_snapshot_round_trip_keeps_runtime_env_types() {
        let env = RuntimeEnv::from_snapshot(serde_json::json!({
            "NAME": "rbe",
            "COUNT": 7,
            "FLAGS": [true, false]
        }))
        .unwrap();
        assert_eq!(env.string("NAME").unwrap(), "rbe");
        assert_eq!(env.number("COUNT").unwrap(), 7.0);
        assert_eq!(env.array("FLAGS").unwrap().len(), 2);
        assert_eq!(env.origin("NAME"), Some(RuntimeEnvOrigin::ImageSnapshot));
    }

'''
text = text.replace(test_anchor, snapshot_test + test_anchor, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Service VM exposes the typed Runtime ENV snapshot through the same host
# capability bridge already used for process-local memory/quickDB.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/service_eval.rs"
text = read(path)
if "use crate::runtime_env::RuntimeEnv;" not in text:
    text = text.replace(
        "use crate::module_runtime::ModuleProgram;",
        "use crate::module_runtime::ModuleProgram;\nuse crate::runtime_env::RuntimeEnv;",
        1,
    )
text = text.replace(
    '''    quick_db_init_errors: Vec<(String, String)>,
}''',
    '''    quick_db_init_errors: Vec<(String, String)>,
    runtime_env: Option<Arc<RuntimeEnv>>,
}''',
    1,
)
text = text.replace(
    '''    fn new(memory: ServiceMemory, classes: &HashMap<String, ServiceClassDef>) -> Self {''',
    '''    fn new(
        memory: ServiceMemory,
        classes: &HashMap<String, ServiceClassDef>,
        runtime_env: Option<Arc<RuntimeEnv>>,
    ) -> Self {''',
    1,
)
text = text.replace(
    '''            quick_db_classes,
            quick_db_init_errors,
        }''',
    '''            quick_db_classes,
            quick_db_init_errors,
            runtime_env,
        }''',
    1,
)
host_match = '''            let value = match module {
                "memory" => self.call_memory(function, &args)?,
                "quickDB" => self.call_quick_db(scope.as_deref(), function, &args)?,
                _ => return Ok(None),
            };'''
if host_match not in text:
    raise SystemExit("missing ServiceHostCapabilities match anchor")
host_new = '''            let value = match module {
                "memory" => self.call_memory(function, &args)?,
                "quickDB" => self.call_quick_db(scope.as_deref(), function, &args)?,
                "ENV" => {
                    let env = self.runtime_env.as_ref().ok_or_else(|| {
                        eval_error(
                            "ENV3001",
                            "Runtime ENV snapshot is unavailable in this Service REL executor",
                        )
                    })?;
                    env.call_rel(function, &args)
                        .map_err(|error| eval_error("ENV3000", error.to_string()))?
                }
                _ => return Ok(None),
            };'''
text = text.replace(host_match, host_new, 1)

text = text.replace(
    '''    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        Self::build(program, modules, memory, None)
    }''',
    '''    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        Self::build(program, modules, memory, None, None)
    }''',
    1,
)
text = text.replace(
    '''    ) -> Self {
        Self::build(program, modules, memory, Some(services))
    }

    fn build(
        program: ServiceProgram,
        modules: ModuleProgram,
        memory: ServiceMemory,
        services: Option<ServiceManager>,
    ) -> Self {''',
    '''    ) -> Self {
        Self::build(program, modules, memory, Some(services), None)
    }

    pub fn with_services_and_runtime_env(
        program: ServiceProgram,
        modules: ModuleProgram,
        memory: ServiceMemory,
        services: ServiceManager,
        runtime_env: Arc<RuntimeEnv>,
    ) -> Self {
        Self::build(
            program,
            modules,
            memory,
            Some(services),
            Some(runtime_env),
        )
    }

    fn build(
        program: ServiceProgram,
        modules: ModuleProgram,
        memory: ServiceMemory,
        services: Option<ServiceManager>,
        runtime_env: Option<Arc<RuntimeEnv>>,
    ) -> Self {''',
    1,
)
text = text.replace(
    '''        let host_capabilities = Arc::new(ServiceHostCapabilities::new(memory, &classes));''',
    '''        let host_capabilities = Arc::new(ServiceHostCapabilities::new(
            memory,
            &classes,
            runtime_env,
        ));''',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# ServiceManager stores the exact Runtime ENV JSON selected by Mother and hands
# it unchanged to resident, on-demand and restarted service children.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace(
    '''    fabric: Option<ServiceFabricEndpoint>,
}''',
    '''    fabric: Option<ServiceFabricEndpoint>,
    runtime_env: Option<Arc<Value>>,
}''',
    1,
)
text = text.replace(
    '''    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None).await;''',
    '''    pub async fn spawn_all(catalog: &ServiceCatalog) -> anyhow::Result<Self> {
        let manager = Self::prepare_all(catalog, None, None).await;''',
    1,
)
text = text.replace(
    '''    pub async fn prepare_all_with_fabric(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
    ) -> Self {
        Self::prepare_all(catalog, Some(fabric)).await
    }

    async fn prepare_all(catalog: &ServiceCatalog, fabric: Option<ServiceFabricEndpoint>) -> Self {
        let manager = Self {
            fabric,
            ..Self::default()
        };''',
    '''    pub async fn prepare_all_with_fabric(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
    ) -> Self {
        Self::prepare_all(catalog, Some(fabric), None).await
    }

    pub async fn prepare_all_with_fabric_and_runtime_env(
        catalog: &ServiceCatalog,
        fabric: ServiceFabricEndpoint,
        runtime_env: Arc<Value>,
    ) -> Self {
        Self::prepare_all(catalog, Some(fabric), Some(runtime_env)).await
    }

    async fn prepare_all(
        catalog: &ServiceCatalog,
        fabric: Option<ServiceFabricEndpoint>,
        runtime_env: Option<Arc<Value>>,
    ) -> Self {
        let manager = Self {
            fabric,
            runtime_env,
            ..Self::default()
        };''',
    1,
)
old_call = "spawn_process(&file, self.fabric.as_ref()).await"
if old_call not in text:
    raise SystemExit("missing ServiceManager spawn_process calls")
text = text.replace(
    old_call,
    "spawn_process(&file, self.fabric.as_ref(), self.runtime_env.as_deref()).await",
)
text = text.replace(
    '''async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
) -> anyhow::Result<ServiceProcess> {''',
    '''async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
    runtime_env: Option<&Value>,
) -> anyhow::Result<ServiceProcess> {''',
    1,
)
command_marker = '''    if let Some(fabric) = fabric {
        command
            .arg("--service-mother-address")
            .arg(fabric.address().to_string());
    }
'''
if command_marker not in text:
    raise SystemExit("missing service child Fabric command anchor")
text = text.replace(
    command_marker,
    command_marker + '''    if runtime_env.is_some() {
        command.arg("--runtime-env-frame");
    }
''',
    1,
)
write_marker = '''    if let Some(fabric) = fabric {
        if let Err(error) = super::write_parent_bootstrap_secret(&mut liveness, fabric.auth()).await
        {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(anyhow::anyhow!(
                "send service {:?} Service Fabric bootstrap secret: {error}",
                file.name
            ));
        }
    }
'''
if write_marker not in text:
    raise SystemExit("missing Service Fabric bootstrap write anchor")
runtime_write = write_marker + '''    if let Some(runtime_env) = runtime_env {
        if let Err(error) = super::write_parent_bootstrap_json(&mut liveness, runtime_env).await {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(anyhow::anyhow!(
                "send service {:?} Runtime ENV snapshot: {error}",
                file.name
            ));
        }
    }
'''
text = text.replace(write_marker, runtime_write, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Mother receives one Runtime ENV snapshot from backend and uses that exact Arc
# for all child lifetimes, including Mother/process restarts.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
run_token_anchor = '''    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("Service Mother")?
    {
        Some(token) => token,
        None => flag_value(args, "--service-token")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("backend --service-mother requires parent authentication")
            })?,
    };
'''
if run_token_anchor not in text:
    raise SystemExit("missing Service Mother token anchor")
runtime_frame_read = run_token_anchor + '''    let runtime_env_frame = if args.iter().any(|arg| arg == "--runtime-env-frame") {
        service_runtime::read_parent_bootstrap_json_if_configured("Service Mother Runtime ENV")?
            .ok_or_else(|| {
                anyhow::anyhow!("Service Mother Runtime ENV frame requires inherited bootstrap")
            })?
            .into()
    } else {
        None
    };
'''
text = text.replace(run_token_anchor, runtime_frame_read, 1)
config_anchor = '''    let config = config::Config::load(&settings_path).map_err(|error| {
        anyhow::anyhow!("Service Mother failed to load {settings_path}: {error}")
    })?;
'''
if config_anchor not in text:
    raise SystemExit("missing Service Mother config anchor")
config_new = config_anchor + '''    let runtime_env = Arc::new(match runtime_env_frame {
        Some(value) => value,
        None => serde_json::to_value(&config.runtime_env)?,
    });
'''
text = text.replace(config_anchor, config_new, 1)
prepare_anchor = '''            ServiceManager::prepare_all_with_fabric(catalog, server.fabric_endpoint()).await
'''
if prepare_anchor not in text:
    raise SystemExit("missing Service Mother prepare manager anchor")
text = text.replace(
    prepare_anchor,
    '''            ServiceManager::prepare_all_with_fabric_and_runtime_env(
                catalog,
                server.fabric_endpoint(),
                runtime_env.clone(),
            )
            .await
''',
    1,
)

# spawn_process gets snapshot argument.
text = text.replace(
    '''async fn spawn_process(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    existing_manager: Option<&ServiceManager>,
) -> anyhow::Result<ServiceMotherProcess> {''',
    '''async fn spawn_process(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    runtime_env: &serde_json::Value,
    existing_manager: Option<&ServiceManager>,
) -> anyhow::Result<ServiceMotherProcess> {''',
    1,
)
arg_anchor = '''        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .current_dir(parent)'''
if arg_anchor not in text:
    raise SystemExit("missing Service Mother command args anchor")
text = text.replace(
    arg_anchor,
    '''        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .arg("--runtime-env-frame")
        .current_dir(parent)''',
    1,
)
write_secret_anchor = '''    if let Err(error) = service_runtime::write_parent_bootstrap_secret(&mut liveness, &token).await
    {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send Service Mother parent bootstrap secret: {error}"
        ));
    }
'''
if write_secret_anchor not in text:
    raise SystemExit("missing Service Mother secret write anchor")
text = text.replace(
    write_secret_anchor,
    write_secret_anchor + '''    if let Err(error) =
        service_runtime::write_parent_bootstrap_json(&mut liveness, runtime_env).await
    {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send Service Mother Runtime ENV snapshot: {error}"
        ));
    }
''',
    1,
)
text = text.replace(
    '''pub async fn spawn(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
) -> anyhow::Result<ServiceMotherSupervisor> {''',
    '''pub async fn spawn(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    runtime_env: Arc<serde_json::Value>,
) -> anyhow::Result<ServiceMotherSupervisor> {''',
    1,
)
text = text.replace(
    '''    let initial = spawn_process(&settings_path, &expected_catalog_fingerprint, None).await?;''',
    '''    let initial = spawn_process(
        &settings_path,
        &expected_catalog_fingerprint,
        runtime_env.as_ref(),
        None,
    )
    .await?;''',
    1,
)
text = text.replace(
    '''            supervisor_fingerprint,
            supervisor_manager,
            &mut shutdown_rx,''',
    '''            supervisor_fingerprint,
            runtime_env,
            supervisor_manager,
            &mut shutdown_rx,''',
    1,
)
text = text.replace(
    '''async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    expected_catalog_fingerprint: String,
    manager: ServiceManager,''',
    '''async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    expected_catalog_fingerprint: String,
    runtime_env: Arc<serde_json::Value>,
    manager: ServiceManager,''',
    1,
)
text = text.replace(
    '''                &settings_path,
                &expected_catalog_fingerprint,
                Some(&manager),''',
    '''                &settings_path,
                &expected_catalog_fingerprint,
                runtime_env.as_ref(),
                Some(&manager),''',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# Service host consumes the Runtime ENV frame after authentication/Fabric auth,
# reconstructs RuntimeEnv and injects it into ServiceProgramExecutor.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
manager_anchor = '''    let service_manager = match value("--service-mother-address") {
'''
# Insert after the complete service_manager match by using the following config comment.
config_comment = '''    // Service children load the same typed settings as the mother process so
'''
idx = text.find(config_comment)
if idx < 0:
    raise SystemExit("missing service host config comment")
runtime_read = '''    let runtime_env_frame = if args.iter().any(|arg| arg == "--runtime-env-frame") {
        Some(
            service_runtime::read_parent_bootstrap_json_if_configured("service Runtime ENV")?
                .ok_or_else(|| {
                    anyhow::anyhow!("service Runtime ENV frame requires inherited bootstrap")
                })?,
        )
    } else {
        None
    };

'''
text = text[:idx] + runtime_read + text[idx:]
config_load = '''    let config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("service host failed to load {settings_path}: {error}"))?;
'''
if config_load not in text:
    raise SystemExit("missing service host config load")
text = text.replace(
    config_load,
    config_load + '''    let runtime_env = Arc::new(route_engine::RuntimeEnv::from_snapshot(
        match runtime_env_frame {
            Some(value) => value,
            None => serde_json::to_value(&config.runtime_env)?,
        },
    )?);
''',
    1,
)
executor_anchor = '''    let executor = route_engine::ServiceProgramExecutor::with_services(
        program,
        modules,
        memory.clone(),
        service_manager,
    );'''
if executor_anchor not in text:
    raise SystemExit("missing ServiceProgramExecutor host anchor")
text = text.replace(
    executor_anchor,
    '''    let executor = route_engine::ServiceProgramExecutor::with_services_and_runtime_env(
        program,
        modules,
        memory.clone(),
        service_manager,
        runtime_env,
    );''',
    1,
)
write(path, text)


# Backend sends the already-resolved active image ENV to Service Mother.
path = "engine/crates/backend/src/main.rs"
text = read(path)
spawn_anchor = '''    let service_mother = match service_catalog.as_ref() {
        Some(catalog) => Some(service_mother::spawn(&settings_path, &catalog.fingerprint()).await?),
        None => None,
    };'''
if spawn_anchor not in text:
    raise SystemExit("missing backend Service Mother spawn anchor")
spawn_new = '''    let service_runtime_env = Arc::new(runtime_image.snapshot().environment.to_json());
    let service_mother = match service_catalog.as_ref() {
        Some(catalog) => Some(
            service_mother::spawn(
                &settings_path,
                &catalog.fingerprint(),
                service_runtime_env.clone(),
            )
            .await?,
        ),
        None => None,
    };'''
text = text.replace(spawn_anchor, spawn_new, 1)
write(path, text)


# Docs mark Service Runtime ENV snapshot propagation as implemented behavior.
path = "doc/rel.md"
text = read(path)
security_marker = '''A future sealed production bundle may omit raw REL sources entirely after RBE
'''
if security_marker in text and "Service Mother propagates that same snapshot" not in text:
    insert = '''Supervised Service REL does not reconstruct Runtime ENV from child process state.
Backend sends the active image's typed ENV snapshot over the authenticated
parent-liveness bootstrap channel; Service Mother propagates that same snapshot
to resident, on-demand, and restarted service children.

'''
    text = text.replace(security_marker, insert + security_marker, 1)
write(path, text)
