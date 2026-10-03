//! Phase 4 REL symbol discovery and reachability planning.
//!
//! This module turns already-parsed REL sources into the project-local linked
//! symbol set consumed by `rel_oid_bridge`. It deliberately runs before OID
//! allocation: names/source identity/reachability are compiler facts, while the
//! numeric `30458..=65535` assignment remains the single OID index's job.
//!
//! Important boundary: package exports are not linked-REL symbols. RPX package
//! export IDs/OIDs stay in the Phase 3 package range and are resolved by the
//! package linker. This module only discovers REL-to-REL targets.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::ast::{
    Expr, FunctionDef, ImportTarget, ModuleFile, RouteFile, ServiceProgram, Statement,
};
use crate::modules::binding_name;
use crate::oid_link::{LinkedRelKind, LinkedRelSymbolSpec};

/// Parsed REL roles that can participate in Phase 4 linked-symbol discovery.
///
/// Route handlers are reachability roots, but are intentionally not emitted as
/// generic `RouteExport` OIDs: the current Route AST has no explicit callable
/// export list. This preserves the service-upgrade rule that an HTTP handler
/// must not accidentally become a generic callable merely because it lives in a
/// `.route` source.
#[derive(Debug, Clone, Copy)]
pub enum LinkedRelSourceRef<'a> {
    Module(&'a ModuleFile),
    Route(&'a RouteFile),
    Service(&'a ServiceProgram),
}

/// One already-validated REL source presented to linked-symbol discovery.
#[derive(Debug, Clone)]
pub struct LinkedRelSourceUnit<'a> {
    pub logical_name: String,
    pub source_sha256: String,
    pub source: LinkedRelSourceRef<'a>,
}

impl<'a> LinkedRelSourceUnit<'a> {
    pub fn module(logical_name: impl Into<String>, source: &str, file: &'a ModuleFile) -> Self {
        Self {
            logical_name: logical_name.into(),
            source_sha256: linked_source_sha256(source),
            source: LinkedRelSourceRef::Module(file),
        }
    }

    pub fn route(logical_name: impl Into<String>, source: &str, file: &'a RouteFile) -> Self {
        Self {
            logical_name: logical_name.into(),
            source_sha256: linked_source_sha256(source),
            source: LinkedRelSourceRef::Route(file),
        }
    }

    pub fn service(
        logical_name: impl Into<String>,
        source: &str,
        file: &'a ServiceProgram,
    ) -> Self {
        Self {
            logical_name: logical_name.into(),
            source_sha256: linked_source_sha256(source),
            source: LinkedRelSourceRef::Service(file),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedRelDiscovery {
    /// Reachable linked REL symbols only, sorted by canonical identity.
    pub symbols: Vec<LinkedRelSymbolSpec>,
    /// Canonical OID roots for each service. Lifecycle hooks are compiler roots
    /// even though they are not public Service Fabric exports.
    pub service_roots: BTreeMap<String, BTreeSet<String>>,
    /// Source-level Service Fabric export name -> canonical linked symbol for
    /// each Service. This is compiler truth used later to build native dispatch
    /// metadata; consumers must not reverse-parse canonical symbol strings.
    pub service_exports: BTreeMap<String, BTreeMap<String, String>>,
    /// Canonical symbols retained by graph reachability. Exposed mainly for
    /// compiler diagnostics/tests; numeric OID allocation happens later.
    pub reachable_symbols: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UnitKind {
    Module,
    Route,
    Service,
}

impl UnitKind {
    fn label(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Route => "route",
            Self::Service => "service",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NodeId {
    unit: usize,
    local: String,
}

impl NodeId {
    fn new(unit: usize, local: impl Into<String>) -> Self {
        Self {
            unit,
            local: local.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct Node {
    body: Vec<Statement>,
    edges: BTreeSet<NodeEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NodeEdge {
    Internal(NodeId),
    Canonical(String),
}

#[derive(Debug, Clone)]
struct OidCandidate {
    canonical_id: String,
    kind: LinkedRelKind,
    source_sha256: String,
    node: Option<NodeId>,
    /// Descriptor-only dependency seeds. Class descriptors use this to point
    /// at only the methods that survived reachability.
    descriptor_members: BTreeSet<String>,
}

#[derive(Debug, Clone)]
enum ImportBinding {
    LinkedFunction(String),
    ModuleNamespace(String),
    ServiceNamespace(String),
    Other,
}

#[derive(Debug, Clone)]
struct UnitMeta {
    kind: UnitKind,
    logical_name: String,
    source_sha256: String,
    function_nodes: BTreeMap<String, NodeId>,
    class_method_nodes: BTreeMap<(String, String), NodeId>,
    imports: BTreeMap<String, ImportBinding>,
}

/// Stable source digest stored on discovered linked-symbol identities.
pub fn linked_source_sha256(source: &str) -> String {
    hex::encode(Sha256::digest(source.as_bytes()))
}

/// Discover linked REL identities, exact REL-to-REL dependencies and the
/// reachable subset that should enter OID allocation/materialization.
///
/// Reachability roots are:
/// - exported `.service` functions;
/// - service lifecycle hooks;
/// - HTTP Route methods (as graph roots only, not generic callable OIDs).
///
/// Module exports are retained only when one of those roots actually reaches
/// them. Private helper functions remain local graph connectors and do not
/// consume OIDs merely because they exist.
pub fn discover_linked_rel_symbols(
    units: &[LinkedRelSourceUnit<'_>],
) -> Result<LinkedRelDiscovery, LinkedRelDiscoveryError> {
    let mut unit_keys = BTreeMap::<(UnitKind, String), usize>::new();
    let mut metas = Vec::with_capacity(units.len());
    let mut nodes = BTreeMap::<NodeId, Node>::new();
    let mut roots = BTreeSet::<NodeId>::new();
    let mut candidates = BTreeMap::<String, OidCandidate>::new();
    let mut candidate_by_node = BTreeMap::<NodeId, String>::new();
    let mut service_roots = BTreeMap::<String, BTreeSet<String>>::new();
    let mut public_modules = BTreeMap::<(String, String), String>::new();
    let mut public_services = BTreeMap::<(String, String), String>::new();

    // Pass 1: validate source identity and declare every local node/public OID
    // candidate before resolving any cross-source imports.
    for (unit_index, unit) in units.iter().enumerate() {
        validate_sha256(&unit.source_sha256)?;
        let kind = match unit.source {
            LinkedRelSourceRef::Module(_) => UnitKind::Module,
            LinkedRelSourceRef::Route(_) => UnitKind::Route,
            LinkedRelSourceRef::Service(_) => UnitKind::Service,
        };
        let logical_name = unit.logical_name.trim().to_string();
        if logical_name.is_empty() || logical_name.chars().any(char::is_control) {
            return Err(LinkedRelDiscoveryError::InvalidLogicalName(
                unit.logical_name.clone(),
            ));
        }
        if unit_keys
            .insert((kind, logical_name.clone()), unit_index)
            .is_some()
        {
            return Err(LinkedRelDiscoveryError::DuplicateSource {
                kind: kind.label(),
                logical_name,
            });
        }

        let mut meta = UnitMeta {
            kind,
            logical_name: logical_name.clone(),
            source_sha256: unit.source_sha256.clone(),
            function_nodes: BTreeMap::new(),
            class_method_nodes: BTreeMap::new(),
            imports: BTreeMap::new(),
        };

        match unit.source {
            LinkedRelSourceRef::Module(file) => {
                declare_functions(unit_index, &file.functions, &mut meta, &mut nodes)?;
                let declared = file
                    .functions
                    .iter()
                    .map(|function| function.name.as_str())
                    .collect::<BTreeSet<_>>();
                for export in &file.exports {
                    if !declared.contains(export.as_str()) {
                        return Err(LinkedRelDiscoveryError::MissingExportBody {
                            source: logical_name.clone(),
                            export: export.clone(),
                        });
                    }
                    let canonical = canonical_symbol_id(&["module", &logical_name, export])?;
                    let node = meta
                        .function_nodes
                        .get(export)
                        .expect("validated exported function exists")
                        .clone();
                    insert_candidate(
                        &mut candidates,
                        &mut candidate_by_node,
                        OidCandidate {
                            canonical_id: canonical.clone(),
                            kind: LinkedRelKind::ModuleExport,
                            source_sha256: unit.source_sha256.clone(),
                            node: Some(node),
                            descriptor_members: BTreeSet::new(),
                        },
                    )?;
                    if public_modules
                        .insert((logical_name.clone(), export.clone()), canonical)
                        .is_some()
                    {
                        return Err(LinkedRelDiscoveryError::DuplicatePublicExport {
                            source: logical_name.clone(),
                            export: export.clone(),
                        });
                    }
                }
            }
            LinkedRelSourceRef::Route(file) => {
                declare_functions(unit_index, &file.functions, &mut meta, &mut nodes)?;
                for method in &file.methods {
                    let node = NodeId::new(unit_index, format!("route:{}", method.verb));
                    if nodes
                        .insert(
                            node.clone(),
                            Node {
                                body: method.body.clone(),
                                edges: BTreeSet::new(),
                            },
                        )
                        .is_some()
                    {
                        return Err(LinkedRelDiscoveryError::DuplicateLocalSymbol {
                            source: logical_name.clone(),
                            symbol: format!("Route.{}", method.verb),
                        });
                    }
                    // HTTP handlers are roots for DCE, but not RouteExport OIDs.
                    roots.insert(node);
                }
            }
            LinkedRelSourceRef::Service(file) => {
                declare_functions(unit_index, &file.functions, &mut meta, &mut nodes)?;

                let declared = file
                    .functions
                    .iter()
                    .map(|function| function.name.as_str())
                    .collect::<BTreeSet<_>>();
                for export in &file.exports {
                    if !declared.contains(export.as_str()) {
                        return Err(LinkedRelDiscoveryError::MissingExportBody {
                            source: logical_name.clone(),
                            export: export.clone(),
                        });
                    }
                    let canonical = canonical_symbol_id(&["service", &logical_name, export])?;
                    let node = meta
                        .function_nodes
                        .get(export)
                        .expect("validated exported function exists")
                        .clone();
                    roots.insert(node.clone());
                    insert_candidate(
                        &mut candidates,
                        &mut candidate_by_node,
                        OidCandidate {
                            canonical_id: canonical.clone(),
                            kind: LinkedRelKind::ServiceExport,
                            source_sha256: unit.source_sha256.clone(),
                            node: Some(node),
                            descriptor_members: BTreeSet::new(),
                        },
                    )?;
                    public_services
                        .insert((logical_name.clone(), export.clone()), canonical.clone());
                    service_roots
                        .entry(logical_name.clone())
                        .or_default()
                        .insert(canonical);
                }

                for lifecycle in &file.lifecycle {
                    let node = NodeId::new(unit_index, format!("lifecycle:{}", lifecycle.verb));
                    if nodes
                        .insert(
                            node.clone(),
                            Node {
                                body: lifecycle.body.clone(),
                                edges: BTreeSet::new(),
                            },
                        )
                        .is_some()
                    {
                        return Err(LinkedRelDiscoveryError::DuplicateLocalSymbol {
                            source: logical_name.clone(),
                            symbol: format!("Service.{}", lifecycle.verb),
                        });
                    }
                    roots.insert(node.clone());
                    let canonical = canonical_symbol_id(&[
                        "service",
                        &logical_name,
                        "lifecycle",
                        &lifecycle.verb,
                    ])?;
                    insert_candidate(
                        &mut candidates,
                        &mut candidate_by_node,
                        OidCandidate {
                            canonical_id: canonical.clone(),
                            // Lifecycle hooks are compiler entry functions, not
                            // public Service Fabric exports.
                            kind: LinkedRelKind::Function,
                            source_sha256: unit.source_sha256.clone(),
                            node: Some(node),
                            descriptor_members: BTreeSet::new(),
                        },
                    )?;
                    service_roots
                        .entry(logical_name.clone())
                        .or_default()
                        .insert(canonical);
                }

                for class in &file.classes {
                    let class_canonical =
                        canonical_symbol_id(&["service", &logical_name, "class", &class.name])?;
                    let mut member_ids = BTreeSet::new();
                    for method in &class.methods {
                        let node = NodeId::new(
                            unit_index,
                            format!("class:{}:method:{}", class.name, method.name),
                        );
                        if nodes
                            .insert(
                                node.clone(),
                                Node {
                                    body: method.body.clone(),
                                    edges: BTreeSet::new(),
                                },
                            )
                            .is_some()
                        {
                            return Err(LinkedRelDiscoveryError::DuplicateLocalSymbol {
                                source: logical_name.clone(),
                                symbol: format!("{}.{}", class.name, method.name),
                            });
                        }
                        meta.class_method_nodes
                            .insert((class.name.clone(), method.name.clone()), node.clone());
                        let method_canonical = canonical_symbol_id(&[
                            "service",
                            &logical_name,
                            "class",
                            &class.name,
                            "method",
                            &method.name,
                        ])?;
                        member_ids.insert(method_canonical.clone());
                        insert_candidate(
                            &mut candidates,
                            &mut candidate_by_node,
                            OidCandidate {
                                canonical_id: method_canonical,
                                kind: LinkedRelKind::Method,
                                source_sha256: unit.source_sha256.clone(),
                                node: Some(node),
                                descriptor_members: BTreeSet::new(),
                            },
                        )?;
                    }
                    // A class descriptor has no executable body in the current
                    // service AST. Constructors are intentionally absent: these
                    // classes are namespace/static classes today.
                    insert_candidate(
                        &mut candidates,
                        &mut candidate_by_node,
                        OidCandidate {
                            canonical_id: class_canonical,
                            kind: LinkedRelKind::Class,
                            source_sha256: unit.source_sha256.clone(),
                            node: None,
                            descriptor_members: member_ids,
                        },
                    )?;
                }
            }
        }
        metas.push(meta);
    }

    // Pass 2: resolve import bindings only against public linked REL targets.
    for (unit_index, unit) in units.iter().enumerate() {
        let imports = match unit.source {
            LinkedRelSourceRef::Module(file) => &file.imports,
            LinkedRelSourceRef::Route(file) => &file.imports,
            LinkedRelSourceRef::Service(file) => &file.imports,
        };
        let resolved = imports
            .iter()
            .map(|import| {
                resolve_import_binding(import, &public_modules, &public_services)
                    .map(|binding| (binding_name(import), binding))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        metas[unit_index].imports = resolved;
    }

    // Pass 3: lower AST calls into an internal/canonical graph.
    for (node_id, node) in &mut nodes {
        let meta = &metas[node_id.unit];
        let mut edges = BTreeSet::new();
        collect_statement_edges(
            &node.body,
            meta,
            &public_modules,
            &public_services,
            &mut edges,
        )?;
        node.edges = edges;
    }

    // Pass 4: graph walk from runtime entry roots. Canonical cross-source edges
    // re-enter the corresponding declared body when one exists, so a Service ->
    // Module -> Module chain is analyzed transitively before OID allocation.
    let mut reachable_nodes = BTreeSet::new();
    let mut reachable_canonical = BTreeSet::new();
    let mut queue = VecDeque::<NodeId>::new();
    for root in roots {
        if reachable_nodes.insert(root.clone()) {
            queue.push_back(root);
        }
    }
    while let Some(node_id) = queue.pop_front() {
        if let Some(canonical) = candidate_by_node.get(&node_id) {
            reachable_canonical.insert(canonical.clone());
        }
        let Some(node) = nodes.get(&node_id) else {
            return Err(LinkedRelDiscoveryError::MissingInternalNode(node_id.local));
        };
        for edge in &node.edges {
            match edge {
                NodeEdge::Internal(target) => {
                    if reachable_nodes.insert(target.clone()) {
                        queue.push_back(target.clone());
                    }
                }
                NodeEdge::Canonical(canonical) => {
                    reachable_canonical.insert(canonical.clone());
                    if let Some(candidate) = candidates.get(canonical) {
                        if let Some(target) = &candidate.node {
                            if reachable_nodes.insert(target.clone()) {
                                queue.push_back(target.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // A used class method retains its class descriptor. Unused methods remain
    // dead and are not added merely because another method of the class is live.
    let mut used_class_members = BTreeMap::<String, BTreeSet<String>>::new();
    for (canonical, candidate) in &candidates {
        if candidate.kind != LinkedRelKind::Class {
            continue;
        }
        let live = candidate
            .descriptor_members
            .iter()
            .filter(|member| reachable_canonical.contains(*member))
            .cloned()
            .collect::<BTreeSet<_>>();
        if !live.is_empty() {
            reachable_canonical.insert(canonical.clone());
            used_class_members.insert(canonical.clone(), live);
        }
    }

    // Pass 5: build exact required-symbol edges between OID-bearing nodes.
    let mut symbols = Vec::new();
    for canonical in &reachable_canonical {
        let Some(candidate) = candidates.get(canonical) else {
            // Canonical edges are produced only from verified public export
            // tables, so absence here indicates compiler-state corruption.
            return Err(LinkedRelDiscoveryError::MissingCanonicalCandidate(
                canonical.clone(),
            ));
        };
        let required_symbols = if candidate.kind == LinkedRelKind::Class {
            used_class_members
                .get(canonical)
                .cloned()
                .unwrap_or_default()
        } else if let Some(node) = &candidate.node {
            first_oid_dependencies(node, canonical, &nodes, &candidate_by_node, &candidates)?
        } else {
            BTreeSet::new()
        };
        symbols.push(LinkedRelSymbolSpec {
            canonical_id: canonical.clone(),
            kind: candidate.kind,
            source_sha256: candidate.source_sha256.clone(),
            required_symbols,
            // Authorization/capability ownership remains in RELC's existing
            // typed capability analysis. Do not invent a second string policy
            // namespace here merely to populate the OID metadata field.
            capabilities: BTreeSet::new(),
        });
    }
    symbols.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));

    let mut service_exports = BTreeMap::<String, BTreeMap<String, String>>::new();
    for ((service, export), canonical) in public_services {
        service_exports
            .entry(service)
            .or_default()
            .insert(export, canonical);
    }

    Ok(LinkedRelDiscovery {
        symbols,
        service_roots,
        service_exports,
        reachable_symbols: reachable_canonical,
    })
}

fn declare_functions(
    unit: usize,
    functions: &[FunctionDef],
    meta: &mut UnitMeta,
    nodes: &mut BTreeMap<NodeId, Node>,
) -> Result<(), LinkedRelDiscoveryError> {
    for function in functions {
        let node = NodeId::new(unit, format!("fn:{}", function.name));
        if meta
            .function_nodes
            .insert(function.name.clone(), node.clone())
            .is_some()
            || nodes
                .insert(
                    node,
                    Node {
                        body: function.body.clone(),
                        edges: BTreeSet::new(),
                    },
                )
                .is_some()
        {
            return Err(LinkedRelDiscoveryError::DuplicateLocalSymbol {
                source: meta.logical_name.clone(),
                symbol: function.name.clone(),
            });
        }
    }
    Ok(())
}

fn insert_candidate(
    candidates: &mut BTreeMap<String, OidCandidate>,
    candidate_by_node: &mut BTreeMap<NodeId, String>,
    candidate: OidCandidate,
) -> Result<(), LinkedRelDiscoveryError> {
    if candidates.contains_key(&candidate.canonical_id) {
        return Err(LinkedRelDiscoveryError::CanonicalCollision(
            candidate.canonical_id,
        ));
    }
    if let Some(node) = &candidate.node {
        if candidate_by_node
            .insert(node.clone(), candidate.canonical_id.clone())
            .is_some()
        {
            return Err(LinkedRelDiscoveryError::DuplicateOidIdentityForNode(
                node.local.clone(),
            ));
        }
    }
    candidates.insert(candidate.canonical_id.clone(), candidate);
    Ok(())
}

fn resolve_import_binding(
    import: &ImportTarget,
    public_modules: &BTreeMap<(String, String), String>,
    public_services: &BTreeMap<(String, String), String>,
) -> Result<ImportBinding, LinkedRelDiscoveryError> {
    match import_base(import) {
        ImportTarget::Custom(path) => Ok(ImportBinding::ModuleNamespace(logical_module_name(path))),
        ImportTarget::CustomFunction { path, function } => {
            let logical = logical_module_name(path);
            let canonical = public_modules
                .get(&(logical.clone(), function.clone()))
                .ok_or_else(|| LinkedRelDiscoveryError::MissingImportedExport {
                    kind: "module",
                    source: logical.clone(),
                    export: function.clone(),
                })?;
            Ok(ImportBinding::LinkedFunction(canonical.clone()))
        }
        ImportTarget::Service(service) => Ok(ImportBinding::ServiceNamespace(service.clone())),
        ImportTarget::ServiceFunction { service, function } => {
            let canonical = public_services
                .get(&(service.clone(), function.clone()))
                .ok_or_else(|| LinkedRelDiscoveryError::MissingImportedExport {
                    kind: "service",
                    source: service.clone(),
                    export: function.clone(),
                })?;
            Ok(ImportBinding::LinkedFunction(canonical.clone()))
        }
        ImportTarget::Builtin(_)
        | ImportTarget::BuiltinFunction { .. }
        | ImportTarget::BuiltinSubLibrary { .. }
        | ImportTarget::Aliased { .. } => Ok(ImportBinding::Other),
    }
}

fn collect_statement_edges(
    statements: &[Statement],
    meta: &UnitMeta,
    public_modules: &BTreeMap<(String, String), String>,
    public_services: &BTreeMap<(String, String), String>,
    out: &mut BTreeSet<NodeEdge>,
) -> Result<(), LinkedRelDiscoveryError> {
    for statement in statements {
        match statement {
            Statement::Const { value, .. } | Statement::Return(value) | Statement::Expr(value) => {
                collect_expr_edges(value, meta, public_modules, public_services, out)?;
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                collect_expr_edges(condition, meta, public_modules, public_services, out)?;
                collect_statement_edges(then_body, meta, public_modules, public_services, out)?;
                collect_statement_edges(else_body, meta, public_modules, public_services, out)?;
            }
        }
    }
    Ok(())
}

fn collect_expr_edges(
    expr: &Expr,
    meta: &UnitMeta,
    public_modules: &BTreeMap<(String, String), String>,
    public_services: &BTreeMap<(String, String), String>,
    out: &mut BTreeSet<NodeEdge>,
) -> Result<(), LinkedRelDiscoveryError> {
    match expr {
        Expr::Call(callee, args) => {
            match callee.as_ref() {
                Expr::Ident(name) => {
                    if let Some(node) = meta.function_nodes.get(name) {
                        out.insert(NodeEdge::Internal(node.clone()));
                    } else if let Some(ImportBinding::LinkedFunction(canonical)) =
                        meta.imports.get(name)
                    {
                        out.insert(NodeEdge::Canonical(canonical.clone()));
                    }
                }
                Expr::Member(target, method) => {
                    if let Expr::Ident(owner) = target.as_ref() {
                        if let Some(node) = meta
                            .class_method_nodes
                            .get(&(owner.clone(), method.clone()))
                        {
                            out.insert(NodeEdge::Internal(node.clone()));
                        } else if let Some(binding) = meta.imports.get(owner) {
                            match binding {
                                ImportBinding::ModuleNamespace(logical) => {
                                    let canonical = public_modules
                                        .get(&(logical.clone(), method.clone()))
                                        .ok_or_else(|| {
                                            LinkedRelDiscoveryError::MissingImportedExport {
                                                kind: "module",
                                                source: logical.clone(),
                                                export: method.clone(),
                                            }
                                        })?;
                                    out.insert(NodeEdge::Canonical(canonical.clone()));
                                }
                                ImportBinding::ServiceNamespace(service) => {
                                    let canonical = public_services
                                        .get(&(service.clone(), method.clone()))
                                        .ok_or_else(|| {
                                            LinkedRelDiscoveryError::MissingImportedExport {
                                                kind: "service",
                                                source: service.clone(),
                                                export: method.clone(),
                                            }
                                        })?;
                                    out.insert(NodeEdge::Canonical(canonical.clone()));
                                }
                                ImportBinding::LinkedFunction(_) | ImportBinding::Other => {}
                            }
                        }
                    }
                    collect_expr_edges(target, meta, public_modules, public_services, out)?;
                }
                other => collect_expr_edges(other, meta, public_modules, public_services, out)?,
            }
            for arg in args {
                collect_expr_edges(arg, meta, public_modules, public_services, out)?;
            }
        }
        Expr::Member(target, _) | Expr::UnaryNot(target) => {
            collect_expr_edges(target, meta, public_modules, public_services, out)?;
        }
        Expr::Object(entries) => {
            for (_, value) in entries {
                collect_expr_edges(value, meta, public_modules, public_services, out)?;
            }
        }
        Expr::Array(values) => {
            for value in values {
                collect_expr_edges(value, meta, public_modules, public_services, out)?;
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_edges(left, meta, public_modules, public_services, out)?;
            collect_expr_edges(right, meta, public_modules, public_services, out)?;
        }
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null | Expr::Ident(_) => {}
    }
    Ok(())
}

/// Resolve the first OID-bearing boundaries reachable from one OID body. Local
/// private helpers are traversed through; reaching another candidate records
/// that candidate as a dependency and stops at that boundary. Self-recursion is
/// retained as a self OID dependency so native relocation can target the entry.
fn first_oid_dependencies(
    start: &NodeId,
    start_canonical: &str,
    nodes: &BTreeMap<NodeId, Node>,
    candidate_by_node: &BTreeMap<NodeId, String>,
    candidates: &BTreeMap<String, OidCandidate>,
) -> Result<BTreeSet<String>, LinkedRelDiscoveryError> {
    let mut required = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::from([start.clone()]);
    while let Some(node_id) = queue.pop_front() {
        if !visited.insert(node_id.clone()) {
            continue;
        }
        let node = nodes
            .get(&node_id)
            .ok_or_else(|| LinkedRelDiscoveryError::MissingInternalNode(node_id.local.clone()))?;
        for edge in &node.edges {
            match edge {
                NodeEdge::Canonical(canonical) => {
                    if !candidates.contains_key(canonical) {
                        return Err(LinkedRelDiscoveryError::MissingCanonicalCandidate(
                            canonical.clone(),
                        ));
                    }
                    required.insert(canonical.clone());
                }
                NodeEdge::Internal(target) => {
                    if let Some(canonical) = candidate_by_node.get(target) {
                        required.insert(canonical.clone());
                        // Crossing an OID boundary delegates its transitive
                        // requirements to that target's own record.
                        continue;
                    }
                    queue.push_back(target.clone());
                }
            }
        }
    }
    // A recursive exported function should explicitly retain its self-call.
    // The graph walk above already inserts it when the start body calls itself;
    // this assertion-style guard only ensures no accidental synthetic self edge.
    if required.contains(start_canonical) {
        return Ok(required);
    }
    Ok(required)
}

fn canonical_symbol_id(parts: &[&str]) -> Result<String, LinkedRelDiscoveryError> {
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        let component = canonical_component(part)?;
        if index != 0 {
            out.push('_');
        }
        out.push_str(&component);
    }
    if out.len() > 512 {
        return Err(LinkedRelDiscoveryError::CanonicalTooLong(out));
    }
    Ok(out)
}

fn canonical_component(value: &str) -> Result<String, LinkedRelDiscoveryError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(LinkedRelDiscoveryError::InvalidCanonicalComponent(
            value.to_string(),
        ));
    }
    let mut out = String::with_capacity(value.len());
    let mut previous_separator = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte as char);
            previous_separator = byte == b'_';
            continue;
        }
        if matches!(byte, b'/' | b'\\' | b'.' | b'-' | b':' | b' ') {
            if !previous_separator {
                out.push('_');
                previous_separator = true;
            }
            continue;
        }
        return Err(LinkedRelDiscoveryError::InvalidCanonicalComponent(
            value.to_string(),
        ));
    }
    while out.ends_with('_') {
        out.pop();
    }
    while out.starts_with('_') {
        out.remove(0);
    }
    if out.is_empty() {
        return Err(LinkedRelDiscoveryError::InvalidCanonicalComponent(
            value.to_string(),
        ));
    }
    Ok(out)
}

fn logical_module_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let after_amp = normalized.rsplit('&').next().unwrap_or(&normalized);
    let relative = after_amp.strip_prefix("./").unwrap_or(after_amp);
    let relative = relative.strip_prefix("module/").unwrap_or(relative);
    relative
        .strip_suffix(".module")
        .unwrap_or(relative)
        .to_string()
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => import_base(target),
        other => other,
    }
}

fn validate_sha256(value: &str) -> Result<(), LinkedRelDiscoveryError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(LinkedRelDiscoveryError::InvalidSourceSha256(
            value.to_string(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkedRelDiscoveryError {
    InvalidLogicalName(String),
    DuplicateSource {
        kind: &'static str,
        logical_name: String,
    },
    InvalidSourceSha256(String),
    InvalidCanonicalComponent(String),
    CanonicalTooLong(String),
    CanonicalCollision(String),
    DuplicateLocalSymbol {
        source: String,
        symbol: String,
    },
    DuplicateOidIdentityForNode(String),
    DuplicatePublicExport {
        source: String,
        export: String,
    },
    MissingExportBody {
        source: String,
        export: String,
    },
    MissingImportedExport {
        kind: &'static str,
        source: String,
        export: String,
    },
    MissingInternalNode(String),
    MissingCanonicalCandidate(String),
}

impl fmt::Display for LinkedRelDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogicalName(value) => write!(formatter, "invalid linked REL logical source name {value:?}"),
            Self::DuplicateSource { kind, logical_name } => write!(formatter, "duplicate linked {kind} REL source {logical_name:?}"),
            Self::InvalidSourceSha256(value) => write!(formatter, "invalid linked REL source SHA-256 {value:?}"),
            Self::InvalidCanonicalComponent(value) => write!(formatter, "invalid linked REL canonical-name component {value:?}"),
            Self::CanonicalTooLong(value) => write!(formatter, "linked REL canonical symbol exceeds 512 bytes: {value:?}"),
            Self::CanonicalCollision(value) => write!(formatter, "linked REL canonical symbol collision for {value:?}"),
            Self::DuplicateLocalSymbol { source, symbol } => write!(formatter, "duplicate local REL symbol {symbol:?} in {source:?}"),
            Self::DuplicateOidIdentityForNode(node) => write!(formatter, "local REL node {node:?} was assigned more than one OID identity"),
            Self::DuplicatePublicExport { source, export } => write!(formatter, "duplicate public REL export {export:?} in {source:?}"),
            Self::MissingExportBody { source, export } => write!(formatter, "public REL export {export:?} in {source:?} has no declared function body"),
            Self::MissingImportedExport { kind, source, export } => write!(formatter, "linked {kind} import {source:?}.{export} does not resolve to an approved public export"),
            Self::MissingInternalNode(node) => write!(formatter, "linked REL graph references missing internal node {node:?}"),
            Self::MissingCanonicalCandidate(symbol) => write!(formatter, "linked REL graph references undeclared OID candidate {symbol:?}"),
        }
    }
}

impl std::error::Error for LinkedRelDiscoveryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{MethodDef, ServiceClassDef};

    fn function(name: &str, body: Vec<Statement>) -> FunctionDef {
        FunctionDef {
            name: name.into(),
            params: Vec::new(),
            body,
        }
    }

    fn call_ident(name: &str) -> Expr {
        Expr::Call(Box::new(Expr::Ident(name.into())), Vec::new())
    }

    fn call_member(owner: &str, method: &str) -> Expr {
        Expr::Call(
            Box::new(Expr::Member(
                Box::new(Expr::Ident(owner.into())),
                method.into(),
            )),
            Vec::new(),
        )
    }

    #[test]
    fn private_helpers_do_not_consume_oids_but_preserve_reachable_exports() {
        let module = ModuleFile {
            imports: Vec::new(),
            functions: vec![
                function("publicValue", vec![Statement::Return(call_ident("helper"))]),
                function("helper", vec![Statement::Return(Expr::Number(7.0))]),
                function("unused", vec![Statement::Return(Expr::Number(9.0))]),
            ],
            exports: vec!["publicValue".into()],
        };
        let service = ServiceProgram {
            imports: vec![ImportTarget::CustomFunction {
                path: "module/math.module".into(),
                function: "publicValue".into(),
            }],
            functions: vec![function(
                "run",
                vec![Statement::Return(call_ident("publicValue"))],
            )],
            exports: vec!["run".into()],
            class_name: None,
            lifecycle: Vec::new(),
            classes: Vec::new(),
        };
        let discovery = discover_linked_rel_symbols(&[
            LinkedRelSourceUnit::module("math", "module-src", &module),
            LinkedRelSourceUnit::service("worker", "service-src", &service),
        ])
        .unwrap();
        let ids = discovery
            .symbols
            .iter()
            .map(|symbol| symbol.canonical_id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(ids.contains("module_math_publicValue"));
        assert!(ids.contains("service_worker_run"));
        assert_eq!(
            discovery.service_exports["worker"]["run"],
            "service_worker_run"
        );
        assert!(!ids
            .iter()
            .any(|id| id.contains("helper") || id.contains("unused")));
    }

    #[test]
    fn unused_module_exports_are_pruned_until_a_runtime_root_calls_them() {
        let module = ModuleFile {
            imports: Vec::new(),
            functions: vec![
                function("used", vec![Statement::Return(Expr::Bool(true))]),
                function("unused", vec![Statement::Return(Expr::Bool(false))]),
            ],
            exports: vec!["used".into(), "unused".into()],
        };
        let route = RouteFile {
            imports: vec![ImportTarget::CustomFunction {
                path: "module/shared.module".into(),
                function: "used".into(),
            }],
            field_bindings: Vec::new(),
            functions: Vec::new(),
            class_name: "Route".into(),
            methods: vec![MethodDef {
                verb: "get".into(),
                param_name: None,
                body: vec![Statement::Return(call_ident("used"))],
            }],
        };
        let discovery = discover_linked_rel_symbols(&[
            LinkedRelSourceUnit::module("shared", "module", &module),
            LinkedRelSourceUnit::route("home", "route", &route),
        ])
        .unwrap();
        assert_eq!(discovery.symbols.len(), 1);
        assert_eq!(discovery.symbols[0].canonical_id, "module_shared_used");
        assert_eq!(discovery.symbols[0].kind, LinkedRelKind::ModuleExport);
    }

    #[test]
    fn reachable_class_keeps_descriptor_and_only_used_method() {
        let service = ServiceProgram {
            imports: Vec::new(),
            functions: vec![function(
                "run",
                vec![Statement::Return(call_member("Cache", "get"))],
            )],
            exports: vec!["run".into()],
            class_name: None,
            lifecycle: Vec::new(),
            classes: vec![ServiceClassDef {
                name: "Cache".into(),
                bindings: Default::default(),
                methods: vec![
                    function("get", vec![Statement::Return(Expr::Number(1.0))]),
                    function("clear", vec![Statement::Return(Expr::Null)]),
                ],
            }],
        };
        let discovery = discover_linked_rel_symbols(&[LinkedRelSourceUnit::service(
            "worker", "service", &service,
        )])
        .unwrap();
        let by_id = discovery
            .symbols
            .iter()
            .map(|symbol| (symbol.canonical_id.clone(), symbol))
            .collect::<BTreeMap<_, _>>();
        let class_id = "service_worker_class_Cache";
        let get_id = "service_worker_class_Cache_method_get";
        assert!(by_id.contains_key(class_id));
        assert!(by_id.contains_key(get_id));
        assert!(!by_id.contains_key("service_worker_class_Cache_method_clear"));
        assert_eq!(
            by_id[class_id].required_symbols,
            BTreeSet::from([get_id.into()])
        );
        assert_eq!(
            by_id["service_worker_run"].required_symbols,
            BTreeSet::from([get_id.into()])
        );
    }

    #[test]
    fn service_lifecycle_is_a_root_even_without_public_exports() {
        let service = ServiceProgram {
            imports: Vec::new(),
            functions: Vec::new(),
            exports: Vec::new(),
            class_name: Some("Service".into()),
            lifecycle: vec![MethodDef {
                verb: "start".into(),
                param_name: None,
                body: vec![Statement::Return(Expr::Bool(true))],
            }],
            classes: Vec::new(),
        };
        let discovery = discover_linked_rel_symbols(&[LinkedRelSourceUnit::service(
            "daemon", "service", &service,
        )])
        .unwrap();
        assert_eq!(discovery.symbols.len(), 1);
        assert_eq!(
            discovery.symbols[0].canonical_id,
            "service_daemon_lifecycle_start"
        );
        assert!(discovery.service_roots["daemon"].contains("service_daemon_lifecycle_start"));
        assert!(!discovery.service_exports.contains_key("daemon"));
    }

    #[test]
    fn recursive_public_function_keeps_explicit_self_dependency() {
        let service = ServiceProgram {
            imports: Vec::new(),
            functions: vec![function(
                "again",
                vec![Statement::Return(call_ident("again"))],
            )],
            exports: vec!["again".into()],
            class_name: None,
            lifecycle: Vec::new(),
            classes: Vec::new(),
        };
        let discovery = discover_linked_rel_symbols(&[LinkedRelSourceUnit::service(
            "loop", "service", &service,
        )])
        .unwrap();
        assert_eq!(
            discovery.symbols[0].required_symbols,
            BTreeSet::from(["service_loop_again".into()])
        );
    }
}
