from pathlib import Path
import re

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
# Runtime Image owns executable AST snapshots. Manifests are useful metadata,
# but runtime execution must not reopen source files after RELC validation.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/runtime_image.rs"
text = read(path)
text = text.replace(
    "use crate::dependency_graph::{SymbolDependencyGraph, SymbolId};",
    "use crate::ast::{ModuleFile, RouteFile, ServiceProgram};\nuse crate::dependency_graph::{SymbolDependencyGraph, SymbolId};",
    1,
)
text = text.replace(
    "use crate::server_policy::ServerPolicy;",
    "use crate::server_policy::ServerPolicy;\nuse crate::server_rel::ServerProgram;",
    1,
)
manifest_anchor = '''#[derive(Debug, Clone)]
pub struct RuntimeImage {
'''
if manifest_anchor not in text:
    raise SystemExit("missing RuntimeImage declaration anchor")
executable_enum = r'''#[derive(Debug, Clone)]
pub enum RuntimeExecutable {
    Route(Arc<RouteFile>),
    Module(Arc<ModuleFile>),
    Service(Arc<ServiceProgram>),
    Server(Arc<ServerProgram>),
}

'''
text = text.replace(manifest_anchor, executable_enum + manifest_anchor, 1)
field_anchor = '''    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,
}'''
if field_anchor not in text:
    raise SystemExit("missing RuntimeImage capability field anchor")
text = text.replace(
    field_anchor,
    '''    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,
    pub executables: BTreeMap<SourceId, RuntimeExecutable>,
}''',
    1,
)
method_anchor = '''    pub fn contains_source(&self, id: &SourceId) -> bool {
        self.source(id).is_some()
    }
}'''
if method_anchor not in text:
    raise SystemExit("missing RuntimeImage methods anchor")
methods = r'''    pub fn contains_source(&self, id: &SourceId) -> bool {
        self.source(id).is_some()
    }

    pub fn executable(&self, id: &SourceId) -> Option<&RuntimeExecutable> {
        self.executables.get(id)
    }

    pub fn route_file(&self, id: &SourceId) -> Option<Arc<RouteFile>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Route(file)) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn module_file(&self, id: &SourceId) -> Option<Arc<ModuleFile>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Module(file)) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn service_program(&self, id: &SourceId) -> Option<Arc<ServiceProgram>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Service(program)) => Some(program.clone()),
            _ => None,
        }
    }

    pub fn server_program(&self, id: &SourceId) -> Option<Arc<ServerProgram>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Server(program)) => Some(program.clone()),
            _ => None,
        }
    }
}'''
text = text.replace(method_anchor, methods, 1)
write(path, text)


# ---------------------------------------------------------------------------
# RELC links the parsed units into RuntimeExecutable snapshots and runs Route
# semantic analysis before an image can become executable.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/relc.rs"
text = read(path)
if "use std::sync::Arc;" not in text:
    text = text.replace("use std::path::{Path, PathBuf};", "use std::path::{Path, PathBuf};\nuse std::sync::Arc;", 1)
if "use crate::analyzer::{analyze, Severity};" not in text:
    text = text.replace("use crate::ast::{", "use crate::analyzer::{analyze, Severity};\nuse crate::ast::{", 1)
text = text.replace(
    "use crate::runtime_image::{stable_image_hash, stable_source_hash, RuntimeImage, RuntimeSourceManifest};",
    "use crate::runtime_image::{\n    stable_image_hash, stable_source_hash, RuntimeExecutable, RuntimeImage, RuntimeSourceManifest,\n};",
    1,
)
parse_anchor = '''        let unit = parse_registered_source(source.id(), source.kind(), source.source())?;
        validate_capabilities(source.id(), source.kind(), unit.imports())?;
        compiled.insert(source.id().clone(), unit);'''
if parse_anchor not in text:
    raise SystemExit("missing RELC registered-source parse anchor")
parse_new = '''        let unit = parse_registered_source(source.id(), source.kind(), source.source())?;
        validate_capabilities(source.id(), source.kind(), unit.imports())?;
        if let CompiledUnit::Route(file) = &unit {
            let errors = analyze(file)
                .into_iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Error)
                .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
                .collect::<Vec<_>>();
            if !errors.is_empty() {
                return Err(RelcError::Link(format!(
                    "{} failed Route REL semantic analysis: {}",
                    source.id(),
                    errors.join("; ")
                )));
            }
        }
        compiled.insert(source.id().clone(), unit);'''
