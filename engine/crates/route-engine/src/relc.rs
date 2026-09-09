//! RELC multi-pass orchestration and Runtime Image linking.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde_json::Value as JsonValue;

use crate::ast::{Expr, FunctionDef, ImportTarget, ModuleFile, RouteFile, ServiceProgram, Statement};
use crate::dependency_graph::{SymbolDependencyGraph, SymbolId};
use crate::embedded_rel::{extract_embedded_rel, EmbeddedRelError};
use crate::lexer::Lexer;
use crate::middleware_plan::{MiddlewarePlan, MiddlewarePlanError};
use crate::modules::binding_name;
use crate::parser::{ParseError, Parser};
use crate::runtime_env::{RuntimeEnv, RuntimeEnvError};
use crate::runtime_image::{stable_source_hash, RuntimeImage, RuntimeSourceManifest};
use crate::server_policy::{ServerPolicy, ServerPolicyError};
use crate::server_rel::{compile_server_source, ServerCompileError, ServerProgram};
use crate::source_registry::{
    RelSourceKind, RelSourceRegistry, SourceId, SourceRegistryError,
};

#[derive(Debug, Clone)]
pub struct PhysicalRelSource {
    pub kind: RelSourceKind,
    pub logical_name: String,
    pub path: PathBuf,
    pub source: String,
}

impl PhysicalRelSource {
    pub fn new(
        kind: RelSourceKind,
        logical_name: impl Into<String>,
        path: impl Into<PathBuf>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            logical_name: logical_name.into(),
            path: path.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug)]
pub enum RelcError {
    Registry(SourceRegistryError),
    Embedded(EmbeddedRelError),
    Server(ServerCompileError),
    Parse {
        source: SourceId,
        error: ParseError,
    },
    RuntimeEnv(RuntimeEnvError),
    Policy(ServerPolicyError),
    Middleware(MiddlewarePlanError),
    Capability {
        source: SourceId,
        message: String,
    },
    Link(String),
}

impl fmt::Display for RelcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "RELC source registry: {error}"),
            Self::Embedded(error) => write!(formatter, "{error}"),
            Self::Server(error) => write!(formatter, "{error}"),
            Self::Parse { source, error } => write!(
                formatter,
                "RELC parse error in {source} at {}:{}: {}",
                error.line, error.column, error.message
            ),
            Self::RuntimeEnv(error) => write!(formatter, "RELC Runtime ENV: {error}"),
            Self::Policy(error) => write!(formatter, "RELC ServerPolicy: {error}"),
            Self::Middleware(error) => write!(formatter, "RELC MiddlewarePlan: {error}"),
            Self::Capability { source, message } => {
                write!(formatter, "RELC capability error in {source}: {message}")
            }
            Self::Link(message) => write!(formatter, "RELC link error: {message}"),
        }
    }
}

impl std::error::Error for RelcError {}

impl From<SourceRegistryError> for RelcError {
    fn from(value: SourceRegistryError) -> Self {
        Self::Registry(value)
    }
}
impl From<EmbeddedRelError> for RelcError {
    fn from(value: EmbeddedRelError) -> Self {
        Self::Embedded(value)
    }
}
impl From<ServerCompileError> for RelcError {
    fn from(value: ServerCompileError) -> Self {
        Self::Server(value)
    }
}
impl From<RuntimeEnvError> for RelcError {
    fn from(value: RuntimeEnvError) -> Self {
        Self::RuntimeEnv(value)
    }
}
impl From<ServerPolicyError> for RelcError {
    fn from(value: ServerPolicyError) -> Self {
        Self::Policy(value)
    }
}
impl From<MiddlewarePlanError> for RelcError {
    fn from(value: MiddlewarePlanError) -> Self {
        Self::Middleware(value)
    }
}

