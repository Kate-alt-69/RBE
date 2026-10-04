use std::collections::{BTreeMap, BTreeSet, VecDeque};

use core_lib::CtiNodeKind;

use crate::ast::ImportTarget;
use crate::rel_symbol_discovery::LinkedRelSymbolSpec;
use crate::runtime_image::{RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage};
use crate::source_registry::{RelSourceKind, SourceId};
use crate::SymbolId;

use super::{
    canonical, err, ContainerTaskBoundaryHint, ContainerTaskCapabilitySlot,
    ContainerTaskDiscoveryError,
};

pub(super) fn reachable_symbols(image: &RuntimeImage, root: &SymbolId) -> BTreeSet<SymbolId> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::from([root.clone()]);
    while let Some(symbol) = queue.pop_front() {
        if !reachable.insert(symbol.clone()) {
            continue;
        }
        for dependency in image.dependency_graph.dependencies(&symbol) {
            if !reachable.contains(dependency) {
                queue.push_back(dependency.clone());
            }
        }
    }
    reachable
}

pub(super) fn required_oid_symbols(
    image: &RuntimeImage,
    reachable: &BTreeSet<SymbolId>,
    linked: &BTreeMap<String, &LinkedRelSymbolSpec>,
) -> BTreeSet<String> {
    let mut required = reachable
        .iter()
        .filter_map(|symbol| {
            let manifest = image.source(&symbol.source)?;
            let id = linked_id(manifest.kind, &manifest.logical_name, &symbol.name)?;
            linked.contains_key(&id).then_some(id)
        })
        .collect::<BTreeSet<_>>();

    // Linked REL owns OID-only edges that the general language symbol graph does
    // not need to expose (for example Service class-method descriptors). Carry
    // that exact closure forward so Container never has to rediscover it.
    let mut queue = VecDeque::from_iter(required.iter().cloned());
    while let Some(id) = queue.pop_front() {
        let Some(symbol) = linked.get(&id) else {
            continue;
        };
        for dependency in &symbol.required_symbols {
            if required.insert(dependency.clone()) {
                queue.push_back(dependency.clone());
            }
        }
    }
    required
}

pub(super) fn capability_slots(
    image: &RuntimeImage,
    sources: &BTreeSet<SourceId>,
) -> Result<Vec<ContainerTaskCapabilitySlot>, ContainerTaskDiscoveryError> {
    let mut requirements = BTreeMap::<RuntimeCapabilityRequirement, BTreeSet<SourceId>>::new();
    for source in sources {
        if let Some(found) = image.capability_requirements(source) {
            for requirement in found {
                requirements
                    .entry(requirement.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
    }
    if requirements.len() > u16::MAX as usize {
        return Err(err("Container Task capability-slot space exhausted"));
    }
    Ok(requirements
        .into_iter()
        .enumerate()
        .map(|(index, (requirement, sources))| {
            let node_kind = if matches!(&requirement, RuntimeCapabilityRequirement::Service { .. })
            {
                CtiNodeKind::ServiceCall
            } else {
                CtiNodeKind::CapabilityCall
            };
            ContainerTaskCapabilitySlot {
                slot: index as u16,
                requirement,
                node_kind,
                sources,
            }
        })
        .collect())
}

pub(super) fn boundary_hints(
    image: &RuntimeImage,
    sources: &BTreeSet<SourceId>,
    slots: &[ContainerTaskCapabilitySlot],
) -> Vec<ContainerTaskBoundaryHint> {
    let mut out = BTreeSet::new();
    for slot in slots {
        let label = match &slot.requirement {
            RuntimeCapabilityRequirement::PublicHttp { operation } => {
                format!("public-http.{operation}")
            }
            RuntimeCapabilityRequirement::Storage { owner, operation } => {
                format!("storage:{owner}.{operation}")
            }
            RuntimeCapabilityRequirement::Video { owner, operation } => {
                format!("video:{owner}.{operation}")
            }
            RuntimeCapabilityRequirement::Service { service, operation } => {
                format!("{service}.{operation}")
            }
        };
        for source in &slot.sources {
            out.insert(ContainerTaskBoundaryHint {
                node_kind_code: slot.node_kind as u16,
                source: source.clone(),
                label: label.clone(),
            });
        }
    }
    for source in sources {
        if uses_quickdb(image, source) {
            out.insert(ContainerTaskBoundaryHint {
                node_kind_code: CtiNodeKind::QuickDb as u16,
                source: source.clone(),
                label: "quickDB".into(),
            });
        }
    }
    out.into_iter().collect()
}

fn uses_quickdb(image: &RuntimeImage, source: &SourceId) -> bool {
    let Some(executable) = image.executable(source) else {
        return false;
    };
    let imports = match executable {
        RuntimeExecutable::Route(file) => &file.imports,
        RuntimeExecutable::Module(file) => &file.imports,
        RuntimeExecutable::Service(file) => &file.imports,
        RuntimeExecutable::Field(file) => &file.imports,
        RuntimeExecutable::Server(file) => &file.imports,
    };
    imports.iter().any(import_is_quickdb)
}

fn import_is_quickdb(import: &ImportTarget) -> bool {
    match import {
        ImportTarget::Builtin(name) => name == "quickDB",
        ImportTarget::BuiltinFunction { module, .. }
        | ImportTarget::BuiltinSubLibrary { module, .. } => module == "quickDB",
        ImportTarget::Aliased { target, .. } => import_is_quickdb(target),
        _ => false,
    }
}

fn linked_id(kind: RelSourceKind, logical: &str, symbol: &str) -> Option<String> {
    // HTTP Route handlers are Task roots, not generic callable Route-export OIDs.
    if kind == RelSourceKind::Route && symbol.starts_with("Route.") {
        return None;
    }
    let prefix = match kind {
        RelSourceKind::Route => "route",
        RelSourceKind::Module => "module",
        RelSourceKind::Service => "service",
        RelSourceKind::Field | RelSourceKind::Server => return None,
    };
    canonical(&[prefix, logical, symbol]).ok()
}
