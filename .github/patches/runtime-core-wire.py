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
# Typed Runtime ENV callable surface. Uppercase ENV remains separate from the
# legacy lowercase OS `env` builtin and is supplied only by a runtime host.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/runtime_env.rs"
text = read(path)
text = text.replace(
    "use crate::server_rel::{ServerProgram, ServerSettingBody, ServerValue};",
    "use crate::ast::Value;\nuse crate::server_rel::{ServerProgram, ServerSettingBody, ServerValue};",
    1,
)
text = text.replace(
    '''    InvalidServerEntry(String),
}''',
    '''    InvalidServerEntry(String),
    InvalidCall(String),
}''',
    1,
)
text = text.replace(
    '''            Self::InvalidServerEntry(message) => formatter.write_str(message),
        }''',
    '''            Self::InvalidServerEntry(message) | Self::InvalidCall(message) => {
                formatter.write_str(message)
            }
        }''',
    1,
)
anchor = '''    pub fn can_read(kind: RelSourceKind) -> bool {
'''
if anchor not in text:
    raise SystemExit("missing RuntimeEnv can_read anchor")
runtime_call = r'''    pub(crate) fn call_rel(
        &self,
        function: &str,
        args: &[Value],
    ) -> Result<Value, RuntimeEnvError> {
        let name = match args {
            [Value::String(name)] => name.as_str(),
            _ => {
                return Err(RuntimeEnvError::InvalidCall(format!(
                    "ENV.{function}() requires exactly one string key"
                )))
            }
        };
        match function {
            "has" => Ok(Value::Bool(self.has(name))),
            "get" => Ok(self.get(name).map(json_to_rel).unwrap_or(Value::Null)),
            "require" => self.require(name).map(json_to_rel),
            "string" => self.string(name).map(|value| Value::String(value.to_string())),
            "number" => self.number(name).map(Value::Number),
            "bool" => self.bool(name).map(Value::Bool),
            "object" => self.object(name).map(|value| {
                Value::Object(
                    value
                        .iter()
                        .map(|(key, value)| (key.clone(), json_to_rel(value)))
                        .collect(),
                )
            }),
            "array" => self
                .array(name)
                .map(|value| Value::Array(value.iter().map(json_to_rel).collect())),
            other => Err(RuntimeEnvError::InvalidCall(format!(
                "ENV.{other}() does not exist"
            ))),
        }
    }

'''
text = text.replace(anchor, runtime_call + anchor, 1)
helper_anchor = '''fn merge_layer(
'''
if helper_anchor not in text:
    raise SystemExit("missing RuntimeEnv merge_layer anchor")
json_helper = r'''fn json_to_rel(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Bool(*value),
        JsonValue::Number(value) => Value::Number(value.as_f64().unwrap_or(0.0)),
        JsonValue::String(value) => Value::String(value.clone()),
        JsonValue::Array(values) => Value::Array(values.iter().map(json_to_rel).collect()),
        JsonValue::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json_to_rel(value)))
                .collect(),
        ),
    }
}

'''
text = text.replace(helper_anchor, json_helper + helper_anchor, 1)
test_anchor = '''    #[test]
    fn routes_do_not_receive_runtime_env_by_default() {
'''
if test_anchor not in text:
    raise SystemExit("missing RuntimeEnv test anchor")
runtime_test = r'''    #[test]
    fn rel_call_surface_preserves_json_types() {
        let server = compile_server_source("server Main {}").unwrap();
        let settings = BTreeMap::from([
            ("NAME".to_string(), JsonValue::String("rbe".into())),
            ("COUNT".to_string(), JsonValue::from(7)),
            ("FLAGS".to_string(), serde_json::json!([true, false])),
        ]);
        let env = RuntimeEnv::resolve(&BTreeMap::new(), &server, &settings).unwrap();
        assert_eq!(
            env.call_rel("string", &[Value::String("NAME".into())]).unwrap(),
            Value::String("rbe".into())
        );
        assert_eq!(
            env.call_rel("number", &[Value::String("COUNT".into())]).unwrap(),
            Value::Number(7.0)
        );
        assert!(matches!(
            env.call_rel("array", &[Value::String("FLAGS".into())]).unwrap(),
            Value::Array(values) if values.len() == 2
        ));
        assert_eq!(
            env.call_rel("get", &[Value::String("MISSING".into())]).unwrap(),
            Value::Null
        );
        assert!(env
            .call_rel("require", &[Value::String("MISSING".into())])
            .is_err());
    }

'''
text = text.replace(test_anchor, runtime_test + test_anchor, 1)
write(path, text)