/// Compiles one whole application image. All sources are registered before
/// parsing/linking, so source order never decides whether a dependency exists.
pub fn compile_runtime_image(
    raw_server_source: &str,
    mut physical_sources: Vec<PhysicalRelSource>,
    settings_json: &JsonValue,
) -> Result<RuntimeImage, RelcError> {
    let extracted = extract_embedded_rel(raw_server_source)?;
    let server = compile_server_source(&extracted.server_source)?;

    // PASS 1: source discovery/identity.
    let mut registry = RelSourceRegistry::new();
    let server_id = registry.register_physical(
        RelSourceKind::Server,
        &server.name,
        "server.server",
        raw_server_source,
    )?;

    physical_sources.sort_by(|left, right| {
        (left.kind, left.logical_name.as_str(), left.path.as_path()).cmp(&(
            right.kind,
            right.logical_name.as_str(),
            right.path.as_path(),
        ))
    });
    for source in physical_sources {
        if source.kind == RelSourceKind::Server {
            return Err(RelcError::Link(
                "server.server is the single physical Server REL root".into(),
            ));
        }
        registry.register_physical(
            source.kind,
            source.logical_name,
            source.path,
            source.source,
        )?;
    }
    for embedded in extracted.embedded {
        registry.register_embedded(
            &server_id,
            embedded.kind,
            embedded.logical_name,
            embedded.block_index,
            embedded.start_line,
            embedded.source,
        )?;
    }

    // PASS 2: role-specific parsing using the shared REL lexer/grammar pieces.
    let mut compiled = BTreeMap::<SourceId, CompiledUnit>::new();
    for source in registry.iter() {
        if source.kind() == RelSourceKind::Server {
            compiled.insert(source.id().clone(), CompiledUnit::Server(server.clone()));
            continue;
        }
        let unit = parse_registered_source(source.id(), source.kind(), source.source())?;
        validate_capabilities(source.id(), source.kind(), unit.imports())?;
        compiled.insert(source.id().clone(), unit);
    }
    validate_capabilities(&server_id, RelSourceKind::Server, &server.imports)?;

    // PASS 3/4: declaration + import target collection.
    validate_import_targets(&registry, &compiled)?;

    // PASS 5: symbol graph. Import cycles are not rejected; only actual symbol
    // call edges form recursive SCC metadata.
    let dependency_graph = build_symbol_graph(&registry, &compiled);
    let recursive_groups = dependency_graph.recursive_groups();
    let symbol_table = dependency_graph.symbols().cloned().collect::<BTreeSet<_>>();

    // PASS 6: typed capability/policy/env analysis.
    let settings_env = runtime_env_from_settings(settings_json)?;
    let environment = RuntimeEnv::resolve(&BTreeMap::new(), &server, &settings_env)?;
    let server_policy = ServerPolicy::resolve(&server, settings_json)?;
    let middleware_plan = MiddlewarePlan::lower(&server)?;

    // PASS 7/8/9 are metadata/lowering boundaries in v1. The current evaluator
    // remains the executable representation while the image owns validated
    // policy, identities and graph information.
    let mut sources = Vec::new();
    let mut routes = Vec::new();
    let mut modules = Vec::new();
    let mut services = Vec::new();
    let mut capabilities = BTreeMap::new();
    let mut service_assignments = BTreeMap::new();

    for source in registry.iter() {
        let unit = compiled.get(source.id()).ok_or_else(|| {
            RelcError::Link(format!("registered source {} was not compiled", source.id()))
        })?;
        let manifest = RuntimeSourceManifest {
            id: source.id().clone(),
            kind: source.kind(),
            logical_name: source.logical_name().to_string(),
            exports: unit.exports(),
            imports: unit.imports().iter().map(import_label).collect(),
        };
        match source.kind() {
            RelSourceKind::Route => routes.push(source.id().clone()),
            RelSourceKind::Module => modules.push(source.id().clone()),
            RelSourceKind::Service => {
                services.push(source.id().clone());
                service_assignments.insert(
                    source.logical_name().to_string(),
                    "service-manager:auto".into(),
                );
            }
            RelSourceKind::Server => {}
        }
        capabilities.insert(source.id().clone(), capability_set(unit.imports()));
        sources.push(manifest);
    }

    // PASS 10: immutable Runtime Image link.
    let source_hash = stable_source_hash(
        registry
            .iter()
            .map(|source| (source.id(), source.source())),
    );
    Ok(RuntimeImage {
        image_id: format!("rbe-{source_hash:016x}"),
        source_hash,
        server_policy,
        environment,
        routes,
        modules,
        services,
        sources,
        symbol_table,
        dependency_graph,
        recursive_groups,
        middleware_plan,
        service_assignments,
        capabilities,
    })
}

