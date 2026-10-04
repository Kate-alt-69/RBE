//! Phase 3 lowering from compiler-owned Container Task plans to cached CTI binaries.
//!
//! RELC owns Task meaning. This module owns only the deterministic Task-OID/cache
//! transaction and section serialization consumed by Container. It deliberately
//! reuses the one project-local REL OID range rather than creating another ID
//! namespace. CTI blobs are written before the OID index is published, so a
//! failed build can leave only harmless orphan cache data, never a live Task OID
//! with no image behind it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use container_runtime_core::{
    ContainerTaskAssembler, ContainerTaskAssemblyInput, ContainerTaskIndex,
    CTI_COMPILER_ABI_VERSION,
};
use ipc_protocol::container_task_image::{
    cti_sha256, CtiSection, CtiSectionKind, CTI_DEFAULT_ARENA_BYTES, CTI_MAX_SLOTS,
};
use sha2::{Digest, Sha256};

use crate::relc_linked_image::{
    ContainerTaskDiscovery, ContainerTaskKind, ContainerTaskOidRequest, ContainerTaskPlan,
};
use crate::runtime_image::{RuntimeCapabilityRequirement, RuntimeImage};
use crate::service_oid::{OidCache, OidError, OidIndex, OidTarget, OID_REL_END, OID_REL_START};

pub const CTI_SECTION_PAYLOAD_VERSION: u16 = 1;
const TASK_OID_PREFIX: &str = "task_";
const EDGE_NONE: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskArtifact {
    pub canonical_id: String,
    pub task_oid: u16,
    pub semantic_sha256: [u8; 32],
    pub cti_sha256: [u8; 32],
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskAssemblyReport {
    pub runtime_image_sha256: [u8; 32],
    pub oid_generation: u64,
    pub oid_index_changed: bool,
    pub task_bindings: BTreeMap<String, u16>,
    pub artifacts: Vec<ContainerTaskArtifact>,
}

#[derive(Debug)]
pub enum ContainerTaskAssemblyError {
    Oid(OidError),
    InvalidRuntimeImage(String),
    InvalidTask(String),
    Cache(String),
}

impl fmt::Display for ContainerTaskAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "Container Task OID update failed: {error}"),
            Self::InvalidRuntimeImage(message) => {
                write!(formatter, "Container Task Runtime Image is invalid: {message}")
            }
            Self::InvalidTask(message) => write!(formatter, "Container Task is invalid: {message}"),
            Self::Cache(message) => write!(formatter, "Container Task cache failed: {message}"),
        }
    }
}

impl std::error::Error for ContainerTaskAssemblyError {}

impl From<OidError> for ContainerTaskAssemblyError {
    fn from(value: OidError) -> Self {
        Self::Oid(value)
    }
}