# Builtin contract knows ENV methods, but Route REL whitelist intentionally
# still excludes ENV.
path = "engine/crates/route-engine/src/modules.rs"
text = read(path)
old = '''        "env" => matches!(function, "get"),
'''
new = '''        "env" => matches!(function, "get"),
        "ENV" => matches!(
            function,
            "get" | "has" | "require" | "string" | "number" | "bool" | "object" | "array"
        ),
'''
if old not in text:
    raise SystemExit("missing modules ENV anchor")
text = text.replace(old, new, 1)
write(path, text)


# Composite runtime host: Video Manager plus the immutable Runtime Image ENV.
write(
    "engine/crates/route-engine/src/video_host.rs",
    r'''//! Async language bridge from `.module` execution into runtime-owned capabilities.

use std::sync::Arc;

use core_lib::{AppState, VideoLanguage};

use crate::ast::Value;
use crate::module_eval::{HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError};
use crate::runtime_image::RuntimeImage;

pub struct RuntimeHostCapabilities {
    video: VideoLanguage,
    image: Arc<RuntimeImage>,
}

impl RuntimeHostCapabilities {
    pub fn from_state_and_image(state: &AppState, image: Arc<RuntimeImage>) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
        }
    }
}

impl HostCapabilityCaller for RuntimeHostCapabilities {
    fn call<'a>(
        &'a self,
        scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            if module == "ENV" {
                let value = self
                    .image
                    .environment
                    .call_rel(function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: "ENV3000",
                        message: error.to_string(),
                    })?;
                return Ok(Some(value));
            }
            if !matches!(module, "vm" | "video-manager") {
                return Ok(None);
            }
            let owner = scope.ok_or_else(|| ModuleEvalError {
                code: "VID3003",
                message: "Video Manager capability requires a resolved .module identity".into(),
            })?;
            let args = args.into_iter().map(value_to_json).collect::<Vec<_>>();
            let value =
                self.video
                    .call(&owner, function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: error.code,
                        message: error.message,
                    })?;
            Ok(Some(value_from_json(value)?))
        })
    }
}

fn value_to_json(value: Value) -> serde_json::Value {
    match value {
        Value::String(value) => serde_json::Value::String(value),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(value) => serde_json::Value::Bool(value),
        Value::Null => serde_json::Value::Null,
        Value::Object(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, value_to_json(value)))
                .collect(),
        ),
        Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
    }
}

fn value_from_json(value: serde_json::Value) -> Result<Value, ModuleEvalError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Bool(value)),
        serde_json::Value::Number(value) => value
            .as_f64()
            .map(Value::Number)
            .ok_or_else(|| ModuleEvalError {
                code: "VID3002",
                message: "Video Manager returned a number outside the RBE numeric range".into(),
            }),
        serde_json::Value::String(value) => Ok(Value::String(value)),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(value_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_json::Value::Object(fields) => {
            let mut out = std::collections::HashMap::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, value_from_json(value)?);
            }
            Ok(Value::Object(out))
        }
    }
}
''',
)


# ---------------------------------------------------------------------------
# Runtime Image identity includes settings input, not only REL source bytes.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/runtime_image.rs"
text = read(path)
anchor = '''#[cfg(test)]
mod tests {
'''
if anchor not in text:
    raise SystemExit("missing runtime_image tests anchor")