text = text.replace(parse_anchor, parse_new, 1)

manifest_setup = '''    let mut capabilities = BTreeMap::new();
    let mut service_assignments = BTreeMap::new();

    for source in registry.iter() {'''
if manifest_setup not in text:
    raise SystemExit("missing RELC manifest setup anchor")
manifest_new = '''    let mut capabilities = BTreeMap::new();
    let mut service_assignments = BTreeMap::new();
    let executables = compiled
        .iter()
        .map(|(id, unit)| (id.clone(), unit.runtime_executable()))
        .collect::<BTreeMap<_, _>>();

    for source in registry.iter() {'''
text = text.replace(manifest_setup, manifest_new, 1)
construct_anchor = '''        service_assignments,
        capabilities,
    })'''
if construct_anchor not in text:
    raise SystemExit("missing RuntimeImage construction anchor")
text = text.replace(
    construct_anchor,
    '''        service_assignments,
        capabilities,
        executables,
    })''',
    1,
)
impl_anchor = '''impl CompiledUnit {
    fn imports(&self) -> &[ImportTarget] {'''
if impl_anchor not in text:
    raise SystemExit("missing CompiledUnit impl anchor")
impl_new = '''impl CompiledUnit {
    fn runtime_executable(&self) -> RuntimeExecutable {
        match self {
            Self::Route(file) => RuntimeExecutable::Route(Arc::new(file.clone())),
            Self::Module(file) => RuntimeExecutable::Module(Arc::new(file.clone())),
            Self::Service(file) => RuntimeExecutable::Service(Arc::new(file.clone())),
            Self::Server(file) => RuntimeExecutable::Server(Arc::new(file.clone())),
        }
    }

    fn imports(&self) -> &[ImportTarget] {'''
text = text.replace(impl_anchor, impl_new, 1)

# RELC-linked apps may use typed uppercase ENV, but not ambient process env.
cap_anchor = '''        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } =
            base
        {
            if name == "ENV" && !RuntimeEnv::can_read(kind) {'''
if cap_anchor not in text:
    raise SystemExit("missing RELC capability anchor")
cap_new = '''        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } =
            base
        {
            if name == "env" {
                return Err(RelcError::Capability {
                    source: source.clone(),
                    message: "legacy process environment capability `env` is disabled in RELC-linked applications; use typed `ENV` for public runtime configuration or Vault for secrets".into(),
                });
            }
            if name == "ENV" && !RuntimeEnv::can_read(kind) {'''
text = text.replace(cap_anchor, cap_new, 1)

# Runtime-core physical Service discovery must compile exactly the bytes that
# ServiceCatalog fingerprinted earlier.
old_service_discovery = '''        for service in catalog.services() {
            out.push(PhysicalRelSource::new(
                RelSourceKind::Service,
                service.name.clone(),
                service.path.clone(),
                fs::read_to_string(&service.path).map_err(|error| {
                    anyhow::anyhow!(
                        "failed to read Runtime Image service source {}: {error}",
                        service.path.display()
                    )
                })?,
            ));
        }'''
if old_service_discovery not in text:
    raise SystemExit("missing Runtime Image service discovery anchor")
new_service_discovery = '''        for service in catalog.services() {
            let source = fs::read_to_string(&service.path).map_err(|error| {
                anyhow::anyhow!(
                    "failed to read Runtime Image service source {}: {error}",
                    service.path.display()
                )
            })?;
            if !service.source_matches(&source) {
                anyhow::bail!(
                    "Runtime Image service source {} changed after ServiceCatalog validation",
                    service.path.display()
                );
            }
            out.push(PhysicalRelSource::new(
                RelSourceKind::Service,
                service.name.clone(),
                service.path.clone(),
                source,
            ));
        }'''
text = text.replace(old_service_discovery, new_service_discovery, 1)
write(path, text)


# ---------------------------------------------------------------------------
# ModuleProgram can be assembled entirely from immutable Runtime Image ASTs.
# RELC owns cross-source graph validation, so this constructor intentionally
# does not re-run the legacy disk loader's blanket import-cycle rejection.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/module_runtime.rs"
text = read(path)
text = text.replace(
    "use crate::paths::{binary_dir, default_module_dir, resolve_custom_import};",
    "use crate::paths::{binary_dir, default_module_dir, resolve_custom_import};\nuse crate::runtime_image::RuntimeImage;",
    1,
)
constructor_anchor = '''    pub fn load_with_services(
        module_dir: &Path,
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        Self::load_internal(module_dir, Some(services))
    }

    fn load_internal('''
