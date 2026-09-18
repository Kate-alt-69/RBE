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

use crate::ast::{Expr, FunctionDef, ImportTarget, RouteFile, Statement};
use crate::modules::binding_name;
use crate::runtime_image::{storage_capability_operation_allowed, storage_capability_target};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
/// Generation 7 adds exact Module-owned Environment Storage calls to the
/// existing linked Module authority boundary. Direct Route-to-Storage remains
/// unreachable and capability ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 7;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const WASM_PAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteWasmInput {
    None,
    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`
    /// value. This keeps strings/null/objects/arrays semantically identical.
    JsonBody,
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
/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler generation v7 keeps ABI v3 and permits
/// one immutable linked Module function supplied by RELC. The linked function
/// must itself be exactly one return of one direct HTTP, Video, Service, or
/// Storage host call, and every Route/host argument must resolve to static JSON.
/// Namespace imports, dynamic arguments, wider Module bodies, and nested chains
/// remain interpreter-only.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let links = RouteWasmLinkContext::default();
    compile_route_with_links(file, &links)
}

pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    let mut host_import = None;
    let mut linked_import = None;
    match file.imports.as_slice() {
        [] => {}
        [import] => {
            if let Some(found) = direct_capability_import(import) {
                host_import = Some(found);
            } else if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
                let binding = binding_name(import);
                let Some(linked) = links.module_functions.get(&binding) else {
                    return fallback(
                        "native linked Module function has no immutable RELC link context",
                    );
                };
                linked_import = Some((binding, linked));
            } else {
                return fallback(
                    "native Route-WASM v7 only supports one direct http.get/post/request import or one linked Module function import",
                );
            }
        }
        _ => {
            return fallback(
                "native Route-WASM v7 supports at most one direct or linked host-call import",
            )
        }
    }
    if !file.functions.is_empty() {
        return fallback("route helper functions are not WASM-native yet");
    }
    if file.methods.len() != 1 {
        return fallback("native route compilation currently requires exactly one HTTP method");
    }

    let method = &file.methods[0];
    let [Statement::Return(expr)] = method.body.as_slice() else {
        return fallback("native route body currently requires one literal return statement");
    };
    let (bytes, input) = if let Some(import) = host_import.as_ref() {
        let Some(args) = static_direct_call(&import.binding, expr) else {
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
        let (kind, target, operation, payload) =
            match lower_linked_module_capability_call(binding, linked, expr) {
                Ok(call) => call,
                Err(reason) => return fallback(reason),
            };
        (
            encode_capability_call_module(kind, &target, &operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some(value) = static_json(expr) {
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
    } else if returns_request_body(method.param_name.as_deref(), expr) {
        (encode_input_echo_module(), RouteWasmInput::JsonBody)
    } else {
        return fallback("route return value is outside the native Route-WASM v3 subset");
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
        input,
    })
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

fn lower_linked_module_capability_call(
    route_binding: &str,
    linked: &LinkedModuleFunction,
    route_expr: &Expr,
) -> Result<(ContainerCapabilityKind, String, String, Vec<u8>), String> {
    let route_args = static_direct_call(route_binding, route_expr).ok_or_else(|| {
        "native linked Module calls require the imported function as the return value with static JSON arguments".to_string()
    })?;
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

    let bindings = linked
        .function
        .params
        .iter()
        .cloned()
        .zip(route_args)
        .collect::<BTreeMap<_, _>>();
    let args = if host.kind == ContainerCapabilityKind::Storage && host.operation == "write" {
        lower_storage_write_args(host_args, &bindings)?
    } else {
        host_args
            .iter()
            .map(|argument| static_json_with_bindings(argument, &bindings))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "native linked Module host arguments must resolve to static JSON values".to_string()
            })?
    };
    let payload = serde_json::to_vec(&args)
        .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
    if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err("native linked Module argument payload exceeds the capability envelope".into());
    }
    Ok((host.kind, host.target, host.operation, payload))
}

fn lower_storage_write_args(
    args: &[Expr],
    bindings: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<serde_json::Value>, String> {
    if args.len() != 4 {
        return Err(
            "native storage.write requires encode[], data[], write[], and level[] descriptors"
                .into(),
        );
    }

    let mut descriptor_values = BTreeMap::<String, serde_json::Value>::new();
    for argument in args {
        let (kind, value_expr) = storage_descriptor_expr(argument).ok_or_else(|| {
            "native storage.write arguments must use descriptor brackets".to_string()
        })?;
        let value = static_json_with_bindings(value_expr, bindings).ok_or_else(|| {
            format!("native storage.write {kind}[] value must resolve to static JSON")
        })?;
        if descriptor_values.insert(kind.to_string(), value).is_some() {
            return Err(format!(
                "native storage.write descriptor {kind}[] was provided more than once"
            ));
        }
    }

    let take = |values: &mut BTreeMap<String, serde_json::Value>, name: &str| {
        values
            .remove(name)
            .ok_or_else(|| format!("native storage.write is missing {name}[]"))
    };
    let mut values = descriptor_values;
    let encoding = take(&mut values, "encode")?;
    let data = take(&mut values, "data")?;
    let path = take(&mut values, "write")?;
    let level = take(&mut values, "level")?;

    if !matches!(&path, serde_json::Value::String(value) if value.starts_with("$$/")) {
        return Err("native storage.write write[] must contain a symbolic $$/ path".into());
    }
    let level = level
        .as_f64()
        .filter(|value| value.fract() == 0.0 && (1.0..=3.0).contains(value))
        .ok_or_else(|| "native storage.write level[] must be 1, 2, or 3".to_string())?;
    let level = serde_json::Value::Number(serde_json::Number::from(level as u64));
    if !encoding.is_string() {
        return Err("native storage.write encode[] must be a string".into());
    }

    Ok(vec![serde_json::json!({
        "path": path,
        "data": data,
        "encoding": encoding,
        "level": level,
    })])
}

fn storage_descriptor_expr(expr: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Object(fields) = expr else {
        return None;
    };
    let descriptor = fields
        .iter()
        .find(|(name, _)| name == "__rbeStorageDescriptor")?;
    let value = fields.iter().find(|(name, _)| name == "value")?;
    let Expr::String(kind) = &descriptor.1 else {
        return None;
    };
    matches!(kind.as_str(), "encode" | "data" | "write" | "level")
        .then_some((kind.as_str(), &value.1))
}

fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {
    let Expr::Call(target, args) = expr else {
        return None;
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == binding) {
        return None;
    }
    args.iter().map(static_json).collect()
}

fn static_json_with_bindings(
    expr: &Expr,
    bindings: &BTreeMap<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    match expr {
        Expr::Ident(name) => bindings.get(name).cloned(),
        Expr::String(value) => Some(serde_json::Value::String(value.clone())),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(serde_json::Value::Number),
        Expr::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Array(values) => values
            .iter()
            .map(|value| static_json_with_bindings(value, bindings))
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Expr::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), static_json_with_bindings(value, bindings)?);
            }
            Some(serde_json::Value::Object(object))
        }
        Expr::Member(_, _) | Expr::Call(_, _) | Expr::UnaryNot(_) | Expr::Binary { .. } => None,
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

fn static_json(expr: &Expr) -> Option<serde_json::Value> {
    match expr {
        Expr::String(value) => Some(serde_json::Value::String(value.clone())),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(serde_json::Value::Number),
        Expr::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Array(values) => values
            .iter()
            .map(static_json)
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Expr::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), static_json(value)?);
            }
            Some(serde_json::Value::Object(object))
        }
        Expr::Ident(_)
        | Expr::Member(_, _)
        | Expr::Call(_, _)
        | Expr::UnaryNot(_)
        | Expr::Binary { .. } => None,
    }
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
    fn linked_module_storage_write_lowers_descriptors_for_trusted_boundary() {
        let module = parse_module(
            r#":import[storage.write as writeFile]
               export function save() {
                   return writeFile(
                       encode["UTF8"],
                       data[{ ok: true }],
                       write[$$/generated/data.json],
                       level[2]
                   );
               }"#,
        );
        let links = link_module_function("save", "accounts.cache", &module, "save");
        let route = parse(
            r#":import["./module/accounts/cache".save]
               class Route { get(req) { return save(); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("static linked Storage write wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "write");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!([{
                    "path": "$$/generated/data.json",
                    "data": {"ok": true},
                    "encoding": "UTF8",
                    "level": 2
                }])
            );
            Ok(br#"{\"path\":\"$$/generated/data.json\",\"bytes\":11}"#.to_vec())
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
        assert!(!result.output.is_empty());
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
    fn linked_module_dynamic_route_argument_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { post(req) { return lookup(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("dynamic linked arguments must remain interpreter-only");
        };
        assert!(reason.contains("static JSON arguments"));
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
        assert!(reason.contains(
            "one direct http.get/post/request import or one linked Module function import"
        ));
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
    fn multiple_methods_are_not_collapsed_into_one_run_export() {
        let route = parse("class Route { get(req) { return true; } post(req) { return false; } }");
        assert!(!compile_route(&route).is_native());
    }
}