/// Reconcile Task OIDs, serialize every bound compiler Task into CTI v1, and
/// publish `.cache/compiler/container/index` plus the shared OID index.
///
/// `pinned_oids` is the same old-image liveness set used by native REL linking.
/// A pinned Task OID is never rebound in place; a new image receives a fresh
/// slot while the old execution can keep resolving its original numeric ID.
pub fn assemble_container_task_cache(
    project_root: &Path,
    image: &RuntimeImage,
    discovery: &mut ContainerTaskDiscovery,
    oid_cache: &mut OidCache,
    pinned_oids: &BTreeSet<u16>,
) -> Result<ContainerTaskAssemblyReport, ContainerTaskAssemblyError> {
    let runtime_image_sha256 = decode_sha256("Runtime Image", &image.image_id)
        .map_err(ContainerTaskAssemblyError::InvalidRuntimeImage)?;
    let requests = discovery.oid_requests();
    let (next_oid_index, task_bindings) =
        reconcile_task_oids(oid_cache.index(), &requests, pinned_oids)?;
    discovery.bind_existing_oids(&task_bindings);
    if !discovery.all_oids_bound() {
        return Err(ContainerTaskAssemblyError::InvalidTask(
            "one or more compiler Tasks did not receive an OID".into(),
        ));
    }

    let cache_root = project_root.join(".cache");
    let assembler = ContainerTaskAssembler::new(&cache_root);
    let mut container_index = assembler
        .load_index()
        .map_err(ContainerTaskAssemblyError::Cache)?;

    // Build and validate every CTI in memory before publishing either index.
    // Individual content-addressed blob writes may still leave harmless orphans
    // if the filesystem fails later, but no live OID mapping is exposed early.
    let target_id = target_id(&OidTarget::current());
    let symbol_bindings = &next_oid_index.rel_bindings;
    let mut assembled = Vec::with_capacity(discovery.tasks.len());
    for task in &discovery.tasks {
        let task_oid = task.task_oid.ok_or_else(|| {
            ContainerTaskAssemblyError::InvalidTask(format!(
                "Task {:?} is missing its reconciled OID",
                task.canonical_id
            ))
        })?;
        let semantic_sha256 = decode_sha256("Task semantic", &task.semantic_sha256)
            .map_err(ContainerTaskAssemblyError::InvalidTask)?;
        let sections = lower_sections(task, image, symbol_bindings, semantic_sha256)?;
        let item = assembler
            .assemble(ContainerTaskAssemblyInput {
                task_oid,
                capability_abi: task.capability_abi,
                runtime_image_sha256,
                task_semantic_sha256: semantic_sha256,
                entry_node: task.graph.entry_node,
                target_id,
                flags: 0,
                sections,
            })
            .map_err(ContainerTaskAssemblyError::Cache)?;
        assembled.push((task.canonical_id.clone(), semantic_sha256, item));
    }

    let desired_oids = assembled
        .iter()
        .map(|(_, _, image)| image.task_oid)
        .collect::<BTreeSet<_>>();
    let stale_oids = container_index
        .iter()
        .filter_map(|(key, _)| {
            (key.runtime_image_sha256 == runtime_image_sha256
                && !desired_oids.contains(&key.task_oid))
            .then_some(key.task_oid)
        })
        .collect::<Vec<_>>();
    for task_oid in stale_oids {
        assembler
            .invalidate(&mut container_index, runtime_image_sha256, task_oid)
            .map_err(ContainerTaskAssemblyError::Cache)?;
    }

    let mut artifacts = Vec::with_capacity(assembled.len());
    for (canonical_id, semantic_sha256, item) in &assembled {
        let commit = assembler
            .commit(&mut container_index, item)
            .map_err(ContainerTaskAssemblyError::Cache)?;
        artifacts.push(ContainerTaskArtifact {
            canonical_id: canonical_id.clone(),
            task_oid: item.task_oid,
            semantic_sha256: *semantic_sha256,
            cti_sha256: commit.cti_sha256,
            path: commit.blob_path,
        });
    }

    let oid_index_changed = oid_cache.index() != &next_oid_index;
    if oid_index_changed {
        oid_cache.replace_index(next_oid_index.clone())?;
    }

    artifacts.sort_by_key(|artifact| artifact.task_oid);
    Ok(ContainerTaskAssemblyReport {
        runtime_image_sha256,
        oid_generation: next_oid_index.generation,
        oid_index_changed,
        task_bindings,
        artifacts,
    })
}

fn reconcile_task_oids(
    index: &OidIndex,
    requests: &[ContainerTaskOidRequest],
    pinned_oids: &BTreeSet<u16>,
) -> Result<(OidIndex, BTreeMap<String, u16>), ContainerTaskAssemblyError> {
    index.validate_structure()?;
    for oid in pinned_oids {
        if !(OID_REL_START..=OID_REL_END).contains(oid) {
            return Err(ContainerTaskAssemblyError::InvalidTask(format!(
                "pinned Task/REL OID {oid} is outside {OID_REL_START}..={OID_REL_END}"
            )));
        }
    }

    let mut desired = requests
        .iter()
        .map(|request| request.canonical_id.clone())
        .collect::<Vec<_>>();
    desired.sort();
    if desired.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ContainerTaskAssemblyError::InvalidTask(
            "compiler emitted duplicate Task OID requests".into(),
        ));
    }
    if desired.iter().any(|id| !id.starts_with(TASK_OID_PREFIX)) {
        return Err(ContainerTaskAssemblyError::InvalidTask(
            "Task canonical IDs must use the reserved task_ prefix".into(),
        ));
    }

    let desired_set = desired.iter().cloned().collect::<BTreeSet<_>>();
    let mut retained_non_tasks = index
        .rel_bindings
        .iter()
        .filter(|(id, _)| !id.starts_with(TASK_OID_PREFIX))
        .map(|(id, oid)| (id.clone(), *oid))
        .collect::<BTreeMap<_, _>>();
    let mut occupied = retained_non_tasks.values().copied().collect::<BTreeSet<_>>();
    occupied.extend(pinned_oids.iter().copied());

    let mut task_bindings = BTreeMap::new();
    for id in &desired {
        let Some(&oid) = index.rel_bindings.get(id) else {
            continue;
        };
        if pinned_oids.contains(&oid) || occupied.contains(&oid) {
            continue;
        }
        occupied.insert(oid);
        task_bindings.insert(id.clone(), oid);
    }

    for id in desired {
        if task_bindings.contains_key(&id) {
            continue;
        }
        let oid = (OID_REL_START..=OID_REL_END)
            .find(|oid| !occupied.contains(oid))
            .ok_or_else(|| {
                ContainerTaskAssemblyError::InvalidTask(
                    "linked REL/Task OID range is exhausted".into(),
                )
            })?;
        occupied.insert(oid);
        task_bindings.insert(id, oid);
    }

    retained_non_tasks.extend(task_bindings.clone());
    let mut next = index.clone();
    if next.rel_bindings != retained_non_tasks {
        next.rel_bindings = retained_non_tasks;
        next.generation = next.generation.checked_add(1).ok_or_else(|| {
            ContainerTaskAssemblyError::InvalidTask("OID generation counter is exhausted".into())
        })?;
    }
    next.validate_structure()?;

    // Make stale Task removal explicit in the algorithm. This also guards a
    // future refactor that changes the map construction above.
    if next
        .rel_bindings
        .keys()
        .any(|id| id.starts_with(TASK_OID_PREFIX) && !desired_set.contains(id))
    {
        return Err(ContainerTaskAssemblyError::InvalidTask(
            "stale Task binding survived reconciliation".into(),
        ));
    }
    Ok((next, task_bindings))
}