hash_code = r'''pub(crate) fn stable_image_hash(source_hash: u64, settings: &serde_json::Value) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V1");
    feed_hash(&mut hash, &source_hash.to_be_bytes());
    hash_json(&mut hash, settings);
    hash
}

fn feed_hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn hash_json(hash: &mut u64, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => feed_hash(hash, b"N"),
        serde_json::Value::Bool(value) => feed_hash(hash, if *value { b"T" } else { b"F" }),
        serde_json::Value::Number(value) => {
            feed_hash(hash, b"D");
            feed_hash(hash, value.to_string().as_bytes());
            feed_hash(hash, &[0]);
        }
        serde_json::Value::String(value) => {
            feed_hash(hash, b"S");
            feed_hash(hash, &(value.len() as u64).to_be_bytes());
            feed_hash(hash, value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            feed_hash(hash, b"A");
            feed_hash(hash, &(values.len() as u64).to_be_bytes());
            for value in values {
                hash_json(hash, value);
            }
        }
        serde_json::Value::Object(values) => {
            feed_hash(hash, b"O");
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            feed_hash(hash, &(keys.len() as u64).to_be_bytes());
            for key in keys {
                feed_hash(hash, &(key.len() as u64).to_be_bytes());
                feed_hash(hash, key.as_bytes());
                hash_json(hash, &values[key]);
            }
        }
    }
}

'''
text = text.replace(anchor, hash_code + anchor, 1)
test_anchor = '''    #[test]
    fn source_hash_is_deterministic_and_sensitive_to_content() {
'''
settings_test = r'''    #[test]
    fn image_hash_is_sensitive_to_settings_and_key_order_is_stable() {
        let source_hash = 42;
        let first = serde_json::json!({"runtimeEnv": {"A": 1, "B": true}});
        let reordered = serde_json::json!({"runtimeEnv": {"B": true, "A": 1}});
        let changed = serde_json::json!({"runtimeEnv": {"A": 2, "B": true}});
        assert_eq!(
            stable_image_hash(source_hash, &first),
            stable_image_hash(source_hash, &reordered)
        );
        assert_ne!(
            stable_image_hash(source_hash, &first),
            stable_image_hash(source_hash, &changed)
        );
    }

'''
if test_anchor not in text:
    raise SystemExit("missing runtime_image test anchor")
text = text.replace(test_anchor, settings_test + test_anchor, 1)
write(path, text)


# ---------------------------------------------------------------------------
# RELC physical discovery, canonical nested module names and Service Fabric
# cycle validation. Generic symbol recursion remains legal.
# ---------------------------------------------------------------------------
path = "engine/crates/route-engine/src/relc.rs"
text = read(path)
text = text.replace(
    "use std::collections::{BTreeMap, BTreeSet};\nuse std::fmt;\nuse std::path::PathBuf;",
    "use std::collections::{BTreeMap, BTreeSet};\nuse std::fmt;\nuse std::fs;\nuse std::path::{Path, PathBuf};",
    1,
)
text = text.replace(
    "use serde_json::Value as JsonValue;",
    "use serde_json::Value as JsonValue;\nuse service_runtime::ServiceCatalog;",
    1,
)
text = text.replace(
    "use crate::runtime_image::{stable_source_hash, RuntimeImage, RuntimeSourceManifest};",
    "use crate::runtime_image::{stable_image_hash, stable_source_hash, RuntimeImage, RuntimeSourceManifest};",
    1,
)
impl_anchor = '''impl PhysicalRelSource {
'''
idx = text.index(impl_anchor)
# insert discovery after impl block by finding the first '\n}\n\n' following it
end_impl = text.index("\n}\n\n", idx) + 4
discovery_code = r'''pub fn discover_physical_rel_sources(
    api_dir: &Path,
    module_dir: &Path,
    service_catalog: Option<&ServiceCatalog>,
) -> anyhow::Result<Vec<PhysicalRelSource>> {
    let mut out = Vec::new();
    collect_physical_dir(api_dir, RelSourceKind::Route, "route", &mut out)?;
    collect_physical_dir(module_dir, RelSourceKind::Module, "module", &mut out)?;
    if let Some(catalog) = service_catalog {
        for service in catalog.services() {
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
        }
    }
    Ok(out)
}

fn collect_physical_dir(
    root: &Path,
    kind: RelSourceKind,
    extension: &str,
    out: &mut Vec<PhysicalRelSource>,
) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut paths = Vec::new();
    collect_physical_paths(root, extension, &mut paths)?;
    paths.sort();
    for path in paths {
        let relative = path.strip_prefix(root).unwrap_or(&path).with_extension("");
        let logical_name = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        out.push(PhysicalRelSource::new(
            kind,
            logical_name,
            path.clone(),
            fs::read_to_string(&path).map_err(|error| {
                anyhow::anyhow!("failed to read REL source {}: {error}", path.display())
            })?,
        ));
    }
    Ok(())
}

fn collect_physical_paths(
    dir: &Path,
    extension: &str,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_physical_paths(&path, extension, out)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            out.push(path);
        }
    }
    Ok(())
}

'''
text = text[:end_impl] + discovery_code + text[end_impl:]
text = text.replace(
    '''    // PASS 3/4: declaration + import target collection.
    validate_import_targets(&registry, &compiled)?;

    // PASS 5:''',
    '''    // PASS 3/4: declaration + import target collection.
    validate_import_targets(&registry, &compiled)?;
    validate_service_dependency_cycles(&registry, &compiled)?;

    // PASS 5:''',
    1,
)
text = text.replace(
    '''    let source_hash =
        stable_source_hash(registry.iter().map(|source| (source.id(), source.source())));
    Ok(RuntimeImage {
        image_id: format!("rbe-{source_hash:016x}"),''',
    '''    let source_hash =
        stable_source_hash(registry.iter().map(|source| (source.id(), source.source())));
    let image_hash = stable_image_hash(source_hash, settings_json);
    Ok(RuntimeImage {
        image_id: format!("rbe-{image_hash:016x}"),''',
    1,
)
logical_old = '''fn logical_module_name(path: &str) -> String {
    let normalized = path.replace('\\\\', "/");
    let after_amp = normalized.rsplit('&').next().unwrap_or(&normalized);
    let leaf = after_amp.rsplit('/').next().unwrap_or(after_amp);
    leaf.strip_suffix(".module").unwrap_or(leaf).to_string()
}
'''
# The source contains a Rust '\\' char literal represented as two backslashes in text.
if logical_old not in text:
    logical_old = '''fn logical_module_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let after_amp = normalized.rsplit('&').next().unwrap_or(&normalized);
    let leaf = after_amp.rsplit('/').next().unwrap_or(after_amp);
    leaf.strip_suffix(".module").unwrap_or(leaf).to_string()
}
'''
logical_new = '''fn logical_module_name(path: &str) -> String {
    let normalized = path.replace('\\\\', "/");
    let after_amp = normalized.rsplit('&').next().unwrap_or(&normalized);
    let relative = after_amp.strip_prefix("./").unwrap_or(after_amp);
    let relative = relative.strip_prefix("module/").unwrap_or(relative);
    relative
        .strip_suffix(".module")
        .unwrap_or(relative)
        .to_string()
}
'''
if logical_old not in text:
    raise SystemExit("missing logical_module_name anchor")
