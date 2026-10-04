//! Compiler-owned Container Task discovery for CTI Phase 2.
//!
//! This module emits workload metadata only. It does not assemble `.bin` files,
//! execute Tasks, or mutate the OID index. RELC remains the only authority for
//! source meaning and later OID reconciliation remains the numeric-slot authority.

mod graph;
mod semantic;
mod source_map;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use core_lib::{CtiNodeKind, TaskEventDictionary, CONTAINER_CAPABILITY_ABI_VERSION};

use crate::rel_symbol_discovery::{LinkedRelDiscovery, LinkedRelSymbolSpec};
use crate::relc::PhysicalRelSource;
use crate::runtime_image::{RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage};
use crate::source_registry::SourceId;
use crate::SymbolId;

use graph::{boundary_hints, capability_slots, reachable_symbols, required_oid_symbols};
use semantic::{symbol_hashes, task_semantic_hash};
use source_map::{build_log_dictionary, build_source_map, source_provenance_hashes, source_texts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerTaskKind {
    RouteMethod,
    ServiceExport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskOidRequest {
    pub canonical_id: String,
    pub semantic_sha256: String,
    pub required_symbols: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskNode {
    pub id: u32,
    pub kind: CtiNodeKind,
    pub dependencies: BTreeSet<u32>,
    pub success_edge: Option<u32>,
    pub failure_edge: Option<u32>,
    pub timeout_edge: Option<u32>,
    pub cancelled_edge: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskGraph {
    pub entry_node: u32,
    pub nodes: Vec<ContainerTaskNode>,
}

impl ContainerTaskGraph {
    /// The first compiler shape is deliberately conservative. Capability/service/
    /// QuickDB crossings are emitted separately as compiler-owned boundary hints;
    /// later code-section lowering may split this local block only when it can
    /// preserve exact control/data flow. CC must never guess those splits.
    fn conservative() -> Self {
        Self {
            entry_node: 0,
            nodes: vec![
                ContainerTaskNode {
                    id: 0,
                    kind: CtiNodeKind::WasmBlock,
                    dependencies: BTreeSet::new(),
                    success_edge: Some(1),
                    failure_edge: Some(2),
                    timeout_edge: Some(2),
                    cancelled_edge: Some(2),
                },
                ContainerTaskNode {
                    id: 1,
                    kind: CtiNodeKind::Return,
                    dependencies: BTreeSet::from([0]),
                    success_edge: None,
                    failure_edge: None,
                    timeout_edge: None,
                    cancelled_edge: None,
                },
                ContainerTaskNode {
                    id: 2,
                    kind: CtiNodeKind::Fail,
                    dependencies: BTreeSet::new(),
                    success_edge: None,
                    failure_edge: None,
                    timeout_edge: None,
                    cancelled_edge: None,
                },
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskCapabilitySlot {
    pub slot: u16,
    pub requirement: RuntimeCapabilityRequirement,
    pub node_kind: CtiNodeKind,
    pub sources: BTreeSet<SourceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContainerTaskBoundaryHint {
    pub node_kind_code: u16,
    pub source: SourceId,
    pub label: String,
}

impl ContainerTaskBoundaryHint {
    pub fn node_kind(&self) -> CtiNodeKind {
        CtiNodeKind::from_code(self.node_kind_code)
            .expect("compiler emitted only frozen CTI v1 node kinds")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskSourceFile {
    pub file_id: u32,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskSourceSite {
    pub site_id: u32,
    pub source: SourceId,
    pub symbol: String,
    pub file_id: u32,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskCodeRef {
    pub artifact_sha256: Option<String>,
    pub interpreter_fallback: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ContainerTaskPlan {
    pub kind: ContainerTaskKind,
    pub canonical_id: String,
    /// Phase 3/index reconciliation fills this. Phase 2 never allocates OIDs.
    pub task_oid: Option<u16>,
    pub display_name: String,
    pub source: SourceId,
    pub root_symbol: SymbolId,
    pub capability_abi: u16,
    pub graph: ContainerTaskGraph,
    pub capability_slots: Vec<ContainerTaskCapabilitySlot>,
    pub boundary_hints: Vec<ContainerTaskBoundaryHint>,
    pub required_oid_symbols: BTreeSet<String>,
    /// Reachable symbol-level semantic hashes drive selective invalidation.
    pub symbol_sha256s: BTreeMap<SymbolId, String>,
    /// Whole-source hashes are provenance/debug metadata only; they intentionally
    /// do not drive the Task semantic hash.
    pub source_sha256s: BTreeMap<SourceId, String>,
    pub code: ContainerTaskCodeRef,
    pub log_events: TaskEventDictionary,
    pub source_files: Vec<ContainerTaskSourceFile>,
    pub source_map: Vec<ContainerTaskSourceSite>,
    /// Every source-map site is a legal compact runtime error site in CTI v1.
    pub error_sites: Vec<u32>,
    pub semantic_sha256: String,
}

impl ContainerTaskPlan {
    pub fn oid_request(&self) -> ContainerTaskOidRequest {
        ContainerTaskOidRequest {
            canonical_id: self.canonical_id.clone(),
            semantic_sha256: self.semantic_sha256.clone(),
            required_symbols: self.required_oid_symbols.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContainerTaskDiscovery {
    pub tasks: Vec<ContainerTaskPlan>,
}

impl ContainerTaskDiscovery {
    pub fn oid_requests(&self) -> Vec<ContainerTaskOidRequest> {
        self.tasks
            .iter()
            .map(ContainerTaskPlan::oid_request)
            .collect()
    }

    /// Attach already-reconciled numeric slots. This method never allocates.
    pub fn bind_existing_oids(&mut self, bindings: &BTreeMap<String, u16>) {
        for task in &mut self.tasks {
            task.task_oid = bindings.get(&task.canonical_id).copied();
        }
    }

    pub fn all_oids_bound(&self) -> bool {
        self.tasks.iter().all(|task| task.task_oid.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskDiscoveryError(pub String);

impl fmt::Display for ContainerTaskDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContainerTaskDiscoveryError {}

pub(super) fn err(message: impl Into<String>) -> ContainerTaskDiscoveryError {
    ContainerTaskDiscoveryError(message.into())
}

pub fn discover_container_tasks(
    image: &RuntimeImage,
    linked_rel: &LinkedRelDiscovery,
    raw_server_source: &str,
    physical_sources: &[PhysicalRelSource],
) -> Result<ContainerTaskDiscovery, ContainerTaskDiscoveryError> {
    let texts = source_texts(raw_server_source, physical_sources)?;
    let linked = linked_rel
        .symbols
        .iter()
        .map(|symbol| (symbol.canonical_id.clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut tasks = Vec::new();

    for source_id in &image.routes {
        let manifest = image
            .source(source_id)
            .ok_or_else(|| err(format!("missing Runtime Image source {source_id}")))?;
        let RuntimeExecutable::Route(route) = image
            .executable(source_id)
            .ok_or_else(|| err(format!("missing executable {source_id}")))?
        else {
            return Err(err(format!("{source_id} is not a Route executable")));
        };
        for method in &route.methods {
            let root = SymbolId::new(source_id.clone(), format!("Route.{}", method.verb));
            let canonical_id = canonical(&["task", "route", &manifest.logical_name, &method.verb])?;
            let artifact = image
                .route_wasm_artifact(source_id)
                .filter(|artifact| artifact.verb.eq_ignore_ascii_case(&method.verb));
            tasks.push(build_task(
                ContainerTaskKind::RouteMethod,
                canonical_id,
                format!(
                    "{} {}",
                    method.verb.to_ascii_uppercase(),
                    manifest
                        .route_path
                        .as_deref()
                        .unwrap_or(&manifest.logical_name)
                ),
                source_id,
                root,
                ContainerTaskCodeRef {
                    artifact_sha256: artifact.map(|artifact| artifact.sha256.clone()),
                    interpreter_fallback: image.route_wasm_fallback(source_id).map(str::to_owned),
                },
                image,
                &linked,
                &texts,
            )?);
        }
    }

    for source_id in &image.services {
        let manifest = image
            .source(source_id)
            .ok_or_else(|| err(format!("missing Runtime Image source {source_id}")))?;
        let RuntimeExecutable::Service(service) = image
            .executable(source_id)
            .ok_or_else(|| err(format!("missing executable {source_id}")))?
        else {
            return Err(err(format!("{source_id} is not a Service executable")));
        };
        for export in &service.exports {
            tasks.push(build_task(
                ContainerTaskKind::ServiceExport,
                canonical(&["task", "service", &manifest.logical_name, export])?,
                format!("{}.{}", manifest.logical_name, export),
                source_id,
                SymbolId::new(source_id.clone(), export.clone()),
                ContainerTaskCodeRef {
                    artifact_sha256: None,
                    interpreter_fallback: None,
                },
                image,
                &linked,
                &texts,
            )?);
        }
    }

    tasks.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
    if tasks
        .windows(2)
        .any(|pair| pair[0].canonical_id == pair[1].canonical_id)
    {
        return Err(err("duplicate canonical Container Task identity"));
    }
    Ok(ContainerTaskDiscovery { tasks })
}

#[allow(clippy::too_many_arguments)]
fn build_task(
    kind: ContainerTaskKind,
    canonical_id: String,
    display_name: String,
    source: &SourceId,
    root_symbol: SymbolId,
    code: ContainerTaskCodeRef,
    image: &RuntimeImage,
    linked: &BTreeMap<String, &LinkedRelSymbolSpec>,
    texts: &source_map::SourceTextCatalog,
) -> Result<ContainerTaskPlan, ContainerTaskDiscoveryError> {
    let reachable = reachable_symbols(image, &root_symbol);
    let reachable_sources = reachable
        .iter()
        .map(|symbol| symbol.source.clone())
        .collect::<BTreeSet<_>>();
    let required_oid_symbols = required_oid_symbols(image, &reachable, linked);
    let symbol_sha256s = symbol_hashes(image, &reachable)?;
    let source_sha256s = source_provenance_hashes(image, &reachable_sources, texts)?;
    let capability_slots = capability_slots(image, &reachable_sources)?;
    let boundary_hints = boundary_hints(image, &reachable_sources, &capability_slots);
    let (source_files, source_map) = build_source_map(image, &reachable, texts)?;
    let log_events = build_log_dictionary(&canonical_id, &source_map, &boundary_hints)?;
    let graph = ContainerTaskGraph::conservative();
    let semantic_sha256 = task_semantic_hash(
        kind,
        &canonical_id,
        source,
        &root_symbol,
        &graph,
        &capability_slots,
        &boundary_hints,
        &required_oid_symbols,
        &symbol_sha256s,
        &code,
        &log_events,
        &source_files,
        &source_map,
        linked,
    );

    Ok(ContainerTaskPlan {
        kind,
        canonical_id,
        task_oid: None,
        display_name,
        source: source.clone(),
        root_symbol,
        capability_abi: CONTAINER_CAPABILITY_ABI_VERSION,
        graph,
        capability_slots,
        boundary_hints,
        required_oid_symbols,
        symbol_sha256s,
        source_sha256s,
        code,
        log_events,
        error_sites: source_map.iter().map(|site| site.site_id).collect(),
        source_files,
        source_map,
        semantic_sha256,
    })
}

pub(super) fn canonical(parts: &[&str]) -> Result<String, ContainerTaskDiscoveryError> {
    let mut out = String::new();
    for (index, raw) in parts.iter().enumerate() {
        let mut part = String::new();
        let mut previous_separator = false;
        for byte in raw.trim().bytes() {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                part.push(byte as char);
                previous_separator = byte == b'_';
            } else if matches!(byte, b'/' | b'\\' | b'.' | b'-' | b':' | b' ') {
                if !previous_separator {
                    part.push('_');
                    previous_separator = true;
                }
            } else {
                return Err(err(format!("invalid CTI identity component {raw:?}")));
            }
        }
        let part = part.trim_matches('_');
        if part.is_empty() {
            return Err(err("empty CTI identity component"));
        }
        if index != 0 {
            out.push('_');
        }
        out.push_str(part);
    }
    if out.len() > 512 {
        return Err(err("CTI canonical identity exceeds 512 bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_task_identity_is_stable() {
        assert_eq!(
            canonical(&["task", "route", "admin/users", "post"]).unwrap(),
            "task_route_admin_users_post"
        );
    }

    #[test]
    fn conservative_graph_keeps_failure_explicit() {
        let graph = ContainerTaskGraph::conservative();
        assert_eq!(graph.nodes[0].kind, CtiNodeKind::WasmBlock);
        assert_eq!(graph.nodes[0].success_edge, Some(1));
        assert_eq!(graph.nodes[0].failure_edge, Some(2));
        assert_eq!(graph.nodes[1].kind, CtiNodeKind::Return);
        assert_eq!(graph.nodes[2].kind, CtiNodeKind::Fail);
    }
}