if constructor_anchor not in text:
    raise SystemExit("missing ModuleProgram constructor anchor")
constructor = r'''    pub fn load_with_services(
        module_dir: &Path,
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        Self::load_internal(module_dir, Some(services))
    }

    pub fn from_runtime_image_with_services(
        image: &RuntimeImage,
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        let module_dir = default_module_dir();
        let binary_root = module_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(binary_dir);
        let mut modules = HashMap::new();
        let mut errors = Vec::new();

        for id in &image.modules {
            let Some(manifest) = image.source(id) else {
                errors.push(ModuleCompileError {
                    code: "MOD1010",
                    path: module_dir.clone(),
                    line: 0,
                    column: 0,
                    message: format!("Runtime Image is missing module manifest {id}"),
                });
                continue;
            };
            let Some(file) = image.module_file(id) else {
                errors.push(ModuleCompileError {
                    code: "MOD1011",
                    path: module_dir.clone(),
                    line: 0,
                    column: 0,
                    message: format!("Runtime Image is missing executable module {id}"),
                });
                continue;
            };
            let mut path = module_dir.join(&manifest.logical_name);
            path.set_extension("module");
            validate_local(&path, file.as_ref(), Some(services), &mut errors);
            modules.insert(normalize(&path), file);
        }

        if !errors.is_empty() {
            return Err(ModuleCompileErrors(errors));
        }

        Ok(Self {
            binary_root,
            module_dir,
            modules,
        })
    }

    fn load_internal('''
