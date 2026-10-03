//! Runtime Engine Language (REL) parsing and execution infrastructure.
//!
//! `.route`, `.module`, and `.service` already share core lexer/parser/evaluator
//! building blocks. RELC is being introduced around those pieces to discover,
//! identify, validate and eventually link every REL source role into a Runtime
//! Image. A source role controls capabilities and lifecycle; it does not receive
//! a weaker copy of the common REL grammar.
//!
//! Server REL is represented by the RELC source model and its structural
//! `server.server` front-end. Final ServerPolicy/Runtime Image lowering remains
//! a later RELC pass.

#![allow(dead_code)]

extern crate self as form_urlencoded;

mod form_urlencoded_compat {
    use std::borrow::Cow;

    fn hex_nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    fn decode(input: &[u8]) -> String {
        let mut decoded = Vec::with_capacity(input.len());
        let mut index = 0usize;
        while index < input.len() {
            match input[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < input.len() => {
                    match (hex_nibble(input[index + 1]), hex_nibble(input[index + 2])) {
                        (Some(high), Some(low)) => {
                            decoded.push((high << 4) | low);
                            index += 3;
                        }
                        _ => {
                            decoded.push(b'%');
                            index += 1;
                        }
                    }
                }
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&decoded).into_owned()
    }

    pub(crate) fn parse(
        input: &[u8],
    ) -> impl Iterator<Item = (Cow<'static, str>, Cow<'static, str>)> {
        let mut pairs = Vec::new();
        for sequence in input.split(|byte| *byte == b'&') {
            if sequence.is_empty() {
                continue;
            }
            let mut parts = sequence.splitn(2, |byte| *byte == b'=');
            let name = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            pairs.push((Cow::Owned(decode(name)), Cow::Owned(decode(value))));
        }
        pairs.into_iter()
    }

    #[cfg(test)]
    mod tests {
        use super::parse;

        #[test]
        fn decodes_form_pairs_like_query_input() {
            let pairs = parse(b"first+name=Kate%20K&flag&&encoded=%23ok%25")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            assert_eq!(
                pairs,
                vec![
                    ("first name".to_string(), "Kate K".to_string()),
                    ("flag".to_string(), String::new()),
                    ("encoded".to_string(), "#ok%".to_string()),
                ]
            );
        }

        #[test]
        fn malformed_percent_sequences_remain_literal() {
            let pairs = parse(b"value=%GG%2")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            assert_eq!(pairs, vec![("value".to_string(), "%GG%2".to_string())]);
        }
    }
}

pub(crate) use form_urlencoded_compat::parse;

mod analyzer;
mod ast;
pub mod dependency_graph;
mod discovery;
pub mod embedded_rel;
pub mod execution_tracker;
mod field_manager;
mod interpreter;
mod lexer;
#[cfg_attr(test, allow(clippy::type_complexity))]
mod module_eval;
mod module_runtime;
mod modules;
mod parser;
mod paths;
mod rel_host_builtins;
mod rel_host_descriptor;
mod rel_host_runtime;
mod route_collision;
mod runtime_roots;
mod service_eval;
mod temp_path;
mod terminal;

pub mod archive;
pub mod cache;
pub mod middleware_plan;
pub mod oid_cache_bootstrap;
pub mod oid_index_bridge;
pub mod oid_link;
pub mod oid_materialize;
pub mod rel_oid_bridge;
pub mod rel_symbol_discovery;
pub mod relc;
pub mod runtime_env;
pub mod runtime_image;
pub mod script;
pub mod server_policy;
pub mod server_rel;
pub mod service_bin;
pub mod service_native;
pub mod service_oid;
pub mod service_oid_adapter;
pub mod source_registry;
pub mod transpiled_support;
pub mod transpiler;
mod video_host;
pub mod wasm_compiler;
pub mod workspace;

pub use analyzer::{analyze, Diagnostic, Severity};
pub use archive::{ArchiveFormat, ArchivePath, ArchivePlan, ArchivePlanError, ArchiveWarmingKey};
pub use ast::{
    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,
    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceProgram, Statement, Value,
};
pub use dependency_graph::{SymbolDependencyGraph, SymbolId};
pub use discovery::RouteCache;
pub use embedded_rel::{
    extract_embedded_rel, EmbeddedRelError, EmbeddedRelSource, ExtractedServerRel,
};
pub use execution_tracker::{
    InvocationError, InvocationGuard, InvocationId, InvocationSnapshot, InvocationTracker,
};
pub use field_manager::{FieldResolveError, FieldRuntimeContext};
pub use interpreter::{EvalError, Interpreter, RequestContext};
pub use middleware_plan::{MiddlewarePlan, MiddlewarePlanError, MiddlewareStep};
pub use module_eval::{ModuleEvalError, ModuleExecutor, PackageCallFuture, PackageExportCaller};
pub use module_runtime::{
    ModuleCompileError, ModuleCompileErrors, ModuleProgram, ServiceInterfaces,
};
pub use modules::{binding_name, route_capability_allowed, ModuleError, ModuleRegistry};
pub use oid_cache_bootstrap::{
    clear_service_oid_cache, oid_cache_root, open_service_oid_cache, prepare_service_oid_cache,
};
pub use parser::ParseError;
pub use paths::{binary_dir, default_api_dir, default_module_dir, resolve_custom_import};
pub use rel_host_builtins::{
    allowed_for_role as host_builtin_allowed_for_role,
    function_exists as host_builtin_function_exists, is_host_builtin, ARCHIVE_BUILTIN,
    SCRIPT_BUILTIN, WORKSPACE_BUILTIN,
};
pub use rel_host_runtime::{
    RelHostExecutionFuture, RelHostExecutor, RelHostOutput, RelHostRequest,
};
pub use rel_symbol_discovery::{
    discover_linked_rel_symbols, linked_source_sha256, LinkedRelDiscovery,
    LinkedRelDiscoveryError, LinkedRelSourceRef, LinkedRelSourceUnit,
};
pub use relc::{
    compile_runtime_image, discover_physical_rel_sources, PhysicalRelSource, RelcError,
};
pub use runtime_env::{RuntimeEnv, RuntimeEnvError, RuntimeEnvOrigin};
pub use runtime_image::{RuntimeExecutable, RuntimeImage, RuntimeImageSlot, RuntimeSourceManifest};
pub use runtime_roots::RuntimeRootAuthority;
pub use script::{ScriptLanguage, ScriptPath, ScriptPlan, ScriptPlanError};
pub use server_policy::{
    PolicyOrigin, RecursionPolicy, ResolvedPolicyValue, ServerPolicy, ServerPolicyError,
    ServerStatus,
};
pub use server_rel::{
    compile_server_source, parse_server_source, ServerCompileError, ServerProgram, ServerSetting,
    ServerSettingBody, ServerValue,
};
pub use service_eval::ServiceProgramExecutor;
pub use service_oid::{
    CoreMaterializationReport, OidCache, OidDiagnostic, OidDiagnosticSeverity, OidError, OidIndex,
    OidRecord, OidRecordKind, OidRelocation, OidRelocationKind, OidSlotClass, OidTarget,
    PackageOidOwner, OID_DONE, OID_END_PACKAGE, OID_NATIVE_ABI_VERSION, OID_PACKAGE_END,
    OID_PACKAGE_START, OID_RBE_CORE_END, OID_RBE_CORE_START, OID_REL_END, OID_REL_START,
};
pub use source_registry::{
    RelSource, RelSourceKind, RelSourceRegistry, SourceId, SourceOrigin, SourceRegistryError,
};
pub use temp_path::{is_project_path, is_temp_path, validate_symbolic_path};
pub use wasm_compiler::{
    compile_route as compile_route_wasm, RouteWasmArtifact, RouteWasmCompilation,
    ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};
pub use workspace::{
    WorkspaceOperation, WorkspacePath, WorkspacePlan, WorkspacePlanError, WorkspaceRoot,
};

pub fn parse_service_source(source: &str) -> Result<ServiceProgram, ParseError> {
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .map_err(|error| ParseError {
            message: error.message,
            line: error.line,
            column: error.column,
        })?;
    parser::Parser::new(tokens).parse_service_file()
}

pub fn build_routes(
    api_dir: &std::path::Path,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    route_collision::validate(api_dir)?;
    discovery::build_routes(api_dir, service_interfaces)
}

pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    discovery::build_routes_from_image(image, service_interfaces)
}

pub fn build_routes_from_image_with_package_exports(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
    package_exports: std::sync::Arc<dyn PackageExportCaller>,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    discovery::build_routes_from_image_with_package_exports(
        image,
        service_interfaces,
        Some(package_exports),
    )
}

pub fn validate_runtime_image_routes(image: &RuntimeImage) -> anyhow::Result<()> {
    route_collision::validate_image(image)
}

pub fn validate_runtime_image_reserved_namespace(
    image: &RuntimeImage,
    prefix: &str,
    label: &str,
) -> anyhow::Result<()> {
    route_collision::validate_image_reserved_namespace(image, prefix, label)
}
