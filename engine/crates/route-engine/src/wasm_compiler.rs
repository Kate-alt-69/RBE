//! Native REL `.route` -> WebAssembly lowering.
//!
//! This compiler deliberately exposes a small executable subset first instead
//! of disguising interpreter execution as WASM. Unsupported REL constructs are
//! classified as an explicit interpreter fallback. Native artifacts use the
//! RBE worker ABI and return JSON bytes through `rbe.output_write`.

use std::collections::BTreeMap;

use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
    VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::{service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX};
use sha2::{Digest, Sha256};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::ast::{
    BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement, Value,
};
use crate::modules::binding_name;
use crate::runtime_image::{storage_capability_operation_allowed, storage_capability_target};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
/// Generation 13 compiles each HTTP method independently so one `.route`
/// can pin distinct native artifacts and explicit fallbacks per verb. Exact
/// import binding, capability authority, and ABI v3 remain unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 13;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STATIC_HELPER_CALL_DEPTH: usize = 32;
const WASM_PAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteWasmInput {
    None,
    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`
    /// value. This keeps strings/null/objects/arrays semantically identical.
    JsonBody,
    /// Invocation input is a JSON argument array containing exactly one
    /// evaluator-visible `req.body` value. The guest forwards these bytes to an
    /// already-authorized host capability without parsing or widening them.
    JsonBodyCapabilityArgument,
}

#[derive(Debug, Clone)]
pub struct RouteWasmArtifact {
    pub verb: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub input: RouteWasmInput,
}

#[derive(Debug, Clone)]
pub enum RouteWasmCompilation {
    Native(RouteWasmArtifact),
    InterpreterFallback { reason: String },
}

impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteWasmLinkContext {
    module_functions: BTreeMap<String, LinkedModuleFunction>,
}

#[derive(Debug, Clone)]
struct LinkedModuleFunction {
    owner: String,
    function: FunctionDef,
    imports: Vec<ImportTarget>,
}

impl RouteWasmLinkContext {
    pub(crate) fn insert_module_function(
        &mut self,
        route_binding: String,
        owner: String,
        function: FunctionDef,
        imports: Vec<ImportTarget>,
    ) {
        self.module_functions.insert(
            route_binding,
            LinkedModuleFunction {
                owner,
                function,
                imports,
            },
        );
    }
}

/// Compile the currently supported native route subset.
///
/// Native lowering remains intentionally strict per HTTP method. Compiler
/// generation v13 keeps ABI v3 and lets RELC compile every method in a Route
/// independently while retaining generation 12's exact import binding,
/// generation 11's static capability arguments, and generation 10's bounded
/// helper folding. Namespace imports, ambiguous bindings, dynamic transforms,
/// wider Module bodies, and nested host-call chains remain interpreter-only.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let links = RouteWasmLinkContext::default();
    compile_route_with_links(file, &links)
}

pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    if file.methods.len() != 1 {
        return fallback(
            "whole-route native compilation requires exactly one HTTP method; RELC must compile multi-method Routes per verb",
        );
    }
    compile_route_method_with_links(file, links, &file.methods[0])
}

pub(crate) fn compile_route_method_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
    method: &MethodDef,
) -> RouteWasmCompilation {
    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();
    let mut linked_imports = BTreeMap::<String, &LinkedModuleFunction>::new();
    for import in &file.imports {
        if let Some(found) = direct_capability_import(import) {
            let binding = found.binding.clone();
            if linked_imports.contains_key(&binding)
                || host_imports.insert(binding.clone(), found).is_some()
            {
                return fallback(format!(
                    "native Route-WASM v13 found ambiguous import binding {binding:?}"
                ));
            }
            continue;
        }

        if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
            let binding = binding_name(import);
            let Some(linked) = links.module_functions.get(&binding) else {
                return fallback(
                    "native linked Module function has no immutable RELC link context",
                );
            };
            if host_imports.contains_key(&binding)
                || linked_imports.insert(binding.clone(), linked).is_some()
            {
                return fallback(format!(
                    "native Route-WASM v13 found ambiguous import binding {binding:?}"
                ));
            }
            continue;
        }

        return fallback(
            "native Route-WASM v13 supports only exact direct http.get/post/request imports or exact linked Module function imports; namespace imports remain interpreter-only",
        );
    }
    let returned_binding = returned_call_binding(&method.body);
    let host_import = returned_binding.and_then(|binding| host_imports.get(binding));
    let linked_import = returned_binding.and_then(|binding| {
        linked_imports
            .get(binding)
            .map(|linked| (binding.to_string(), *linked))
    });

    let (bytes, input) = if let Some(import) = host_import {
        let [Statement::Return(expr)] = method.body.as_slice() else {
            return fallback(
                "native host capability route body currently requires one return statement",
            );
        };
        let Some(args) = static_direct_call(&import.binding, expr, &file.functions) else {
            return fallback(
                "native host capability calls require the directly imported function as the return value with static JSON arguments",
            );
        };
        let payload = match serde_json::to_vec(&args) {
            Ok(payload) => payload,
            Err(error) => return fallback(format!("encode native capability arguments: {error}")),
        };
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return fallback("native capability argument payload exceeds the capability envelope");
        }
        (
            encode_capability_call_module(import.kind, &import.target, &import.operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some((binding, linked)) = linked_import.as_ref() {
        let [Statement::Return(expr)] = method.body.as_slice() else {
            return fallback(
                "native linked Module route body currently requires one return statement",
            );
        };
        let call = match lower_linked_module_capability_call(
            binding,
            linked,
            expr,
            method.param_name.as_deref(),
            &file.functions,
        ) {
            Ok(call) => call,
            Err(reason) => return fallback(reason),
        };
        match call.payload {
            LoweredCapabilityPayload::Static(payload) => (
                encode_capability_call_module(call.kind, &call.target, &call.operation, &payload),
                RouteWasmInput::None,
            ),
            LoweredCapabilityPayload::JsonBodySingleArgument => (
                encode_input_capability_call_module(call.kind, &call.target, &call.operation),
                RouteWasmInput::JsonBodyCapabilityArgument,
            ),
        }
    } else if let Some(value) = static_route_result(&method.body, &file.functions) {
        let output = match serde_json::to_vec(&value) {
            Ok(output) => output,
            Err(error) => {
                return fallback(format!("static route result could not be encoded: {error}"))
            }
        };
        if output.len() > MAX_STATIC_OUTPUT_BYTES {
            return fallback("static route result exceeds the WASM execution output limit");
        }
        (encode_static_json_module(&output), RouteWasmInput::None)
    } else if let [Statement::Return(expr)] = method.body.as_slice() {
        if returns_request_body(method.param_name.as_deref(), expr) {
            (encode_input_echo_module(), RouteWasmInput::JsonBody)
        } else {
            return fallback("route return value is outside the native Route-WASM v3 subset");
        }
    } else {
        return fallback(
            "dynamic native route bodies currently require one return statement or request-independent control flow",
        );
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
        input,
    })
}

fn returned_call_binding(body: &[Statement]) -> Option<&str> {
    let [Statement::Return(Expr::Call(target, _))] = body else {
        return None;
    };
    let Expr::Ident(binding) = target.as_ref() else {
        return None;
    };
    Some(binding.as_str())
}

fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {
    RouteWasmCompilation::InterpreterFallback {
        reason: reason.into(),
    }
}

#[derive(Debug, Clone)]
struct DirectCapabilityImport {
    binding: String,
    kind: ContainerCapabilityKind,
    target: String,
    operation: String,
}

fn direct_capability_import(import: &ImportTarget) -> Option<DirectCapabilityImport> {
    let (binding, base) = match import {
        ImportTarget::Aliased { target, alias } => (alias.clone(), target.as_ref()),
        ImportTarget::BuiltinFunction { function, .. } => (function.clone(), import),
        _ => return None,
    };

    match base {
        ImportTarget::BuiltinFunction { module, function }
            if module == "http" && matches!(function.as_str(), "get" | "post" | "request") =>
        {
            Some(DirectCapabilityImport {
                binding,
                kind: ContainerCapabilityKind::Network,
                target: PUBLIC_HTTP_TARGET.to_string(),
                operation: function.clone(),
            })
        }
        _ => None,
    }
}

fn base_import(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => base_import(target),
        other => other,
    }
}

fn direct_linked_capability_import(
    import: &ImportTarget,
    owner: &str,
) -> Option<DirectCapabilityImport> {
    let binding = binding_name(import);
    let (kind, target, operation) = match base_import(import) {
        ImportTarget::BuiltinFunction { module, function }
            if module == "http" && matches!(function.as_str(), "get" | "post" | "request") =>
        {
            (
                ContainerCapabilityKind::Network,
                PUBLIC_HTTP_TARGET.to_string(),
                function.clone(),
            )
        }
        ImportTarget::BuiltinFunction { module, function }
            if module == "storage" && storage_capability_operation_allowed(function) =>
        {
            (
                ContainerCapabilityKind::Storage,
                storage_capability_target(owner)?,
                function.clone(),
            )
        }
        ImportTarget::BuiltinFunction { module, function }
            if matches!(module.as_str(), "vm" | "video-manager")
                && video_language_operation_allowed(function) =>
        {
            (
                ContainerCapabilityKind::Video,
                format!("{VIDEO_CAPABILITY_TARGET_PREFIX}{owner}"),
                function.clone(),
            )
        }
        ImportTarget::ServiceFunction { service, function }
            if service_capability_name_allowed(service) && valid_capability_operation(function) =>
        {
            (
                ContainerCapabilityKind::Service,
                format!("{SERVICE_CAPABILITY_TARGET_PREFIX}{service}"),
                function.clone(),
            )
        }
        _ => return None,
    };
    if target.len() > CONTAINER_MAX_CAPABILITY_TARGET_BYTES
        || operation.len() > CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
    {
        return None;
    }
    Some(DirectCapabilityImport {
        binding,
        kind,
        target,
        operation,
    })
}

fn valid_capability_operation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[derive(Debug, Clone)]
enum LoweredCapabilityPayload {
    Static(Vec<u8>),
    JsonBodySingleArgument,
}

#[derive(Debug, Clone)]
struct LoweredCapabilityCall {
    kind: ContainerCapabilityKind,
    target: String,
    operation: String,
    payload: LoweredCapabilityPayload,
}

fn lower_linked_module_capability_call(
    route_binding: &str,
    linked: &LinkedModuleFunction,
    route_expr: &Expr,
    request_parameter: Option<&str>,
    route_functions: &[FunctionDef],
) -> Result<LoweredCapabilityCall, String> {
    let Expr::Call(route_target, route_args) = route_expr else {
        return Err(
            "native linked Module calls require the imported function as the return value".into(),
        );
    };
    if !matches!(route_target.as_ref(), Expr::Ident(name) if name == route_binding) {
        return Err("native linked Module return must call the imported Module binding".into());
    }
    if route_args.len() != linked.function.params.len() {
        return Err(format!(
            "native linked Module call arity mismatch: Route supplied {}, Module function expects {}",
            route_args.len(),
            linked.function.params.len()
        ));
    }

    let [Statement::Return(module_expr)] = linked.function.body.as_slice() else {
        return Err(
            "native linked Module function must contain exactly one return statement".into(),
        );
    };
    let [module_import] = linked.imports.as_slice() else {
        return Err(
            "native linked Module function requires exactly one direct host capability import"
                .into(),
        );
    };
    let host = direct_linked_capability_import(module_import, &linked.owner).ok_or_else(|| {
        "native linked Module import must be one exact HTTP, Video, Service, or Storage function"
            .to_string()
    })?;
    let Expr::Call(target, host_args) = module_expr else {
        return Err("native linked Module return must directly call its host import".into());
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == &host.binding) {
        return Err("native linked Module return must call the imported host binding".into());
    }

    let route_function_map = static_function_map(route_functions).ok_or_else(|| {
        "native capability lowering requires unique local Route helper names".to_string()
    })?;
    let route_scope = BTreeMap::<String, Value>::new();
    if let Some(route_args) = route_args
        .iter()
        .map(|argument| {
            static_eval_expr(
                argument,
                &route_scope,
                &route_function_map,
                MAX_STATIC_HELPER_CALL_DEPTH,
            )
        })
        .collect::<Option<Vec<_>>>()
    {
        let bindings = linked
            .function
            .params
            .iter()
            .cloned()
            .zip(route_args)
            .collect::<BTreeMap<_, _>>();
        let no_module_helpers = BTreeMap::<String, FunctionDef>::new();
        let args = host_args
            .iter()
            .map(|argument| {
                static_eval_expr(
                    argument,
                    &bindings,
                    &no_module_helpers,
                    MAX_STATIC_HELPER_CALL_DEPTH,
                )
                .map(|value| static_value_to_json(&value))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "native linked Module host arguments must resolve to request-independent REL values"
                    .to_string()
            })?;
        let payload = serde_json::to_vec(&args)
            .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err(
                "native linked Module argument payload exceeds the capability envelope".into(),
            );
        }
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::Static(payload),
        });
    }

    let dynamic_body_passthrough = linked.function.params.len() == 1
        && route_args.len() == 1
        && host_args.len() == 1
        && returns_request_body(request_parameter, &route_args[0])
        && matches!(
            &host_args[0],
            Expr::Ident(name) if name == &linked.function.params[0]
        );
    if dynamic_body_passthrough {
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::JsonBodySingleArgument,
        });
    }

    Err(
        "native linked Module dynamic arguments currently support exactly one req.body value passed unchanged through one Module parameter into the host call"
            .into(),
    )
}

fn static_direct_call(
    binding: &str,
    expr: &Expr,
    functions: &[FunctionDef],
) -> Option<Vec<serde_json::Value>> {
    let Expr::Call(target, args) = expr else {
        return None;
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == binding) {
        return None;
    }

    let functions = static_function_map(functions)?;
    let scope = BTreeMap::<String, Value>::new();
    args.iter()
        .map(|argument| {
            static_eval_expr(argument, &scope, &functions, MAX_STATIC_HELPER_CALL_DEPTH)
                .map(|value| static_value_to_json(&value))
        })
        .collect()
}

fn static_function_map(functions: &[FunctionDef]) -> Option<BTreeMap<String, FunctionDef>> {
    let mut function_map = BTreeMap::new();
    for function in functions {
        if function_map
            .insert(function.name.clone(), function.clone())
            .is_some()
        {
            return None;
        }
    }
    Some(function_map)
}

#[derive(Debug, Clone)]
enum StaticFlow {
    Continue,
    Return(Value),
}

fn static_route_result(body: &[Statement], functions: &[FunctionDef]) -> Option<serde_json::Value> {
    let function_map = static_function_map(functions)?;

    let mut scope = BTreeMap::<String, Value>::new();
    let value = match static_exec_block(
        body,
        &mut scope,
        &function_map,
        MAX_STATIC_HELPER_CALL_DEPTH,
    )? {
        StaticFlow::Continue => Value::Null,
        StaticFlow::Return(value) => value,
    };
    Some(static_value_to_json(&value))
}

fn static_exec_block(
    body: &[Statement],
    scope: &mut BTreeMap<String, Value>,
    functions: &BTreeMap<String, FunctionDef>,
    remaining_helper_depth: usize,
) -> Option<StaticFlow> {
    for statement in body {
        match statement {
            Statement::Const { name, value } => {
                let value = static_eval_expr(value, scope, functions, remaining_helper_depth)?;
                scope.insert(name.clone(), value);
            }
            Statement::Return(expr) => {
                return Some(StaticFlow::Return(static_eval_expr(
                    expr,
                    scope,
                    functions,
                    remaining_helper_depth,
                )?));
            }
            Statement::Expr(expr) => {
                static_eval_expr(expr, scope, functions, remaining_helper_depth)?;
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let condition =
                    static_eval_expr(condition, scope, functions, remaining_helper_depth)?;
                let branch = if condition.truthy() {
                    then_body
                } else {
                    else_body
                };
                match static_exec_block(branch, scope, functions, remaining_helper_depth)? {
                    StaticFlow::Continue => {}
                    flow @ StaticFlow::Return(_) => return Some(flow),
                }
            }
        }
    }
    Some(StaticFlow::Continue)
}

fn static_eval_expr(
    expr: &Expr,
    scope: &BTreeMap<String, Value>,
    functions: &BTreeMap<String, FunctionDef>,
    remaining_helper_depth: usize,
) -> Option<Value> {
    match expr {
        Expr::String(value) => Some(Value::String(value.clone())),
        Expr::Number(value) => Some(Value::Number(*value)),
        Expr::Bool(value) => Some(Value::Bool(*value)),
        Expr::Null => Some(Value::Null),
        Expr::Ident(name) => scope.get(name).cloned(),
        Expr::Member(base, field) => {
            let base = static_eval_expr(base, scope, functions, remaining_helper_depth)?;
            crate::transpiled_support::member_get(&base, field).ok()
        }
        Expr::Call(target, args) => {
            if remaining_helper_depth == 0 {
                return None;
            }
            let Expr::Ident(name) = target.as_ref() else {
                return None;
            };
            let function = functions.get(name)?;
            if function.params.len() != args.len() {
                return None;
            }

            let values = args
                .iter()
                .map(|arg| static_eval_expr(arg, scope, functions, remaining_helper_depth))
                .collect::<Option<Vec<_>>>()?;
            let mut child_scope = function
                .params
                .iter()
                .cloned()
                .zip(values)
                .collect::<BTreeMap<_, _>>();
            match static_exec_block(
                &function.body,
                &mut child_scope,
                functions,
                remaining_helper_depth - 1,
            )? {
                StaticFlow::Continue => Some(Value::Null),
                StaticFlow::Return(value) => Some(value),
            }
        }
        Expr::Object(fields) => {
            let mut values = std::collections::HashMap::with_capacity(fields.len());
            for (name, value) in fields {
                values.insert(
                    name.clone(),
                    static_eval_expr(value, scope, functions, remaining_helper_depth)?,
                );
            }
            Some(Value::Object(values))
        }
        Expr::Array(items) => items
            .iter()
            .map(|item| static_eval_expr(item, scope, functions, remaining_helper_depth))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        Expr::UnaryNot(value) => Some(crate::transpiled_support::unary_not(static_eval_expr(
            value,
            scope,
            functions,
            remaining_helper_depth,
        )?)),
        Expr::Binary { left, op, right } => match op {
            BinaryOp::And => {
                let left = static_eval_expr(left, scope, functions, remaining_helper_depth)?;
                if !left.truthy() {
                    Some(Value::Bool(false))
                } else {
                    Some(Value::Bool(
                        static_eval_expr(right, scope, functions, remaining_helper_depth)?.truthy(),
                    ))
                }
            }
            BinaryOp::Or => {
                let left = static_eval_expr(left, scope, functions, remaining_helper_depth)?;
                if left.truthy() {
                    Some(Value::Bool(true))
                } else {
                    Some(Value::Bool(
                        static_eval_expr(right, scope, functions, remaining_helper_depth)?.truthy(),
                    ))
                }
            }
            _ => crate::transpiled_support::binary(
                *op,
                static_eval_expr(left, scope, functions, remaining_helper_depth)?,
                static_eval_expr(right, scope, functions, remaining_helper_depth)?,
            )
            .ok(),
        },
    }
}

fn static_value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(value) => serde_json::Value::String(value.clone()),
        Value::Number(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Null => serde_json::Value::Null,
        Value::Object(fields) => {
            let mut entries = fields.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(name, _)| (*name).clone());
            let mut object = serde_json::Map::new();
            for (name, value) in entries {
                object.insert(name.clone(), static_value_to_json(value));
            }
            serde_json::Value::Object(object)
        }
        Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(static_value_to_json).collect())
        }
    }
}

fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {
    let Some(parameter) = parameter else {
        return false;
    };
    matches!(
        expr,
        Expr::Member(target, field)
            if field == "body" && matches!(target.as_ref(), Expr::Ident(name) if name == parameter)
    )
}

fn encode_capability_call_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
    payload: &[u8],
) -> Vec<u8> {
    let target = target.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = operation_offset + operation.len();
    let response_offset = (payload_offset + payload.len() + 15) & !15usize;
    let memory_bytes = response_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES;

    let mut types = TypeSection::new();
    types.ty().function(
        [
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
        ],
        [ValType::I32],
    );
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "capability_call", EntityType::Function(0));
    imports.import("rbe", "capability_response_len", EntityType::Function(1));
    imports.import("rbe", "capability_response_read", EntityType::Function(2));
    imports.import("rbe", "output_write", EntityType::Function(2));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let pages = memory_bytes.max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // Four capability/output imports occupy function indices 0..=3.
    exports.export("run", ExportKind::Func, 4);

    let mut run = Function::new([(1, ValType::I32)]);
    run.instructions()
        .i32_const(kind.abi_code())
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .i32_const(payload.len() as i32)
        .call(0)
        .drop()
        .call(1)
        .local_set(0)
        .i32_const(response_offset as i32)
        .local_get(0)
        .call(2)
        .drop()
        .i32_const(response_offset as i32)
        .local_get(0)
        .call(3)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(
        0,
        &ConstExpr::i32_const(target_offset as i32),
        target.iter().copied(),
    );
    data.active(
        0,
        &ConstExpr::i32_const(operation_offset as i32),
        operation.iter().copied(),
    );
    data.active(
        0,
        &ConstExpr::i32_const(payload_offset as i32),
        payload.iter().copied(),
    );

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

fn encode_input_capability_call_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
) -> Vec<u8> {
    // Invocation input is already the exact JSON argument array `[req.body]`.
    // Keeping JSON encoding at the HTTP boundary means the guest never needs a
    // JSON parser and cannot reinterpret or widen the capability request.
    let target = target.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = (operation_offset + operation.len() + 15) & !15usize;
    let response_offset = (payload_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES + 15) & !15usize;
    let memory_bytes = response_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES;

    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function(
        [
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
        ],
        [ValType::I32],
    );

    let mut imports = ImportSection::new();
    imports.import("rbe", "input_len", EntityType::Function(0));
    imports.import("rbe", "input_read", EntityType::Function(1));
    imports.import("rbe", "capability_call", EntityType::Function(2));
    imports.import("rbe", "capability_response_len", EntityType::Function(0));
    imports.import("rbe", "capability_response_read", EntityType::Function(1));
    imports.import("rbe", "output_write", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(0);

    let pages = memory_bytes.max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, 6);

    let mut run = Function::new([(2, ValType::I32)]);
    run.instructions()
        .call(0)
        .local_set(0)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .call(1)
        .drop()
        .i32_const(kind.abi_code())
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .call(2)
        .drop()
        .call(3)
        .local_set(1)
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(4)
        .drop()
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(5)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(
        0,
        &ConstExpr::i32_const(target_offset as i32),
        target.iter().copied(),
    );
    data.active(
        0,
        &ConstExpr::i32_const(operation_offset as i32),
        operation.iter().copied(),
    );

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

fn encode_input_echo_module() -> Vec<u8> {
    // Route-WASM ABI v2 body passthrough: input bytes are already the JSON
    // encoding of req.body, so the guest only needs bounded input/output copy.
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "input_len", EntityType::Function(0));
    imports.import("rbe", "input_read", EntityType::Function(1));
    imports.import("rbe", "output_write", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(0);

    let pages = CONTAINER_MAX_EXECUTION_INPUT_BYTES
        .max(1)
        .div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // input_len/input_read/output_write occupy function indices 0..=2.
    exports.export("run", ExportKind::Func, 3);

    let mut run = Function::new([(1, ValType::I32)]);
    run.instructions()
        .call(0)
        .local_set(0)
        .i32_const(0)
        .local_get(0)
        .call(1)
        .drop()
        .i32_const(0)
        .local_get(0)
        .call(2)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code);
    module.finish()
}

fn encode_static_json_module(output: &[u8]) -> Vec<u8> {
    // Function type 0: rbe.output_write(i32 ptr, i32 len) -> i32.
    // Function type 1: run() -> i32.
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function([], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "output_write", EntityType::Function(0));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let pages = output.len().max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // output_write is imported function index 0; run is defined function index 1.
    exports.export("run", ExportKind::Func, 1);

    let mut run = Function::new([]);
    run.instructions()
        .i32_const(0)
        .i32_const(output.len() as i32)
        .call(0)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), output.iter().copied());

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use execution_engine::{CapabilityHost, ExecutionLimits, WasmExecutor};

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    fn parse_module(source: &str) -> crate::ast::ModuleFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_module_file().unwrap()
    }

    fn link_module_function(
        route_binding: &str,
        owner: &str,
        module: &crate::ast::ModuleFile,
        function: &str,
    ) -> RouteWasmLinkContext {
        let mut links = RouteWasmLinkContext::default();
        let function = module
            .functions
            .iter()
            .find(|candidate| candidate.name == function)
            .unwrap()
            .clone();
        links.insert_module_function(
            route_binding.to_string(),
            owner.to_string(),
            function,
            module.imports.clone(),
        );
        links
    }

    #[test]
    fn compiles_literal_route_to_valid_deterministic_wasm() {
        let route = parse("class Route { get(req) { return { ok: true, count: 7 }; } }");
        let RouteWasmCompilation::Native(first) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        let RouteWasmCompilation::Native(second) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        assert_eq!(first.verb, "get");
        assert_eq!(first.input, RouteWasmInput::None);
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(&first.bytes[..4], b"\0asm");
        wasmparser::validate(&first.bytes).unwrap();
    }

    #[test]
    fn request_independent_const_arithmetic_and_if_compile_to_native_wasm() {
        let route = parse(
            r#"class Route {
                get(req) {
                    const base = 6 * 7;
                    const enabled = base === 42;
                    if (enabled && !false) {
                        return { ok: true, value: base };
                    } else {
                        return { ok: false, value: 0 };
                    }
                }
            }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("request-independent REL control flow should compile natively");
        };
        assert_eq!(artifact.input, RouteWasmInput::None);
        wasmparser::validate(&artifact.bytes).unwrap();
        let result = WasmExecutor::new()
            .unwrap()
            .execute(&artifact.bytes, ExecutionLimits::default())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.output).unwrap(),
            serde_json::json!({ "ok": true, "value": 42.0 })
        );
    }

    #[test]
    fn request_independent_local_helpers_fold_into_native_wasm() {
        let route = parse(
            r#"function double(value) { return value * 2; }
               function build(value) {
                   if (value === 42) {
                       return { ok: true, value: double(value) };
                   }
                   return { ok: false, value: 0 };
               }
               class Route {
                   get(req) {
                       const answer = 21 * 2;
                       return build(answer);
                   }
               }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("pure request-independent helpers should compile natively");
        };
        assert_eq!(artifact.input, RouteWasmInput::None);
        wasmparser::validate(&artifact.bytes).unwrap();
        let result = WasmExecutor::new()
            .unwrap()
            .execute(&artifact.bytes, ExecutionLimits::default())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.output).unwrap(),
            serde_json::json!({ "ok": true, "value": 84.0 })
        );
    }

    #[test]
    fn request_dependent_local_helper_stays_interpreter_fallback() {
        let route = parse(
            r#"function echo(value) { return value; }
               class Route { post(req) { return echo(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { .. } = compile_route(&route) else {
            panic!("request-dependent helper arguments must remain dynamic");
        };
    }

    #[test]
    fn recursive_static_helper_fails_closed_at_compiler_depth_limit() {
        let route = parse(
            r#"function loop(value) { return loop(value); }
               class Route { get(req) { return loop(1); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { .. } = compile_route(&route) else {
            panic!("unbounded helper recursion must not execute during compilation");
        };
    }

    #[test]
    fn request_dependent_if_stays_interpreter_fallback() {
        let route = parse(
            r#"class Route {
                post(req) {
                    if (req.body) { return true; }
                    return false;
                }
            }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("request-dependent control flow must not be constant-folded");
        };
        assert!(reason.contains("request-independent control flow"));
    }

    #[test]
    fn multiple_exact_direct_imports_select_the_called_binding() {
        let route = parse(
            r#":import[http.get as fetch]
               :import[http.post as send]
               class Route {
                   get(req) { return send("https://example.com/data", { ok: true }); }
               }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("multiple exact direct imports should select the returned binding");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "post");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data", { "ok": true }])
            );
            Ok(br#"{"ok":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

    #[test]
    fn mixed_exact_direct_and_linked_imports_select_the_linked_binding() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(path) { return readEntry(path); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import[http.get as fetch]
               :import["./module/accounts/cache".load]
               class Route { get(req) { return load("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("mixed exact imports should select the linked binding");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"found":true}"#);
    }

    #[test]
    fn namespace_import_among_exact_imports_still_fails_closed() {
        let route = parse(
            r#":import[http]
               :import[http.get as fetch]
               class Route { get(req) { return fetch("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("namespace import must not gain native authority implicitly");
        };
        assert!(reason.contains("namespace imports remain interpreter-only"));
    }

    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct static http.get should lower to native capability ABI");
        };
        assert_eq!(artifact.input, RouteWasmInput::None);
        wasmparser::validate(&artifact.bytes).unwrap();
        assert!(artifact
            .bytes
            .windows(PUBLIC_HTTP_TARGET.len())
            .any(|window| window == PUBLIC_HTTP_TARGET.as_bytes()));
        assert!(artifact.bytes.windows(3).any(|window| window == b"get"));
        assert!(artifact
            .bytes
            .windows(b"https://example.com/data".len())
            .any(|window| window == b"https://example.com/data"));
    }

    #[test]
    fn direct_capability_arguments_can_use_pure_route_helpers() {
        let route = parse(
            r#":import[http.get]
               function origin() { return "https://example.com"; }
               function url(path) { return origin() + path; }
               class Route { get(req) { return get(url("/data")); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("pure helper-built HTTP argument should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Network);
            assert_eq!(request.operation, "get");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data"])
            );
            Ok(br#"{"ok":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

    #[test]
    fn linked_module_capability_arguments_fold_pure_rel_expressions() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(prefix, id) {
                   return readEntry(prefix + "/" + id + ".json");
               }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               function user() { return "kate"; }
               class Route { get(req) { return load("users", user()); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("pure linked Module capability expressions should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"found":true}"#);
    }

    #[test]
    fn generated_http_wasm_round_trips_through_real_capability_host_abi() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let expected =
            br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#;
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data"])
            );
            Ok(
                br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#
                    .to_vec(),
            )
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, expected);
        assert!(result.fuel_consumed > 0);
    }

    #[test]
    fn generated_http_wasm_fails_closed_when_host_denies_capability() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            Err("CAPABILITY_DENIED: test denial".into())
        });
        let error = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .expect_err("host denial must fail the whole WASM execution");
        let message = error.to_string();
        assert!(message.contains("WASM ABI violation"));
        assert!(message.contains("CAPABILITY_DENIED"));
    }

    #[test]
    fn linked_module_service_call_substitutes_static_route_arguments() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { get(req) { return lookup("kate"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("strict linked Service wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Service);
            assert_eq!(request.target, "service:uac");
            assert_eq!(request.operation, "get_user");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["kate"])
            );
            Ok(br#"{"id":"kate"}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"id":"kate"}"#);
    }

    #[test]
    fn linked_module_storage_call_uses_canonical_module_owner() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(path) { return readEntry(path); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { get(req) { return load("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("strict linked Storage wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":false}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"found":false}"#);
    }

    #[test]
    fn linked_module_storage_call_passes_dynamic_request_body_natively() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(path) { return readEntry(path); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { post(req) { return load(req.body); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("dynamic req.body Storage wrapper should compile natively");
        };
        assert_eq!(artifact.input, RouteWasmInput::JsonBodyCapabilityArgument);
        wasmparser::validate(&artifact.bytes).unwrap();

        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true,"dataHex":"6f6b"}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                br#"["users/kate.json"]"#,
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"found":true,"dataHex":"6f6b"}"#);
    }

    #[test]
    fn linked_module_video_call_uses_canonical_module_owner() {
        let module = parse_module(
            r#":import[video-manager.status as vmStatus]
               export function status() { return vmStatus(); }"#,
        );
        let links = link_module_function("status", "media.status", &module, "status");
        let route = parse(
            r#":import["./module/media/status".status]
               class Route { get(req) { return status(); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("strict linked Video wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Video);
            assert_eq!(request.target, "module:media.status");
            assert_eq!(request.operation, "status");
            assert_eq!(request.payload, b"[]");
            Ok(br#"{"ok":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

    #[test]
    fn linked_module_transformed_dynamic_route_argument_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { post(req) { return lookup(req.body.id); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("transformed dynamic linked arguments must remain interpreter-only");
        };
        assert!(reason.contains("exactly one req.body value passed unchanged"));
    }

    #[test]
    fn generic_capability_emitter_uses_versioned_kind_mapping() {
        for (kind, target, operation) in [
            (
                ContainerCapabilityKind::Storage,
                "storage:accounts.cache",
                "read",
            ),
            (
                ContainerCapabilityKind::Video,
                "module:media.bridge",
                "status",
            ),
            (
                ContainerCapabilityKind::Service,
                "service:uac-cache",
                "get_user",
            ),
        ] {
            let wasm = encode_capability_call_module(kind, target, operation, b"[]");
            wasmparser::validate(&wasm).unwrap();
            let expected_kind = kind;
            let expected_target = target.to_string();
            let expected_operation = operation.to_string();
            let host: CapabilityHost = Box::new(move |request| {
                assert_eq!(request.kind, expected_kind);
                assert_eq!(request.target, expected_target);
                assert_eq!(request.operation, expected_operation);
                assert_eq!(request.payload, b"[]");
                Ok(br#"{"ok":true}"#.to_vec())
            });
            let result = WasmExecutor::new()
                .unwrap()
                .execute_with_input_and_capabilities(
                    &wasm,
                    &[],
                    ExecutionLimits::default(),
                    Some(host),
                )
                .unwrap();
            assert_eq!(result.output, br#"{"ok":true}"#);
        }
    }

    #[test]
    fn direct_service_route_import_stays_outside_native_subset() {
        let route = parse(
            r#":import[service:uac.get_user]
               class Route { get(req) { return get_user("alice"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("Route-to-Service must remain behind an exported Module boundary");
        };
        assert!(reason.contains("linked Module function"));
    }

    #[test]
    fn direct_storage_route_import_stays_outside_native_subset() {
        let route = parse(
            r#":import[storage.read]
               class Route { get(req) { return read("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("Route-to-Storage must remain behind an exported Module boundary");
        };
        assert!(reason.contains("linked Module function"));
    }

    #[test]
    fn aliased_static_http_get_is_native() {
        let route = parse(
            r#":import[http.get as fetch]
               class Route { get(req) { return fetch("https://example.com/"); } }"#,
        );
        assert!(compile_route(&route).is_native());
    }

    #[test]
    fn http_namespace_import_stays_interpreter_fallback() {
        let route = parse(
            r#":import[http]
               class Route { get(req) { return http.get("https://example.com/"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("namespace import must not widen native Network authority");
        };
        assert!(reason.contains("namespace imports remain interpreter-only"));
    }

    #[test]
    fn dynamic_http_argument_stays_interpreter_fallback() {
        let route = parse(
            r#":import[http.get]
               class Route { post(req) { return get(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("dynamic HTTP argument is outside the v3 native subset");
        };
        assert!(reason.contains("static JSON arguments"));
    }

    #[test]
    fn request_body_passthrough_is_native_v3_input() {
        let route = parse("class Route { post(req) { return req.body; } }");
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("req.body passthrough should lower through Route-WASM v3 input");
        };
        assert_eq!(artifact.input, RouteWasmInput::JsonBody);
        wasmparser::validate(&artifact.bytes).unwrap();
    }

    #[test]
    fn other_runtime_expression_is_explicit_interpreter_fallback() {
        let route = parse("class Route { post(req) { return req.query; } }");
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("unsupported dynamic route must remain interpreter fallback");
        };
        assert!(reason.contains("outside the native Route-WASM v3 subset"));
    }

    #[test]
    fn multi_method_route_compiles_each_method_independently() {
        let route = parse(
            r#"class Route {
                get(req) { return { ok: true, method: "get" }; }
                post(req) { return req.body; }
            }"#,
        );
        assert_eq!(route.methods.len(), 2);
        let links = RouteWasmLinkContext::default();

        let RouteWasmCompilation::Native(get) =
            compile_route_method_with_links(&route, &links, &route.methods[0])
        else {
            panic!("GET should compile independently");
        };
        assert_eq!(get.verb, "get");
        assert_eq!(get.input, RouteWasmInput::None);
        let get_result = WasmExecutor::new()
            .unwrap()
            .execute(&get.bytes, ExecutionLimits::default())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get_result.output).unwrap(),
            serde_json::json!({ "ok": true, "method": "get" })
        );

        let RouteWasmCompilation::Native(post) =
            compile_route_method_with_links(&route, &links, &route.methods[1])
        else {
            panic!("POST should compile independently");
        };
        assert_eq!(post.verb, "post");
        assert_eq!(post.input, RouteWasmInput::JsonBody);
        let post_result = WasmExecutor::new()
            .unwrap()
            .execute_with_input(&post.bytes, br#"{"value":42}"#, ExecutionLimits::default())
            .unwrap();
        assert_eq!(post_result.output, br#"{"value":42}"#);
    }

    #[test]
    fn multi_method_route_can_mix_native_and_explicit_fallback() {
        let route = parse(
            r#"class Route {
                get(req) { return true; }
                post(req) { return req.query; }
            }"#,
        );
        let links = RouteWasmLinkContext::default();
        assert!(compile_route_method_with_links(&route, &links, &route.methods[0]).is_native());
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_method_with_links(&route, &links, &route.methods[1])
        else {
            panic!("unsupported POST should remain an explicit method fallback");
        };
        assert!(reason.contains("outside the native Route-WASM v3 subset"));
    }

    #[test]
    fn multiple_methods_are_not_collapsed_into_one_run_export() {
        let route = parse("class Route { get(req) { return true; } post(req) { return false; } }");
        assert!(!compile_route(&route).is_native());
    }
}