text = text.replace(logical_old, logical_new, 1)
cycle_anchor = '''fn logical_module_name(path: &str) -> String {
'''
if cycle_anchor not in text:
    raise SystemExit("missing cycle insertion anchor")
cycle_code = r'''fn service_dependency(import: &ImportTarget) -> Option<&str> {
    match import_base(import) {
        ImportTarget::Service(service) | ImportTarget::ServiceFunction { service, .. } => {
            Some(service.as_str())
        }
        _ => None,
    }
}

fn validate_service_dependency_cycles(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
) -> Result<(), RelcError> {
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    for (id, unit) in compiled {
        let CompiledUnit::Service(service) = unit else {
            continue;
        };
        let source = registry
            .get(id)
            .ok_or_else(|| RelcError::Link(format!("compiled service {id} is not registered")))?;
        let dependencies = service
            .imports
            .iter()
            .filter_map(service_dependency)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        graph.insert(source.logical_name().to_string(), dependencies);
    }

    fn visit(
        node: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        stack: &mut Vec<String>,
    ) -> Result<(), RelcError> {
        if visited.contains(node) {
            return Ok(());
        }
        if !visiting.insert(node.to_string()) {
            let start = stack.iter().position(|value| value == node).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(node.to_string());
            return Err(RelcError::Link(format!(
                "synchronous Service Fabric dependency cycle would deadlock: {}",
                cycle.join(" -> ")
            )));
        }
        stack.push(node.to_string());
        if let Some(dependencies) = graph.get(node) {
            for dependency in dependencies {
                visit(dependency, graph, visiting, visited, stack)?;
            }
        }
        stack.pop();
        visiting.remove(node);
        visited.insert(node.to_string());
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = Vec::new();
    for node in graph.keys() {
        visit(node, &graph, &mut visiting, &mut visited, &mut stack)?;
    }
    Ok(())
}

'''
text = text.replace(cycle_anchor, cycle_code + cycle_anchor, 1)
test_anchor = '''    #[test]
    fn route_runtime_env_is_a_capability_error_not_a_grammar_error() {
'''
if test_anchor not in text:
    raise SystemExit("missing RELC test anchor")
