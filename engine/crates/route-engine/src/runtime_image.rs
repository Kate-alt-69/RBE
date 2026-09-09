//! Immutable linked REL Runtime Image and transactional activation slot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::dependency_graph::{SymbolDependencyGraph, SymbolId};
use crate::middleware_plan::MiddlewarePlan;
use crate::runtime_env::RuntimeEnv;
use crate::server_policy::ServerPolicy;
use crate::source_registry::{RelSourceKind, SourceId};

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {
    pub id: SourceId,
    pub kind: RelSourceKind,
    pub logical_name: String,
    pub exports: Vec<String>,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeImage {
    pub image_id: String,
    pub source_hash: u64,
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
}

impl RuntimeImage {
    pub fn source(&self, id: &SourceId) -> Option<&RuntimeSourceManifest> {
        self.sources.iter().find(|source| &source.id == id)
    }

    pub fn contains_source(&self, id: &SourceId) -> bool {
        self.source(id).is_some()
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
) -> u64 {
    // Explicit FNV-1a avoids relying on std's non-contractual DefaultHasher
    // algorithm for Runtime Image identity.
    let mut hash = 0xcbf29ce484222325u64;
    for (id, source) in sources {
        for byte in id
            .as_str()
            .bytes()
            .chain([0])
            .chain(source.bytes())
            .chain([0xff])
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::RelSourceKind;

    #[test]
    fn source_hash_is_deterministic_and_sensitive_to_content() {
        let id = SourceId::physical(RelSourceKind::Module, "A").unwrap();
        let first = stable_source_hash(std::iter::once((&id, "one")));
        let same = stable_source_hash(std::iter::once((&id, "one")));
        let other = stable_source_hash(std::iter::once((&id, "two")));
        assert_eq!(first, same);
        assert_ne!(first, other);
    }
}