fn lower_sections(
    task: &ContainerTaskPlan,
    image: &RuntimeImage,
    oid_bindings: &BTreeMap<String, u16>,
    semantic_sha256: [u8; 32],
) -> Result<Vec<CtiSection>, ContainerTaskAssemblyError> {
    let mut required_oids = task
        .required_oid_symbols
        .iter()
        .map(|symbol| {
            oid_bindings.get(symbol).copied().ok_or_else(|| {
                ContainerTaskAssemblyError::InvalidTask(format!(
                    "Task {:?} requires unbound OID symbol {symbol:?}",
                    task.canonical_id
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    required_oids.sort_unstable();
    required_oids.dedup();

    let task_oid = task.task_oid.ok_or_else(|| {
        ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} is not OID-bound",
            task.canonical_id
        ))
    })?;

    let mut sections = vec![
        CtiSection::required(
            CtiSectionKind::OidTable,
            encode_oid_table(task_oid, &required_oids)?,
        ),
        CtiSection::required(CtiSectionKind::Graph, encode_graph(task)?),
        CtiSection::required(CtiSectionKind::SlotLayout, encode_slot_layout()),
        CtiSection::required(CtiSectionKind::Capabilities, encode_capabilities(task)?),
        CtiSection::required(CtiSectionKind::Code, encode_code(task, image)?),
        CtiSection::required(CtiSectionKind::ResourcePolicy, encode_resource_policy()),
        CtiSection::required(CtiSectionKind::LogEvents, encode_log_events(task)?),
        CtiSection::required(CtiSectionKind::SourceFiles, encode_source_files(task)?),
        CtiSection::required(CtiSectionKind::SourceMap, encode_source_map(task)?),
        CtiSection::required(CtiSectionKind::ErrorSites, encode_error_sites(task)?),
        CtiSection::required(CtiSectionKind::Dependencies, encode_dependencies(task)?),
        CtiSection::required(
            CtiSectionKind::Checksums,
            encode_checksums(task, image, semantic_sha256)?,
        ),
    ];
    sections.push(CtiSection::optional(
        CtiSectionKind::DebugNames,
        encode_debug_names(task)?,
    ));
    Ok(sections)
}

fn encode_oid_table(task_oid: u16, required_oids: &[u16]) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, task_oid);
    push_u16(
        &mut out,
        u16::try_from(required_oids.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many required Task OIDs".into())
        })?,
    );
    push_u16(&mut out, 0);
    for oid in required_oids {
        push_u16(&mut out, *oid);
    }
    Ok(out)
}