relc_tests = r'''    #[test]
    fn runtime_image_id_changes_when_settings_change() {
        let server = "server Main {}";
        let first = compile_runtime_image(
            server,
            Vec::new(),
            &serde_json::json!({"runtimeEnv": {"MODE": "one"}}),
        )
        .unwrap();
        let second = compile_runtime_image(
            server,
            Vec::new(),
            &serde_json::json!({"runtimeEnv": {"MODE": "two"}}),
        )
        .unwrap();
        assert_eq!(first.source_hash, second.source_hash);
        assert_ne!(first.image_id, second.image_id);
    }

    #[test]
    fn rejects_synchronous_service_fabric_cycles_during_link() {
        let services = vec![
            PhysicalRelSource::new(
                RelSourceKind::Service,
                "a",
                "service/a.service",
                r#":import[service:b]
                   :service[name = a]
                   export function run() { return b.run(); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Service,
                "b",
                "service/b.service",
                r#":import[service:a]
                   :service[name = b]
                   export function run() { return a.run(); }"#,
            ),
        ];
        let error = compile_runtime_image("server Main {}", services, &serde_json::json!({}))
            .expect_err("service dependency cycle must fail");
        assert!(error.to_string().contains("would deadlock"));
    }

    #[test]
    fn nested_module_logical_names_preserve_the_relative_path() {
        assert_eq!(logical_module_name("./module/users/profile.module"), "users/profile");
        assert_eq!(logical_module_name("module&users/profile"), "users/profile");
    }

'''
text = text.replace(test_anchor, relc_tests + test_anchor, 1)
write(path, text)


# Export physical source discovery.
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use relc::{compile_runtime_image, PhysicalRelSource, RelcError};",
    "pub use relc::{compile_runtime_image, discover_physical_rel_sources, PhysicalRelSource, RelcError};",
)