text = text.replace(constructor_anchor, constructor, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Route registration from Runtime Image. No route/module source file is opened
# by the executable serving path after RELC has linked the image.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
if "use crate::runtime_image::RuntimeImage;" not in text:
    text = text.replace(
        "use crate::parser::Parser;",
        "use crate::parser::Parser;\nuse crate::runtime_image::RuntimeImage;",
        1,
    )
logical_anchor = '''pub(crate) fn collision_key_for(url_path: &str) -> String {'''
if logical_anchor not in text:
    raise SystemExit("missing HTTP collision_key_for anchor; base HTTP patch was not applied")
logical_fn = r'''pub(crate) fn url_path_for_logical(logical_name: &str) -> String {
    let mut segments = logical_name
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| route_segment(segment.to_string()))
        .collect::<Vec<_>>();
    if segments.last().is_some_and(|segment| segment == "index") {
        segments.pop();
    }
    format!("/api/{}", segments.join("/"))
}

'''
text = text.replace(logical_anchor, logical_fn + logical_anchor, 1)

append_anchor = '''    Ok(router)
}
'''
# Use the last build_routes close, not an earlier helper.
pos = text.rfind(append_anchor)
if pos < 0:
    raise SystemExit("missing discovery build_routes close")
insert_at = pos + len(append_anchor)
image_builder = r'''

/// Build the executable REL router from the exact immutable ASTs linked by
/// RELC. Disk files are deployment inputs, not runtime authorities.
pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<Router<AppState>> {
    crate::route_collision::validate_image(image)?;
    let module_program = Arc::new(ModuleProgram::from_runtime_image_with_services(
        image,
        service_interfaces,
    )?);
    tracing::info!(
        modules = module_program.len(),
        image = %image.image_id,
        "using Runtime Image module snapshots"
    );

    let mut router: Router<AppState> = Router::new();
    for id in &image.routes {
        let manifest = image
            .source(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no manifest"))?;
        let route_file = image
            .route_file(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no executable snapshot"))?;
        let url_path = url_path_for_logical(&manifest.logical_name);
        tracing::info!(
            source = %id,
            url = %url_path,
            methods = ?route_file.methods.iter().map(|method| &method.verb).collect::<Vec<_>>(),
            "registered Runtime Image Route REL"
        );
        router = router.route(
            &url_path,
            build_method_router(route_file.as_ref(), module_program.clone(), url_path),
        );
    }
    Ok(router)
}
'''
text = text[:insert_at] + image_builder + text[insert_at:]
write(path, text)


# Runtime Image collision validation uses immutable route ASTs rather than a
# second filesystem parse.
path = "engine/crates/route-engine/src/route_collision.rs"
text = read(path)
text = text.replace(
    "use crate::discovery::{collision_key_for, url_path_for};",
    "use crate::discovery::{collision_key_for, url_path_for, url_path_for_logical};\nuse crate::runtime_image::RuntimeImage;",
    1,
)
validate_anchor = '''fn find_collisions(api_dir: &Path) -> anyhow::Result<Vec<RouteCollision>> {'''
if validate_anchor not in text:
    raise SystemExit("missing route collision find anchor")
image_validation = r'''pub(crate) fn validate_image(image: &RuntimeImage) -> anyhow::Result<()> {
    let mut owners: HashMap<(String, String), PathBuf> = HashMap::new();
    let mut collisions = Vec::new();

    for id in &image.routes {
        let manifest = image
            .source(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no manifest"))?;
        let file = image
            .route_file(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no executable snapshot"))?;
        let url_path = url_path_for_logical(&manifest.logical_name);
        let owner = PathBuf::from(id.as_str());

        if let Some(prefix) = RESERVED_NATIVE_API_PREFIXES
            .iter()
            .find(|prefix| is_in_native_namespace(&url_path, prefix))
        {
            collisions.push(RouteCollision {
                path: owner,
                message: format!(
                    "route URL `{url_path}` conflicts with native API namespace `{prefix}`"
                ),
            });
            continue;
        }

        for method in &file.methods {
            let verb = method.verb.to_ascii_lowercase();
            let key = (collision_key_for(&url_path), verb.clone());
            if let Some(existing) = owners.get(&key) {
                collisions.push(RouteCollision {
                    path: owner.clone(),
                    message: format!(
                        "route {} `{}` conflicts with {}",
                        verb.to_ascii_uppercase(),
                        url_path,
                        existing.display()
                    ),
                });
            } else {
                owners.insert(key, owner.clone());
            }
        }
    }

    if collisions.is_empty() {
        return Ok(());
    }
    write_collision_report(&collisions)?;
    Err(anyhow::anyhow!(
        "Runtime Image contains {} route collision(s)",
        collisions.len()
    ))
}

fn write_collision_report(collisions: &[RouteCollision]) -> anyhow::Result<()> {
    let error_path = PathBuf::from("data")
        .join("admin")
        .join("compiler-error.txt");
    if let Some(parent) = error_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut report = String::new();
    for collision in collisions {
        report.push_str(&format!(
            "E3013: {}: {}\n",
            collision.path.display(),
            collision.message
        ));
    }
    fs::write(&error_path, report)?;
    Ok(())
}

'''
text = text.replace(validate_anchor, image_validation + validate_anchor, 1)
# Reuse report helper in legacy validator as well.
old_report = '''    let error_path = PathBuf::from("data")
        .join("admin")
        .join("compiler-error.txt");
    if let Some(parent) = error_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut report = String::new();
    for collision in &collisions {
        report.push_str(&format!(
            "E3013: {}: {}\n",
            collision.path.display(),
            collision.message
        ));
    }
    fs::write(&error_path, report)?;

    Err(anyhow::anyhow!(
        "route compiler found {} route collision(s); see {}",
        collisions.len(),
        error_path.display()
    ))'''
if old_report in text:
    new_report = '''    write_collision_report(&collisions)?;
    Err(anyhow::anyhow!(
        "route compiler found {} route collision(s); see data/admin/compiler-error.txt",
        collisions.len()
    ))'''
    text = text.replace(old_report, new_report, 1)
write(path, text)


# Public runtime builder and RuntimeExecutable export.
path = "engine/crates/route-engine/src/lib.rs"
text = read(path)
text = text.replace(
    "pub use runtime_image::{RuntimeImage, RuntimeImageSlot, RuntimeSourceManifest};",
    "pub use runtime_image::{RuntimeExecutable, RuntimeImage, RuntimeImageSlot, RuntimeSourceManifest};",
    1,
)
public_anchor = '''pub fn build_routes(
    api_dir: &std::path::Path,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    route_collision::validate(api_dir)?;
    discovery::build_routes(api_dir, service_interfaces)
}
'''
if public_anchor not in text:
    raise SystemExit("missing public build_routes anchor")
public_new = public_anchor + r'''
pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    discovery::build_routes_from_image(image, service_interfaces)
}
'''
text = text.replace(public_anchor, public_new, 1)
write(path, text)


# API runtime serving path uses image ASTs. api_dir remains in the signature for
# compatibility/cache setup but is no longer an executable source authority.
path = "engine/crates/api/src/lib.rs"
text = read(path)
text = text.replace(
    '''    api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
    runtime_image: Arc<route_engine::RuntimeImageSlot>,
) -> anyhow::Result<Router> {
    let cors = build_cors_layer(&state);
    let dot_route_routes = route_engine::build_routes(api_dir, service_interfaces)?;''',
    '''    _api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
    runtime_image: Arc<route_engine::RuntimeImageSlot>,
) -> anyhow::Result<Router> {
    let cors = build_cors_layer(&state);
    let image = runtime_image.snapshot();
    let dot_route_routes = route_engine::build_routes_from_image(image.as_ref(), service_interfaces)?;''',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# Service source integrity: ServiceCatalog's SHA-256 becomes an executable
# contract. Every service activation and the child itself reject drift.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/lib.rs"
text = read(path)
anchor = '''#[derive(Debug, Clone, Copy)]
pub struct ServiceDefaults {
'''
if anchor not in text:
    raise SystemExit("missing ServiceDefaults anchor")
service_file_impl = r'''impl ServiceFile {
    pub fn source_digest_hex(&self) -> String {
        digest_hex(&self.source_digest)
    }

    pub fn source_matches(&self, source: &str) -> bool {
        let actual: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        constant_time_eq(&self.source_digest, &actual)
    }
}

pub fn service_source_digest_hex(source: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(source.as_bytes()).into();
    digest_hex(&digest)
}

pub fn service_source_matches_digest(source: &str, expected_hex: &str) -> bool {
    if expected_hex.len() != 64 || !expected_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let actual = service_source_digest_hex(source);
    constant_time_eq(actual.as_bytes(), expected_hex.as_bytes())
}

fn digest_hex(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

'''
text = text.replace(anchor, service_file_impl + anchor, 1)
test_anchor = '''    #[test]
    fn memory_round_trip() {
'''
if test_anchor not in text:
    raise SystemExit("missing service runtime test anchor")
integrity_test = r'''    #[test]
    fn service_source_digest_contract_rejects_drift() {
        let original = ":service[name = test]\nexport function run() { return true; }\n";
        let changed = ":service[name = test]\nexport function run() { return false; }\n";
        let expected = service_source_digest_hex(original);
        assert_eq!(expected.len(), 64);
        assert!(service_source_matches_digest(original, &expected));
        assert!(!service_source_matches_digest(changed, &expected));
    }

'''
text = text.replace(test_anchor, integrity_test + test_anchor, 1)
write(path, text)


path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
spawn_anchor = '''async fn spawn_process(
    file: &ServiceFile,
    fabric: Option<&ServiceFabricEndpoint>,
) -> anyhow::Result<ServiceProcess> {
'''
if spawn_anchor not in text:
    raise SystemExit("missing service spawn anchor")
hardening = r'''fn harden_service_child_environment(command: &mut Command) {
    command.env_clear();
    for name in [
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "RUST_LOG",
        "SETTINGS_PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("RBE_PARENT_LIVENESS_PIPE", "1");
}

'''
text = text.replace(spawn_anchor, hardening + spawn_anchor, 1)
exe_anchor = '''    let exe = std::env::current_exe().context("resolve backend executable")?;
'''
if exe_anchor not in text:
    raise SystemExit("missing service current_exe anchor")
source_guard = r'''    let source = std::fs::read_to_string(&file.path).with_context(|| {
        format!(
            "read service source {} before child activation",
            file.path.display()
        )
    })?;
    if !file.source_matches(&source) {
        anyhow::bail!(
            "service {:?} source changed after catalog validation; refusing to execute drifted REL",
            file.name
        );
    }

'''
text = text.replace(exe_anchor, source_guard + exe_anchor, 1)
command_anchor = '''    let token = random_token();
    let mut command = Command::new(&alias);
    command
        .args(["--service-host", "--service-file"])
        .arg(&file.path);
'''
if command_anchor not in text:
    raise SystemExit("missing service Command anchor")
command_new = '''    let token = random_token();
    let mut command = Command::new(&alias);
    harden_service_child_environment(&mut command);
    command
        .args(["--service-host", "--service-file"])
        .arg(&file.path)
        .arg("--service-source-digest")
        .arg(file.source_digest_hex());
'''
text = text.replace(command_anchor, command_new, 1)
text = text.replace(
    '''        .current_dir(parent)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())''',
    '''        .current_dir(parent)
        .stdin(Stdio::piped())''',
    1,
)
write(path, text)


# Child verifies the parent-validated digest again after it has read the source,
# closing the parent-check -> process-spawn -> child-read race.
path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
source_anchor = '''    let source = std::fs::read_to_string(&service_file).map_err(|error| {
        anyhow::anyhow!(
            "service host failed to read executable body {}: {error}",
            service_file.display()
        )
    })?;
'''
if source_anchor not in text:
    raise SystemExit("missing service host source anchor")
source_check = source_anchor + r'''    let expected_source_digest = value("--service-source-digest");
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_some()
        && expected_source_digest.is_none()
    {
        anyhow::bail!("service host requires the parent-validated source digest");
    }
    if let Some(expected_source_digest) = expected_source_digest {
        if !service_runtime::service_source_matches_digest(&source, &expected_source_digest) {
            anyhow::bail!(
                "service host source changed after parent validation; refusing to execute {}",
                service_file.display()
            );
        }
    }
'''
text = text.replace(source_anchor, source_check, 1)
write(path, text)


# Service Mother receives only the environment it actually needs. Inherited
# loader/proxy/application variables are not ambient Service REL powers.
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
spawn_anchor = '''async fn spawn_process(
    settings_path: impl AsRef<Path>,
'''
if spawn_anchor not in text:
    raise SystemExit("missing Service Mother spawn anchor")
mother_helper = r'''fn harden_service_mother_environment(command: &mut Command, settings_path: &Path) {
    command.env_clear();
    for name in [
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "RUST_LOG",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("SETTINGS_PATH", settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1");
}

'''
text = text.replace(spawn_anchor, mother_helper + spawn_anchor, 1)
old_spawn = '''    let token = new_service_mother_token();
    let mut child = match Command::new(&alias)
        .args(["--service-mother", "--launch-separate"])
        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .current_dir(parent)
        .env("SETTINGS_PATH", &settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
'''
if old_spawn not in text:
    raise SystemExit("missing Service Mother command anchor")
new_spawn = '''    let token = new_service_mother_token();
    let mut command = Command::new(&alias);
    harden_service_mother_environment(&mut command, &settings_path);
    let mut child = match command
        .args(["--service-mother", "--launch-separate"])
        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .current_dir(parent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
'''
text = text.replace(old_spawn, new_spawn, 1)
write(path, text)


# ---------------------------------------------------------------------------
# CI cannot hold a paid runner forever if a process/service test deadlocks.
# ---------------------------------------------------------------------------
path = ".github/workflows/ci.yml"
text = read(path)
if "timeout-minutes:" not in text:
    text = text.replace(
        '''  engine:
    runs-on: ubuntu-latest
''',
        '''  engine:
    runs-on: ubuntu-latest
    timeout-minutes: 30
''',
        1,
    )
write(path, text)


# ---------------------------------------------------------------------------
# Document the real security contract and explicitly reject source deletion as
# a substitute for immutable execution/authentication.
# ---------------------------------------------------------------------------
path = "doc/rel.md"
text = read(path)
marker = '''## Embedded REL files
'''
if marker not in text:
    raise SystemExit("missing REL docs embedded marker")
security_docs = r'''## Runtime source and environment security

A linked application executes the immutable AST/program snapshots stored in its
Runtime Image. Raw `.route`, `.module`, `.service`, and `server.server` files are
boot/deployment inputs; changing them after RELC validation does not change the
active Route/Module program. Service child activation independently verifies the
ServiceCatalog SHA-256 contract until Service execution is fully transported as
an image snapshot too.

Uppercase `ENV` is public typed Runtime Image configuration. It is not the
operating-system process environment and it must not carry credentials. Secrets
belong in Vault. The legacy lowercase `env` process-environment capability is
rejected by RELC-linked applications, and Service Mother/Service child processes
start from a scrubbed environment instead of inheriting arbitrary loader, proxy,
or application variables.

RBE does not destructively delete source files as its primary security boundary.
Deletion does not defend against a host administrator/process-memory compromise,
can break restart/relink workflows, and does not make HTTP endpoints secret. The
runtime contract is instead:

```text
source bytes -> RELC validate/link -> immutable Runtime Image -> execute snapshot
```

A future sealed production bundle may omit raw REL sources entirely after RBE
has a persistent signed executable Runtime Image format. That is a deployment
hardening/obfuscation feature, not a replacement for authentication,
authorization, Vault isolation, rate limiting, or network policy.

'''
if "## Runtime source and environment security" not in text:
    text = text.replace(marker, security_docs + marker, 1)
write(path, text)