fn encode_graph(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut nodes = task.graph.nodes.clone();
    nodes.sort_by_key(|node| node.id);
    if nodes.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} contains duplicate graph node IDs",
            task.canonical_id
        )));
    }
    if !nodes.iter().any(|node| node.id == task.graph.entry_node) {
        return Err(ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} graph entry node {} is missing",
            task.canonical_id, task.graph.entry_node
        )));
    }

    let valid = nodes.iter().map(|node| node.id).collect::<BTreeSet<_>>();
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(&mut out, task.graph.entry_node);
    push_u32(
        &mut out,
        u32::try_from(nodes.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many Task graph nodes".into())
        })?,
    );
    for node in nodes {
        for dependency in &node.dependencies {
            if !valid.contains(dependency) {
                return Err(ContainerTaskAssemblyError::InvalidTask(format!(
                    "Task {:?} node {} depends on missing node {dependency}",
                    task.canonical_id, node.id
                )));
            }
        }
        for edge in [
            node.success_edge,
            node.failure_edge,
            node.timeout_edge,
            node.cancelled_edge,
        ]
        .into_iter()
        .flatten()
        {
            if !valid.contains(&edge) {
                return Err(ContainerTaskAssemblyError::InvalidTask(format!(
                    "Task {:?} node {} points at missing node {edge}",
                    task.canonical_id, node.id
                )));
            }
        }
        push_u32(&mut out, node.id);
        push_u16(&mut out, node.kind as u16);
        push_u16(
            &mut out,
            u16::try_from(node.dependencies.len()).map_err(|_| {
                ContainerTaskAssemblyError::InvalidTask("too many node dependencies".into())
            })?,
        );
        push_u32(&mut out, edge(node.success_edge));
        push_u32(&mut out, edge(node.failure_edge));
        push_u32(&mut out, edge(node.timeout_edge));
        push_u32(&mut out, edge(node.cancelled_edge));
        for dependency in node.dependencies {
            push_u32(&mut out, dependency);
        }
    }
    Ok(out)
}

fn encode_slot_layout() -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(&mut out, CTI_DEFAULT_ARENA_BYTES as u32);
    push_u16(&mut out, CTI_MAX_SLOTS as u16);
    push_u16(&mut out, 0);
    out
}

fn encode_capabilities(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut slots = task.capability_slots.clone();
    slots.sort_by_key(|slot| slot.slot);
    if slots.windows(2).any(|pair| pair[0].slot == pair[1].slot) {
        return Err(ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} repeats a capability slot",
            task.canonical_id
        )));
    }

    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(
        &mut out,
        u16::try_from(slots.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many capability slots".into())
        })?,
    );
    for slot in slots {
        push_u16(&mut out, slot.slot);
        push_u16(&mut out, slot.node_kind as u16);
        match &slot.requirement {
            RuntimeCapabilityRequirement::PublicHttp { operation } => {
                push_u16(&mut out, 1);
                push_string(&mut out, "public-http")?;
                push_string(&mut out, operation)?;
            }
            RuntimeCapabilityRequirement::Storage { owner, operation } => {
                push_u16(&mut out, 2);
                push_string(&mut out, owner)?;
                push_string(&mut out, operation)?;
            }
            RuntimeCapabilityRequirement::Video { owner, operation } => {
                push_u16(&mut out, 3);
                push_string(&mut out, owner)?;
                push_string(&mut out, operation)?;
            }
            RuntimeCapabilityRequirement::Service { service, operation } => {
                push_u16(&mut out, 4);
                push_string(&mut out, service)?;
                push_string(&mut out, operation)?;
            }
        }
        push_u16(
            &mut out,
            u16::try_from(slot.sources.len()).map_err(|_| {
                ContainerTaskAssemblyError::InvalidTask("too many capability source owners".into())
            })?,
        );
        for source in slot.sources {
            push_string(&mut out, source.as_str())?;
        }
    }
    Ok(out)
}

fn encode_code(
    task: &ContainerTaskPlan,
    image: &RuntimeImage,
) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(
        &mut out,
        match task.kind {
            ContainerTaskKind::RouteMethod => 1,
            ContainerTaskKind::ServiceExport => 2,
        },
    );

    match &task.code.artifact_sha256 {
        Some(expected) => {
            let expected_bytes = decode_sha256("Task code artifact", expected)
                .map_err(ContainerTaskAssemblyError::InvalidTask)?;
            let artifact = image.route_wasm_artifact(&task.source).ok_or_else(|| {
                ContainerTaskAssemblyError::InvalidTask(format!(
                    "Task {:?} references missing Route WASM artifact {expected}",
                    task.canonical_id
                ))
            })?;
            if artifact.sha256 != *expected || cti_sha256(&artifact.bytes) != expected_bytes {
                return Err(ContainerTaskAssemblyError::InvalidTask(format!(
                    "Task {:?} Route WASM artifact identity mismatch",
                    task.canonical_id
                )));
            }
            out.push(1);
            out.extend_from_slice(&[0; 3]);
            out.extend_from_slice(&expected_bytes);
            push_bytes(&mut out, &artifact.bytes)?;
        }
        None => {
            out.push(0);
            out.extend_from_slice(&[0; 3]);
            out.extend_from_slice(&[0; 32]);
            push_bytes(&mut out, &[])?;
        }
    }
    push_optional_string(&mut out, task.code.interpreter_fallback.as_deref())?;
    Ok(out)
}