fn runtime_env_from_settings(
    settings: &JsonValue,
) -> Result<BTreeMap<String, JsonValue>, RelcError> {
    let Some(value) = settings.get("runtimeEnv") else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        RelcError::Link("settings.json runtimeEnv must be a JSON object".into())
    })?;
    Ok(object
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

fn parse_registered_source(
    id: &SourceId,
    kind: RelSourceKind,
    source: &str,
) -> Result<CompiledUnit, RelcError> {
    let tokens = Lexer::new(source).tokenize().map_err(|error| RelcError::Parse {
        source: id.clone(),
        error: ParseError {
            message: error.message,
            line: error.line,
            column: error.column,
        },
    })?;
    let result = match kind {
        RelSourceKind::Route => Parser::new(tokens).parse_file().map(CompiledUnit::Route),
        RelSourceKind::Module => Parser::new(tokens)
            .parse_module_file()
            .map(CompiledUnit::Module),
        RelSourceKind::Service => Parser::new(tokens)
            .parse_service_file()
            .map(CompiledUnit::Service),
        RelSourceKind::Server => unreachable!("Server REL is compiled before registry parsing"),
    };
    result.map_err(|error| RelcError::Parse {
        source: id.clone(),
        error,
    })
}

fn validate_capabilities(
    source: &SourceId,
    kind: RelSourceKind,
    imports: &[ImportTarget],
) -> Result<(), RelcError> {
    for import in imports {
        let base = import_base(import);
        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } = base
        {
            if name == "ENV" && !RuntimeEnv::can_read(kind) {
                return Err(RelcError::Capability {
                    source: source.clone(),
                    message: "Runtime ENV is not exposed to Route REL by default".into(),
                });
            }
            if name == "quickDB" && kind != RelSourceKind::Service {
                return Err(RelcError::Capability {
                    source: source.clone(),
                    message: "quickDB is a Service REL process-local capability".into(),
                });
            }
        }
        if matches!(base, ImportTarget::Service(_) | ImportTarget::ServiceFunction { .. })
            && kind == RelSourceKind::Route
        {
            return Err(RelcError::Capability {
                source: source.clone(),
                message: "Route REL cannot directly import Service REL; use a module boundary".into(),
            });
        }
    }
    Ok(())
}