# ---------------------------------------------------------------------------
# Backend Runtime Image compilation and ServerPolicy -> effective Config.
# ---------------------------------------------------------------------------
write(
    "engine/crates/backend/src/runtime_image_boot.rs",
    r'''use std::path::Path;

use config::Config;
use route_engine::{RuntimeImage, ServerPolicy, ServerValue};
use service_runtime::ServiceCatalog;

pub fn compile(config: &Config, catalog: Option<&ServiceCatalog>) -> anyhow::Result<RuntimeImage> {
    let root = runtime_paths::binary_dir();
    let server_path = root.join("server.server");
    let server_source = if server_path.is_file() {
        std::fs::read_to_string(&server_path).map_err(|error| {
            anyhow::anyhow!("failed to read Server REL root {}: {error}", server_path.display())
        })?
    } else {
        tracing::warn!(
            path = %server_path.display(),
            "server.server is absent; using compatibility Server REL root"
        );
        "server Main {}\n".to_string()
    };
    let physical = route_engine::discover_physical_rel_sources(
        &route_engine::default_api_dir(),
        &route_engine::default_module_dir(),
        catalog,
    )?;
    let settings = effective_settings_json(config);
    let image = route_engine::compile_runtime_image(&server_source, physical, &settings)
        .map_err(|error| anyhow::anyhow!("Runtime Image compile failed: {error}"))?;
    tracing::info!(
        image = %image.image_id,
        source_hash = format_args!("{:016x}", image.source_hash),
        routes = image.routes.len(),
        modules = image.modules.len(),
        services = image.services.len(),
        server = %image.server_policy.server_name,
        status = image.server_policy.status.as_str(),
        "linked immutable Runtime Image"
    );
    Ok(image)
}

pub fn apply_server_policy(config: &mut Config, policy: &ServerPolicy) -> anyhow::Result<()> {
    if let Some(value) = string(policy, "listener.host")? {
        config.api.host = value;
    }
    if let Some(value) = integer(policy, "listener.port")? {
        config.api.port = u16::try_from(value)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| anyhow::anyhow!("ServerPolicy listener.port must fit 1..=65535"))?;
    }
    if let Some(value) = integer(policy, "requestTimeoutMs")? {
        config.api.request_timeout_ms = value;
    }
    if let Some(value) = integer(policy, "maxBodySizeBytes")? {
        config.api.max_body_size_bytes = usize::try_from(value)
            .map_err(|_| anyhow::anyhow!("ServerPolicy maxBodySizeBytes exceeds usize"))?;
    }
    if let Some(value) = boolean(policy, "trustedProxyHeaders")? {
        config.security.trusted_proxy_headers = value;
    }
    if let Some(value) = string_array(policy, "corsAllowedOrigins")? {
        config.security.cors_allowed_origins = value;
    }
    if let Some(value) = integer(policy, "maxJsonPayloadBytes")? {
        config.security.max_json_payload_bytes = usize::try_from(value)
            .map_err(|_| anyhow::anyhow!("ServerPolicy maxJsonPayloadBytes exceeds usize"))?;
    }
    if let Some(value) = string(policy, "cspPolicy")? {
        config.security.csp_policy = value;
    }
    Ok(())
}

fn effective_settings_json(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "api": {
            "host": config.api.host,
            "port": config.api.port,
            "requestTimeoutMs": config.api.request_timeout_ms,
            "maxBodySizeBytes": config.api.max_body_size_bytes,
        },
        "security": {
            "trustedProxyHeaders": config.security.trusted_proxy_headers,
            "corsAllowedOrigins": config.security.cors_allowed_origins,
            "maxJsonPayloadBytes": config.security.max_json_payload_bytes,
            "cspPolicy": config.security.csp_policy,
        },
        "runtimeEnv": config.runtime_env,
    })
}

fn policy_value<'a>(policy: &'a ServerPolicy, key: &str) -> Option<&'a ServerValue> {
    policy.get(key).map(|value| &value.value)
}

fn string(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<String>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::String(value) | ServerValue::Ident(value)) => Ok(Some(value.clone())),
        Some(value) => anyhow::bail!("ServerPolicy {key} must be a string/identifier, got {value:?}"),
    }
}

fn boolean(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<bool>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Bool(value)) => Ok(Some(*value)),
        Some(value) => anyhow::bail!("ServerPolicy {key} must be boolean, got {value:?}"),
    }
}

fn integer(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<u64>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Number(value))
            if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64 =>
        {
            Ok(Some(*value as u64))
        }
        Some(value) => anyhow::bail!("ServerPolicy {key} must be a non-negative integer, got {value:?}"),
    }
}

fn string_array(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<Vec<String>>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                ServerValue::String(value) | ServerValue::Ident(value) => Ok(value.clone()),
                other => Err(anyhow::anyhow!(
                    "ServerPolicy {key} array entries must be strings, got {other:?}"
                )),
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some),
        Some(value) => anyhow::bail!("ServerPolicy {key} must be an array, got {value:?}"),
    }
}

#[allow(dead_code)]
fn _path_marker(_: &Path) {}
''',
)