fn encode_resource_policy() -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(&mut out, CTI_DEFAULT_ARENA_BYTES as u32);
    push_u32(&mut out, 0);
    push_u64(&mut out, 0);
    out
}

fn encode_log_events(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let entries = task.log_events.entries();
    let mut out = Vec::new();
    push_u16(&mut out, task.log_events.abi_version);
    push_u16(
        &mut out,
        u16::try_from(entries.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many Task log events".into())
        })?,
    );
    for event in entries {
        push_u16(&mut out, event.event_id);
        out.push(event.class as u8);
        out.push(event.level as u8);
        push_string(&mut out, &event.symbol)?;
        push_string(&mut out, &event.template)?;
    }
    Ok(out)
}

fn encode_source_files(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut files = task.source_files.clone();
    files.sort_by_key(|file| file.file_id);
    if files.windows(2).any(|pair| pair[0].file_id == pair[1].file_id) {
        return Err(ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} repeats a source file ID",
            task.canonical_id
        )));
    }
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(
        &mut out,
        u32::try_from(files.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many source files".into())
        })?,
    );
    for file in files {
        push_u32(&mut out, file.file_id);
        push_string(&mut out, &file.path)?;
    }
    Ok(out)
}

fn encode_source_map(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut sites = task.source_map.clone();
    sites.sort_by_key(|site| site.site_id);
    if sites.windows(2).any(|pair| pair[0].site_id == pair[1].site_id) {
        return Err(ContainerTaskAssemblyError::InvalidTask(format!(
            "Task {:?} repeats a source-map site ID",
            task.canonical_id
        )));
    }
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(
        &mut out,
        u32::try_from(sites.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many source-map sites".into())
        })?,
    );
    for site in sites {
        push_u32(&mut out, site.site_id);
        push_u32(&mut out, site.file_id);
        push_u32(&mut out, site.line);
        push_u32(&mut out, site.column);
        push_string(&mut out, site.source.as_str())?;
        push_string(&mut out, &site.symbol)?;
    }
    Ok(out)
}

fn encode_error_sites(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut sites = task.error_sites.clone();
    sites.sort_unstable();
    sites.dedup();
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(
        &mut out,
        u32::try_from(sites.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many error sites".into())
        })?,
    );
    for site in sites {
        push_u32(&mut out, site);
    }
    Ok(out)
}

fn encode_dependencies(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_u32(
        &mut out,
        u32::try_from(task.symbol_sha256s.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many semantic symbol hashes".into())
        })?,
    );
    for (symbol, digest) in &task.symbol_sha256s {
        push_string(&mut out, symbol.source.as_str())?;
        push_string(&mut out, &symbol.name)?;
        out.extend_from_slice(
            &decode_sha256("symbol semantic", digest)
                .map_err(ContainerTaskAssemblyError::InvalidTask)?,
        );
    }
    push_u32(
        &mut out,
        u32::try_from(task.source_sha256s.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("too many source provenance hashes".into())
        })?,
    );
    for (source, digest) in &task.source_sha256s {
        push_string(&mut out, source.as_str())?;
        out.extend_from_slice(
            &decode_sha256("source provenance", digest)
                .map_err(ContainerTaskAssemblyError::InvalidTask)?,
        );
    }
    Ok(out)
}

fn encode_checksums(
    task: &ContainerTaskPlan,
    image: &RuntimeImage,
    semantic_sha256: [u8; 32],
) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let runtime_image_sha256 = decode_sha256("Runtime Image", &image.image_id)
        .map_err(ContainerTaskAssemblyError::InvalidRuntimeImage)?;
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, CTI_COMPILER_ABI_VERSION);
    out.extend_from_slice(&runtime_image_sha256);
    out.extend_from_slice(&semantic_sha256);
    match &task.code.artifact_sha256 {
        Some(digest) => out.extend_from_slice(
            &decode_sha256("Task code artifact", digest)
                .map_err(ContainerTaskAssemblyError::InvalidTask)?,
        ),
        None => out.extend_from_slice(&[0; 32]),
    }
    Ok(out)
}