fn validate_import_targets(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
) -> Result<(), RelcError> {
    for (source_id, unit) in compiled {
        for import in unit.imports() {
            match import_base(import) {
                ImportTarget::Custom(path) | ImportTarget::CustomFunction { path, .. } => {
                    let logical = logical_module_name(path);
                    if registry
                        .get_logical(RelSourceKind::Module, &logical)
                        .is_none()
                    {
                        return Err(RelcError::Link(format!(
                            "{source_id} imports missing module `{logical}`"
                        )));
                    }
                }
                ImportTarget::Service(service)
                | ImportTarget::ServiceFunction { service, .. } => {
                    if registry
                        .get_logical(RelSourceKind::Service, service)
                        .is_none()
                    {
                        return Err(RelcError::Link(format!(
                            "{source_id} imports missing service `{service}`"
                        )));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn build_symbol_graph(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
) -> SymbolDependencyGraph {
    let mut graph = SymbolDependencyGraph::default();
    for (source_id, unit) in compiled {
        for (name, body) in unit.symbol_bodies() {
            let from = SymbolId::new(source_id.clone(), name.clone());
            graph.add_symbol(from.clone());
            let imports = import_bindings(registry, unit.imports());
            let locals = unit.symbol_names();
            collect_statement_edges(
                &from,
                &body,
                &locals,
                &imports,
                &mut graph,
            );
        }
    }
    graph
}

#[derive(Clone)]
struct ImportBinding {
    source: SourceId,
    function: Option<String>,
}

fn import_bindings(
    registry: &RelSourceRegistry,
    imports: &[ImportTarget],
) -> BTreeMap<String, ImportBinding> {
    let mut out = BTreeMap::new();
    for import in imports {
        let binding = binding_name(import);
        match import_base(import) {
            ImportTarget::Custom(path) => {
                if let Some(target) = registry.get_logical(RelSourceKind::Module, &logical_module_name(path)) {
                    out.insert(binding, ImportBinding { source: target.id().clone(), function: None });
                }
            }
            ImportTarget::CustomFunction { path, function } => {
                if let Some(target) = registry.get_logical(RelSourceKind::Module, &logical_module_name(path)) {
                    out.insert(binding, ImportBinding { source: target.id().clone(), function: Some(function.clone()) });
                }
            }
            ImportTarget::Service(service) => {
                if let Some(target) = registry.get_logical(RelSourceKind::Service, service) {
                    out.insert(binding, ImportBinding { source: target.id().clone(), function: None });
                }
            }
            ImportTarget::ServiceFunction { service, function } => {
                if let Some(target) = registry.get_logical(RelSourceKind::Service, service) {
                    out.insert(binding, ImportBinding { source: target.id().clone(), function: Some(function.clone()) });
                }
            }
            _ => {}
        }
    }
    out
}

fn collect_statement_edges(
    from: &SymbolId,
    statements: &[Statement],
    locals: &BTreeSet<String>,
    imports: &BTreeMap<String, ImportBinding>,
    graph: &mut SymbolDependencyGraph,
) {
    for statement in statements {
        match statement {
            Statement::Const { value, .. } | Statement::Return(value) | Statement::Expr(value) => {
                collect_expr_edges(from, value, locals, imports, graph)
            }
            Statement::If { condition, then_body, else_body } => {
                collect_expr_edges(from, condition, locals, imports, graph);
                collect_statement_edges(from, then_body, locals, imports, graph);
                collect_statement_edges(from, else_body, locals, imports, graph);
            }
        }
    }
}

fn collect_expr_edges(
    from: &SymbolId,
    expr: &Expr,
    locals: &BTreeSet<String>,
    imports: &BTreeMap<String, ImportBinding>,
    graph: &mut SymbolDependencyGraph,
) {
    match expr {
        Expr::Call(callee, args) => {
            match callee.as_ref() {
                Expr::Ident(name) if locals.contains(name) => {
                    graph.add_edge(from.clone(), SymbolId::new(from.source.clone(), name.clone()));
                }
                Expr::Ident(name) => {
                    if let Some(binding) = imports.get(name) {
                        if let Some(function) = &binding.function {
                            graph.add_edge(
                                from.clone(),
                                SymbolId::new(binding.source.clone(), function.clone()),
                            );
                        }
                    }
                }
                Expr::Member(target, method) => {
                    if let Expr::Ident(binding_name) = target.as_ref() {
                        if let Some(binding) = imports.get(binding_name) {
                            graph.add_edge(
                                from.clone(),
                                SymbolId::new(binding.source.clone(), method.clone()),
                            );
                        }
                    }
                    collect_expr_edges(from, target, locals, imports, graph);
                }
                other => collect_expr_edges(from, other, locals, imports, graph),
            }
            for arg in args {
                collect_expr_edges(from, arg, locals, imports, graph);
            }
        }
        Expr::Member(target, _) | Expr::UnaryNot(target) => {
            collect_expr_edges(from, target, locals, imports, graph)
        }
        Expr::Object(entries) => {
            for (_, value) in entries {
                collect_expr_edges(from, value, locals, imports, graph);
            }
        }
        Expr::Array(values) => {
            for value in values {
                collect_expr_edges(from, value, locals, imports, graph);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_edges(from, left, locals, imports, graph);
            collect_expr_edges(from, right, locals, imports, graph);
        }
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null | Expr::Ident(_) => {}
    }
}

fn logical_module_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let after_amp = normalized.rsplit('&').next().unwrap_or(&normalized);
    let leaf = after_amp.rsplit('/').next().unwrap_or(after_amp);
    leaf.strip_suffix(".module").unwrap_or(leaf).to_string()
}

fn capability_set(imports: &[ImportTarget]) -> BTreeSet<String> {
    imports
        .iter()
        .filter_map(|import| match import_base(import) {
            ImportTarget::Builtin(name) => Some(name.clone()),
            ImportTarget::BuiltinFunction { module, .. } => Some(module.clone()),
            _ => None,
        })
        .collect()
}

fn import_label(import: &ImportTarget) -> String {
    match import {
        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::BuiltinFunction { module, function } => format!("{module}.{function}"),
        ImportTarget::Custom(path) => format!("module:{path}"),
        ImportTarget::CustomFunction { path, function } => format!("module:{path}.{function}"),
        ImportTarget::Service(service) => format!("service:{service}"),
        ImportTarget::ServiceFunction { service, function } => {
            format!("service:{service}.{function}")
        }
        ImportTarget::Aliased { target, alias } => format!("{} as {alias}", import_label(target)),
    }
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => import_base(target),
        other => other,
    }
}

#[derive(Debug, Clone)]
enum CompiledUnit {
    Route(RouteFile),
    Module(ModuleFile),
    Service(ServiceProgram),
    Server(ServerProgram),
}

impl CompiledUnit {
    fn imports(&self) -> &[ImportTarget] {
        match self {
            Self::Route(file) => &file.imports,
            Self::Module(file) => &file.imports,
            Self::Service(file) => &file.imports,
            Self::Server(file) => &file.imports,
        }
    }

    fn exports(&self) -> Vec<String> {
        match self {
            Self::Route(file) => file
                .methods
                .iter()
                .map(|method| format!("Route.{}", method.verb))
                .collect(),
            Self::Module(file) => file.exports.clone(),
            Self::Service(file) => file.exports.clone(),
            Self::Server(_) => Vec::new(),
        }
    }

    fn symbol_names(&self) -> BTreeSet<String> {
        self.symbol_bodies().into_iter().map(|(name, _)| name).collect()
    }

    fn symbol_bodies(&self) -> Vec<(String, Vec<Statement>)> {
        let mut out = Vec::new();
        match self {
            Self::Route(file) => {
                add_functions(&mut out, &file.functions);
                for method in &file.methods {
                    out.push((format!("Route.{}", method.verb), method.body.clone()));
                }
            }
            Self::Module(file) => add_functions(&mut out, &file.functions),
            Self::Service(file) => {
                add_functions(&mut out, &file.functions);
                for method in &file.lifecycle {
                    out.push((format!("Service.{}", method.verb), method.body.clone()));
                }
                for class in &file.classes {
                    for method in &class.methods {
                        out.push((format!("{}.{}", class.name, method.name), method.body.clone()));
                    }
                }
            }
            Self::Server(file) => add_functions(&mut out, &file.functions),
        }
        out
    }
}

fn add_functions(output: &mut Vec<(String, Vec<Statement>)>, functions: &[FunctionDef]) {
    output.extend(
        functions
            .iter()
            .map(|function| (function.name.clone(), function.body.clone())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_physical_and_embedded_sources_into_one_image() {
        let server = r#"server Main {
            status online;
            env { APP_NAME "RBE"; force LOCKED true; }
            middleware { correlationId; errorHandler; }
        }
        [file-start:module.Auth]
        export function appName() { return "RBE"; }
        [file-end:module]
        [file-start:route.Health path="/health"]
        class Route { get() { return true; } }
        [file-end:route]
        "#;
        let settings = serde_json::json!({
            "api": { "host": "0.0.0.0", "port": 7044, "requestTimeoutMs": 30000, "maxBodySizeBytes": 10485760 },
            "runtimeEnv": { "APP_NAME": "settings", "LOCKED": false }
        });
        let image = compile_runtime_image(server, Vec::new(), &settings).unwrap();
        assert_eq!(image.routes.len(), 1);
        assert_eq!(image.modules.len(), 1);
        assert_eq!(image.environment.string("APP_NAME").unwrap(), "settings");
        assert!(image.environment.bool("LOCKED").unwrap());
        assert_eq!(image.middleware_plan.steps.len(), 2);
    }

    #[test]
    fn rejects_physical_embedded_logical_collision() {
        let server = r#"server Main {}
        [file-start:module.Auth]
        export function value() { return 1; }
        [file-end:module]
        "#;
        let physical = vec![PhysicalRelSource::new(
            RelSourceKind::Module,
            "Auth",
            "module/Auth.module",
            "export function value() { return 2; }",
        )];
        assert!(compile_runtime_image(server, physical, &serde_json::json!({})).is_err());
    }

    #[test]
    fn route_runtime_env_is_a_capability_error_not_a_grammar_error() {
        let server = "server Main {}";
        let route = vec![PhysicalRelSource::new(
            RelSourceKind::Route,
            "/bad",
            "api/bad.route",
            ":import[ENV] class Route { get() { return ENV.get(\"A\"); } }",
        )];
        assert!(matches!(
            compile_runtime_image(server, route, &serde_json::json!({})),
            Err(RelcError::Capability { .. })
        ));
    }
}