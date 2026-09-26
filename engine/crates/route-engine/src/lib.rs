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

// The route cache and direct parser helpers are retained as internal building
// blocks for the AOT/diagnostic pipeline even when a particular build path does
// not currently call them directly.
#![allow(dead_code)]

mod analyzer;
mod ast;
pub mod dependency_graph;
mod discovery;
pub mod embedded_rel;
pub mod execution_tracker;
mod field_manager;
mod interpreter;
mod lexer;
mod module_eval;
mod module_runtime;
mod modules;
mod parser;
mod paths;
mod route_collision;
mod service_eval;
mod terminal;

pub mod cache;
pub mod middleware_plan;
pub mod relc;
pub mod runtime_env;
pub mod runtime_image;
pub mod server_policy;
pub mod server_rel;
pub mod source_registry;
pub mod transpiled_support;
pub mod transpiler;
mod video_host;
pub mod wasm_compiler;

pub use analyzer::{analyze, Diagnostic, Severity};
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
pub use parser::ParseError;
pub use paths::{binary_dir, default_api_dir, default_module_dir, resolve_custom_import};
pub use relc::{
    compile_runtime_image, discover_physical_rel_sources, PhysicalRelSource, RelcError,
};
pub use runtime_env::{RuntimeEnv, RuntimeEnvError, RuntimeEnvOrigin};
pub use runtime_image::{RuntimeExecutable, RuntimeImage, RuntimeImageSlot, RuntimeSourceManifest};
pub use server_policy::{
    PolicyOrigin, RecursionPolicy, ResolvedPolicyValue, ServerPolicy, ServerPolicyError,
    ServerStatus,
};
pub use server_rel::{
    compile_server_source, parse_server_source, ServerCompileError, ServerProgram, ServerSetting,
    ServerSettingBody, ServerValue,
};
pub use service_eval::ServiceProgramExecutor;
pub use source_registry::{
    RelSource, RelSourceKind, RelSourceRegistry, SourceId, SourceOrigin, SourceRegistryError,
};
pub use wasm_compiler::{
    compile_route as compile_route_wasm, RouteWasmArtifact, RouteWasmCompilation,
    ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};

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

/// Validate the immutable Runtime Image against native API namespaces and
/// route/method collisions before backend boot launches any child processes.
pub fn validate_runtime_image_routes(image: &RuntimeImage) -> anyhow::Result<()> {
    route_collision::validate_image(image)
}

/// Reserve one additional runtime-configured native namespace before the HTTP
/// router is assembled. This is used by optional control-plane surfaces whose
/// path is not known when RELC performs its static native-route validation.
pub fn validate_runtime_image_reserved_namespace(
    image: &RuntimeImage,
    prefix: &str,
    label: &str,
) -> anyhow::Result<()> {
    route_collision::validate_image_reserved_namespace(image, prefix, label)
}