# ---------------------------------------------------------------------------
# Backend boot: compile/link before maintenance listener and expensive infra,
# then apply resolved policy and install one atomic RuntimeImageSlot.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
text = text.replace("mod port_guard;", "mod port_guard;\nmod runtime_image_boot;", 1)
start = text.index('    let settings_path =\n        std::env::var("SETTINGS_PATH")')
end_marker = '    lifecycle.set(BackendState::ServicesStarting);\n'
end = text.index(end_marker, start) + len(end_marker)
new_boot = r'''    let settings_path =
        std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".to_string());
    boot_trace(format!("settings path={settings_path}"));
    let mut config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("failed to load {settings_path}: {error}"))?;
    let refresh_interval =
        Duration::from_secs(config.runtime.process_refresh_hours.saturating_mul(3600));
    let maintenance = Arc::new(MaintenanceMetrics::new(
        config.runtime.process_refresh_hours,
    ));
    boot_trace("settings loaded");

    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");

    let mut supervisor = Supervisor::new(RestartPolicy::default());
    let lifecycle = supervisor.lifecycle();
    lifecycle.set(BackendState::Initializing);
    let state_rx = lifecycle.subscribe();
    tokio::spawn(async move {
        supervisor.run().await;
    });
    boot_trace("supervisor spawned");

    let io = atomic_io::AtomicIo::new();
    let admin_dir = runtime_paths::default_admin_dir();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();
    boot_trace("error-client initialized, panic hook installed");
    lifecycle.set(BackendState::ConfigurationLoaded);

    // Compile every executable service and the complete REL Runtime Image
    // before binding even the maintenance responder. A malformed source or
    // invalid ServerPolicy cannot leave the backend half-started.
    let service_catalog = service_boot::compile(&config.services, &io)?;
    let service_interfaces: route_engine::ServiceInterfaces = service_catalog
        .as_ref()
        .map(|catalog| {
            catalog
                .services()
                .iter()
                .map(|service| {
                    (
                        service.name.clone(),
                        service.exports.iter().cloned().collect::<HashSet<_>>(),
                    )
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let runtime_image = runtime_image_boot::compile(&config, service_catalog.as_ref())?;
    runtime_image_boot::apply_server_policy(&mut config, &runtime_image.server_policy)?;
    let runtime_image = Arc::new(route_engine::RuntimeImageSlot::new(runtime_image));
    let config = Arc::new(config);
    boot_trace(format!(
        "effective api bind={}:{}",
        config.api.host, config.api.port
    ));
    tracing::info!(
        path = %settings_path,
        refresh_hours = config.runtime.process_refresh_hours,
        image = %runtime_image.snapshot().image_id,
        "configuration and Runtime Image loaded"
    );

    // Reclaim stale/crashed prior backend listeners BEFORE the temporary
    // responder starts. The responder is this same executable, so running the
    // old image-name based reclaim after it binds would mistake it for stale RBE.
    if config.runtime.reclaim_port {
        port_guard::reclaim_port_if_needed(config.api.port);
    }

    let maintenance_notice =
        maintenance_notice::MaintenanceNoticeProcess::spawn(&config.api.host, config.api.port)
            .await?;
    tracing::info!(
        pid = maintenance_notice.pid(),
        host = %config.api.host,
        port = config.api.port,
        "temporary API maintenance responder ready"
    );
    boot_trace("temporary API maintenance responder ready");
    lifecycle.set(BackendState::ServicesStarting);
'''
text = text[:start] + new_boot + text[end:]
text = text.replace(
    "    let router = api::build_router(app_state, &api_dir, &service_interfaces)?;",
    "    let router = api::build_router(app_state, &api_dir, &service_interfaces, runtime_image)?;",
    1,
)
write(path, text)


# API publishes the slot as an Axum Extension without putting route-engine in
# core-lib AppState (which would create a crate dependency cycle).
path = "engine/crates/api/src/lib.rs"
text = read(path)
text = text.replace("use std::path::Path;", "use std::path::Path;\nuse std::sync::Arc;", 1)
old_sig = '''pub fn build_router(
    state: AppState,
    api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
) -> anyhow::Result<Router> {'''
new_sig = '''pub fn build_router(
    state: AppState,
    api_dir: &Path,
    service_interfaces: &route_engine::ServiceInterfaces,
    runtime_image: Arc<route_engine::RuntimeImageSlot>,
) -> anyhow::Result<Router> {'''
if old_sig not in text:
    raise SystemExit("missing api build_router signature")
text = text.replace(old_sig, new_sig, 1)
old_return = "    Ok(router.layer(middleware).with_state(state))"
new_return = '''    Ok(router
        .layer(middleware)
        .layer(axum::Extension(runtime_image))
        .with_state(state))'''
if old_return not in text:
    raise SystemExit("missing api router return")
text = text.replace(old_return, new_return, 1)
write(path, text)


# Each REL request snapshots one immutable image before consuming Request.
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
text = text.replace(
    "use crate::video_host::VideoHostCapabilities;",
    "use crate::video_host::RuntimeHostCapabilities;",
    1,
)
execute_anchor = '''    let path = request.uri().path().to_string();
    let args = if takes_request {'''
execute_new = '''    let path = request.uri().path().to_string();
    let image = match request
        .extensions()
        .get::<Arc<crate::runtime_image::RuntimeImageSlot>>()
    {
        Some(slot) => slot.snapshot(),
        None => {
            let error = "Runtime Image extension is unavailable";
            tracing::error!(path = %path, error, "REL request has no active Runtime Image");
            return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
        }
    };
    let args = if takes_request {'''
if execute_anchor not in text:
    raise SystemExit("missing discovery execute anchor; HTTP patch must run first")
text = text.replace(execute_anchor, execute_new, 1)
text = text.replace(
    "        Arc::new(VideoHostCapabilities::from_state(&state)),",
    "        Arc::new(RuntimeHostCapabilities::from_state_and_image(&state, image)),",
    1,
)
write(path, text)