fn encode_debug_names(task: &ContainerTaskPlan) -> Result<Vec<u8>, ContainerTaskAssemblyError> {
    let mut out = Vec::new();
    push_u16(&mut out, CTI_SECTION_PAYLOAD_VERSION);
    push_u16(&mut out, 0);
    push_string(&mut out, &task.canonical_id)?;
    push_string(&mut out, &task.display_name)?;
    push_string(&mut out, task.source.as_str())?;
    push_string(&mut out, task.root_symbol.source.as_str())?;
    push_string(&mut out, &task.root_symbol.name)?;
    Ok(out)
}

fn target_id(target: &OidTarget) -> u32 {
    let digest = Sha256::digest(target.label().as_bytes());
    u32::from_be_bytes(digest[..4].try_into().expect("SHA-256 prefix is four bytes"))
}

fn edge(value: Option<u32>) -> u32 {
    value.unwrap_or(EDGE_NONE)
}

fn decode_sha256(label: &str, value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{label} SHA-256 is not 64 hexadecimal characters"));
    }
    let bytes = hex::decode(value).map_err(|error| format!("decode {label} SHA-256: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("{label} SHA-256 does not contain 32 bytes"))
}

fn push_string(out: &mut Vec<u8>, value: &str) -> Result<(), ContainerTaskAssemblyError> {
    push_bytes(out, value.as_bytes())
}

fn push_optional_string(
    out: &mut Vec<u8>,
    value: Option<&str>,
) -> Result<(), ContainerTaskAssemblyError> {
    match value {
        Some(value) => {
            out.push(1);
            push_string(out, value)?;
        }
        None => {
            out.push(0);
            push_u32(out, 0);
        }
    }
    Ok(())
}

fn push_bytes(out: &mut Vec<u8>, value: &[u8]) -> Result<(), ContainerTaskAssemblyError> {
    push_u32(
        out,
        u32::try_from(value.len()).map_err(|_| {
            ContainerTaskAssemblyError::InvalidTask("CTI section value exceeds u32".into())
        })?,
    );
    out.extend_from_slice(value);
    Ok(())
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_oid::{OidIndex, OID_REL_START};

    fn request(id: &str) -> ContainerTaskOidRequest {
        ContainerTaskOidRequest {
            canonical_id: id.into(),
            semantic_sha256: "11".repeat(32),
            required_symbols: BTreeSet::new(),
        }
    }

    #[test]
    fn task_oids_share_rel_range_without_renumbering_symbols() {
        let mut index = OidIndex::fresh();
        index
            .rel_bindings
            .insert("module_users_find".into(), OID_REL_START);
        let (next, tasks) = reconcile_task_oids(
            &index,
            &[request("task_route_login_post")],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(next.rel_bindings["module_users_find"], OID_REL_START);
        assert_eq!(tasks["task_route_login_post"], OID_REL_START + 1);
    }

    #[test]
    fn unchanged_task_keeps_oid_but_live_pin_forces_fresh_slot() {
        let mut index = OidIndex::fresh();
        index
            .rel_bindings
            .insert("task_route_login_post".into(), OID_REL_START);
        let (_, stable) = reconcile_task_oids(
            &index,
            &[request("task_route_login_post")],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(stable["task_route_login_post"], OID_REL_START);

        let (_, rebound) = reconcile_task_oids(
            &index,
            &[request("task_route_login_post")],
            &BTreeSet::from([OID_REL_START]),
        )
        .unwrap();
        assert_eq!(rebound["task_route_login_post"], OID_REL_START + 1);
    }

    #[test]
    fn removed_tasks_are_removed_without_touching_non_task_bindings() {
        let mut index = OidIndex::fresh();
        index
            .rel_bindings
            .insert("module_users_find".into(), OID_REL_START);
        index
            .rel_bindings
            .insert("task_route_old_get".into(), OID_REL_START + 1);
        let (next, tasks) = reconcile_task_oids(&index, &[], &BTreeSet::new()).unwrap();
        assert!(tasks.is_empty());
        assert_eq!(next.rel_bindings.len(), 1);
        assert_eq!(next.rel_bindings["module_users_find"], OID_REL_START);
    }

    #[test]
    fn malformed_hash_is_rejected() {
        assert!(decode_sha256("test", "xyz").is_err());
        assert_eq!(decode_sha256("test", &"ab".repeat(32)).unwrap(), [0xab; 32]);
    }
}