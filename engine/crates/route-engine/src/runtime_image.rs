//! Immutable linked REL Runtime Image and transactional activation slot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};

use crate::ast::{ModuleFile, RouteFile, ServiceProgram};
use crate::dependency_graph::{SymbolDependencyGraph, SymbolId};
use crate::middleware_plan::MiddlewarePlan;
use crate::runtime_env::RuntimeEnv;
use crate::server_policy::ServerPolicy;
use crate::server_rel::ServerProgram;
use crate::source_registry::{RelSourceKind, SourceId};
use crate::wasm_compiler::{
    RouteWasmArtifact, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {
    pub id: SourceId,
    pub kind: RelSourceKind,
    pub logical_name: String,
    pub exports: Vec<String>,
    pub imports: Vec<String>,
    pub route_path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RuntimeExecutable {
    Route(Arc<RouteFile>),
    Module(Arc<ModuleFile>),
    Service(Arc<ServiceProgram>),
    Server(Arc<ServerProgram>),
}

#[derive(Debug, Clone)]
pub struct RuntimeImage {
    pub image_id: String,
    pub source_hash: String,
    pub server_policy: ServerPolicy,
    pub environment: RuntimeEnv,
    pub routes: Vec<SourceId>,
    pub modules: Vec<SourceId>,
    pub services: Vec<SourceId>,
    pub sources: Vec<RuntimeSourceManifest>,
    pub symbol_table: BTreeSet<SymbolId>,
    pub dependency_graph: SymbolDependencyGraph,
    pub recursive_groups: Vec<Vec<SymbolId>>,
    pub middleware_plan: MiddlewarePlan,
    pub service_assignments: BTreeMap<String, String>,
    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,
    /// Exact native route artifacts pinned at image-link time. These bytes
    /// are the only route WASM payloads eligible for Container registration.
    pub route_wasm_artifacts: BTreeMap<SourceId, RouteWasmArtifact>,
    /// Routes outside the current native compiler subset remain explicit.
    pub route_wasm_fallbacks: BTreeMap<SourceId, String>,
    pub executables: BTreeMap<SourceId, RuntimeExecutable>,
}

impl RuntimeImage {
    pub fn source(&self, id: &SourceId) -> Option<&RuntimeSourceManifest> {
        self.sources.iter().find(|source| &source.id == id)
    }

    pub fn contains_source(&self, id: &SourceId) -> bool {
        self.source(id).is_some()
    }

    pub fn executable(&self, id: &SourceId) -> Option<&RuntimeExecutable> {
        self.executables.get(id)
    }

    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {
        self.route_wasm_artifacts.get(id)
    }

    pub fn route_wasm_fallback(&self, id: &SourceId) -> Option<&str> {
        self.route_wasm_fallbacks.get(id).map(String::as_str)
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
}

/// Holds the currently active immutable image. Readers clone the Arc and are
/// never affected by a later activation. A failed compilation never reaches
/// this slot, which gives reloads the A-running/B-compiling transactional model.
pub struct RuntimeImageSlot {
    current: RwLock<Arc<RuntimeImage>>,
}

impl RuntimeImageSlot {
    pub fn new(initial: RuntimeImage) -> Self {
        Self {
            current: RwLock::new(Arc::new(initial)),
        }
    }

    pub fn snapshot(&self) -> Arc<RuntimeImage> {
        self.current
            .read()
            .expect("Runtime Image slot poisoned")
            .clone()
    }

    pub fn activate(&self, next: RuntimeImage) -> Arc<RuntimeImage> {
        let next = Arc::new(next);
        let mut current = self.current.write().expect("Runtime Image slot poisoned");
        std::mem::replace(&mut *current, next)
    }
}

pub(crate) fn stable_source_hash<'a>(
    sources: impl Iterator<Item = (&'a SourceId, &'a str)>,
) -> String {
    // Runtime Image authority is security-sensitive. Canonicalize the source
    // set and bind the complete SourceId + source bytes with SHA-256 instead of
    // the former 64-bit non-cryptographic FNV identity.
    let mut sources = sources.collect::<Vec<_>>();
    sources.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_SOURCE_SET_V1");
    feed_hash(&mut hash, &(sources.len() as u64).to_be_bytes());
    for (id, source) in sources {
        feed_hash(&mut hash, id.as_str().as_bytes());
        feed_hash(&mut hash, source.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(crate) fn stable_image_hash(source_hash: &str, settings: &serde_json::Value) -> String {
    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V3");
    feed_hash(&mut hash, source_hash.as_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_ABI_VERSION.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_COMPILER_VERSION.to_be_bytes());
    hash_json(&mut hash, settings);
    hex::encode(hash.finalize())
}

fn feed_hash(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn hash_json(hash: &mut Sha256, value: &serde_json::Value) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::RelSourceKind;

    #[test]
    fn image_hash_is_sensitive_to_settings_and_key_order_is_stable() {
        let source_hash = "2a".repeat(32);
        let first = serde_json::json!({"runtimeEnv": {"A": 1, "B": true}});
        let reordered = serde_json::json!({"runtimeEnv": {"B": true, "A": 1}});
        let changed = serde_json::json!({"runtimeEnv": {"A": 2, "B": true}});
        let first_hash = stable_image_hash(&source_hash, &first);
        assert_eq!(first_hash, stable_image_hash(&source_hash, &reordered));
        assert_ne!(first_hash, stable_image_hash(&source_hash, &changed));
        assert_eq!(first_hash.len(), 64);
        assert!(first_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }

    #[test]
    fn source_hash_is_deterministic_and_sensitive_to_content() {
        let id = SourceId::physical(RelSourceKind::Module, "A").unwrap();
        let first = stable_source_hash(std::iter::once((&id, "one")));
        let same = stable_source_hash(std::iter::once((&id, "one")));
        let other = stable_source_hash(std::iter::once((&id, "two")));
        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_eq!(first.len(), 64);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}
