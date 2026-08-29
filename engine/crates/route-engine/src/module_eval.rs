//! Async evaluator for compiled `.module` programs.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value as JsonValue;
use service_runtime::ServiceManager;

use crate::ast::{BinaryOp, Expr, FunctionDef, ImportTarget, ModuleFile, Statement, Value};
use crate::module_runtime::ModuleProgram;
use crate::modules::{binding_name, ModuleRegistry};

const MAX_MODULE_CALL_DEPTH: usize = 64;

type EvalFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ModuleEvalError>> + Send + 'a>>;
type FlowFuture<'a> = Pin<Box<dyn Future<Output = Result<Flow, ModuleEvalError>> + Send + 'a>>;

pub type ServiceCallFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;

pub trait ServiceCaller: Send + Sync {
    fn call<'a>(
        &'a self,
        service: &'a str,
        function: &'a str,
        args: Vec<JsonValue>,
    ) -> ServiceCallFuture<'a>;
}

impl ServiceCaller for ServiceManager {
    fn call<'a>(
        &'a self,
        service: &'a str,
        function: &'a str,
        args: Vec<JsonValue>,
    ) -> ServiceCallFuture<'a> {
        Box::pin(async move {
            ServiceManager::call(self, service, function, args)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

pub type HostCapabilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Value>, ModuleEvalError>> + Send + 'a>>;

pub trait HostCapabilityCaller: Send + Sync {
    fn call<'a>(
        &'a self,
        scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a>;
}

#[derive(Debug, Clone)]
pub struct ModuleEvalError {
    pub code: &'static str,
    pub message: String,
}

impl ModuleEvalError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ModuleEvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ModuleEvalError {}

pub struct ModuleExecutor<'a> {
    program: &'a ModuleProgram,
    services: Option<Arc<dyn ServiceCaller>>,
    host_capabilities: Option<Arc<dyn HostCapabilityCaller>>,
}

impl<'a> ModuleExecutor<'a> {
    pub fn new(program: &'a ModuleProgram) -> Self {
        Self {
            program,
            services: None,
            host_capabilities: None,
        }
    }

    pub fn with_services(program: &'a ModuleProgram, services: ServiceManager) -> Self {
        Self::with_service_caller(program, Arc::new(services))
    }

    pub fn with_services_and_host_capabilities(
        program: &'a ModuleProgram,
        services: ServiceManager,
        host_capabilities: Arc<dyn HostCapabilityCaller>,
    ) -> Self {
        Self {
            program,
            services: Some(Arc::new(services)),
            host_capabilities: Some(host_capabilities),
        }
    }

    pub fn with_service_caller(
        program: &'a ModuleProgram,
        services: Arc<dyn ServiceCaller>,
    ) -> Self {
        Self {
            program,
            services: Some(services),
            host_capabilities: None,
        }
    }

    pub fn with_host_capabilities(
        program: &'a ModuleProgram,
        host_capabilities: Arc<dyn HostCapabilityCaller>,
    ) -> Self {
        Self {
            program,
            services: None,
            host_capabilities: Some(host_capabilities),
        }
    }

    async fn call_host_capability(
        &self,
        scope: Option<String>,
        module: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Option<Value>, ModuleEvalError> {
        let Some(host_capabilities) = self.host_capabilities.as_ref() else {
            return Ok(None);
        };
        host_capabilities.call(scope, module, function, args).await
    }

    pub async fn call(
        &self,
        raw_path: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ModuleEvalError> {
        self.call_export(raw_path, function, args, 0).await
    }

    pub(crate) async fn call_inline(
        &self,
        file: Arc<ModuleFile>,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ModuleEvalError> {
        self.call_function(file, function, args, 0, None).await
    }

    pub(crate) async fn call_inline_definition(
        &self,
        file: Arc<ModuleFile>,
        function: FunctionDef,
        args: Vec<Value>,
    ) -> Result<Value, ModuleEvalError> {
        self.execute_function(file, function, args, 0, None).await
    }

    async fn call_service(
        &self,
        service: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ModuleEvalError> {
        let services = self.services.as_ref().ok_or_else(|| {
            ModuleEvalError::new(
                "MOD3400",
                format!("service runtime is unavailable while calling {service}.{function}"),
            )
        })?;
        let args = args
            .into_iter()
            .map(value_to_json)
            .collect::<Result<Vec<_>, _>>()?;
        let value = services
            .call(service, function, args)
            .await
            .map_err(|message| {
                ModuleEvalError::new(
                    "MOD3401",
                    format!("service {service}.{function} failed: {message}"),
                )
            })?;
        value_from_json(value)
    }

    async fn call_export(
        &self,
        raw_path: &str,
        function: &str,
        args: Vec<Value>,
        depth: usize,
    ) -> Result<Value, ModuleEvalError> {
        let (owner, file) = self.program.resolve_scoped(raw_path).ok_or_else(|| {
            ModuleEvalError::new("MOD3000", format!("module {raw_path:?} is not loaded"))
        })?;
        if !file.exports.iter().any(|name| name == function) {
            return Err(ModuleEvalError::new(
                "MOD3001",
                format!("module {raw_path:?} does not export {function:?}"),
            ));
        }
        self.call_function(file, function, args, depth, Some(owner))
            .await
    }

    async fn call_function(
        &self,
        file: Arc<ModuleFile>,
        function_name: &str,
        args: Vec<Value>,
        depth: usize,
        capability_scope: Option<String>,
    ) -> Result<Value, ModuleEvalError> {
        if depth >= MAX_MODULE_CALL_DEPTH {
            return Err(ModuleEvalError::new(
                "MOD3004",
                format!("module call depth exceeded {MAX_MODULE_CALL_DEPTH}"),
            ));
        }
        let function = file
            .functions
            .iter()
            .find(|candidate| candidate.name == function_name)
            .cloned()
            .ok_or_else(|| {
                ModuleEvalError::new("MOD3002", format!("function {function_name:?} has no body"))
            })?;
        self.execute_function(file, function, args, depth, capability_scope)
            .await
    }

    async fn execute_function(
        &self,
        file: Arc<ModuleFile>,
        function: FunctionDef,
        args: Vec<Value>,
        depth: usize,
        capability_scope: Option<String>,
    ) -> Result<Value, ModuleEvalError> {
        if function.params.len() != args.len() {
            return Err(ModuleEvalError::new(
                "MOD3003",
                format!(
                    "function {:?} expected {} argument(s), got {}",
                    function.name,
                    function.params.len(),
                    args.len()
                ),
            ));
        }

        let mut frame = Frame::new(self, file, depth, capability_scope);
        for (param, value) in function.params.iter().zip(args) {
            frame.scope.insert(param.clone(), value);
        }
        match frame.exec_block(&function.body).await? {
            Flow::Continue => Ok(Value::Null),
            Flow::Return(value) => Ok(value),
        }
    }
}

impl ModuleProgram {
    pub async fn call(
        &self,
        raw_path: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, ModuleEvalError> {
        ModuleExecutor::new(self)
            .call(raw_path, function, args)
            .await
    }
}

enum Flow {
    Continue,
    Return(Value),
}

struct Frame<'exec, 'program> {
    executor: &'exec ModuleExecutor<'program>,
    file: Arc<ModuleFile>,
    modules: ModuleRegistry,
    builtin_modules: HashMap<String, String>,
    builtin_functions: HashMap<String, (String, String)>,
    custom_modules: HashMap<String, String>,
    custom_functions: HashMap<String, (String, String)>,
    service_modules: HashMap<String, String>,
    service_functions: HashMap<String, (String, String)>,
    scope: HashMap<String, Value>,
    depth: usize,
    capability_scope: Option<String>,
}

impl<'exec, 'program> Frame<'exec, 'program> {
    fn new(
        executor: &'exec ModuleExecutor<'program>,
        file: Arc<ModuleFile>,
        depth: usize,
        capability_scope: Option<String>,
    ) -> Self {
        let modules = ModuleRegistry::from_imports(&file.imports);
        let mut builtin_modules = HashMap::new();
        let mut builtin_functions = HashMap::new();
        let mut custom_modules = HashMap::new();
        let mut custom_functions = HashMap::new();
        let mut service_modules = HashMap::new();
        let mut service_functions = HashMap::new();
        for import in &file.imports {
            let binding = binding_name(import);
            match import_base(import) {
                ImportTarget::Builtin(module) => {
                    builtin_modules.insert(binding, module.clone());
                }
                ImportTarget::BuiltinFunction { module, function } => {
                    builtin_functions.insert(binding, (module.clone(), function.clone()));
                }
                ImportTarget::Custom(path) => {
                    custom_modules.insert(binding, path.clone());
                }
                ImportTarget::CustomFunction { path, function } => {
                    custom_functions.insert(binding, (path.clone(), function.clone()));
                }
                ImportTarget::Service(service) => {
                    service_modules.insert(binding, service.clone());
                }
                ImportTarget::ServiceFunction { service, function } => {
                    service_functions.insert(binding, (service.clone(), function.clone()));
                }
                ImportTarget::Aliased { .. } => unreachable!("import_base removes aliases"),
            }
        }
        Self {
            executor,
            file,
            modules,
            builtin_modules,
            builtin_functions,
            custom_modules,
            custom_functions,
            service_modules,
            service_functions,
            scope: HashMap::new(),
            depth,
            capability_scope,
        }
    }

    fn exec_block<'b>(&'b mut self, body: &'b [Statement]) -> FlowFuture<'b> {
        Box::pin(async move {
            for statement in body {
                match statement {
                    Statement::Const { name, value } => {
                        let value = self.eval(value).await?;
                        self.scope.insert(name.clone(), value);
                    }
                    Statement::Return(expr) => {
                        return Ok(Flow::Return(self.eval(expr).await?));
                    }
                    Statement::Expr(expr) => {
                        self.eval(expr).await?;
                    }
                    Statement::If {
                        condition,
                        then_body,
                        else_body,
                    } => {
                        let branch = if self.eval(condition).await?.truthy() {
                            then_body
                        } else {
                            else_body
                        };
                        match self.exec_block(branch).await? {
                            Flow::Continue => {}
                            flow @ Flow::Return(_) => return Ok(flow),
                        }
                    }
                }
            }
            Ok(Flow::Continue)
        })
    }

    fn eval<'b>(&'b mut self, expr: &'b Expr) -> EvalFuture<'b> {
        Box::pin(async move {
            match expr {
                Expr::String(value) => Ok(Value::String(value.clone())),
                Expr::Number(value) => Ok(Value::Number(*value)),
                Expr::Bool(value) => Ok(Value::Bool(*value)),
                Expr::Null => Ok(Value::Null),
                Expr::Ident(name) => {
                    if let Some(value) = self.scope.get(name) {
                        return Ok(value.clone());
                    }
                    if self
                        .file
                        .functions
                        .iter()
                        .any(|function| function.name == *name)
                    {
                        return Err(ModuleEvalError::new(
                            "MOD3100",
                            format!("function {name:?} must be called, not used as a value"),
                        ));
                    }
                    if self.is_import_binding(name) {
                        return Err(ModuleEvalError::new(
                            "MOD3101",
                            format!("import {name:?} must be called, not used as a value"),
                        ));
                    }
                    Err(ModuleEvalError::new(
                        "MOD3102",
                        format!("{name:?} is not defined"),
                    ))
                }
                Expr::Object(fields) => {
                    let mut out = HashMap::new();
                    for (key, value) in fields {
                        out.insert(key.clone(), self.eval(value).await?);
                    }
                    Ok(Value::Object(out))
                }
                Expr::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(self.eval(item).await?);
                    }
                    Ok(Value::Array(out))
                }
                Expr::Member(base, field) => {
                    if let Expr::Ident(name) = base.as_ref() {
                        if self.is_import_binding(name) {
                            return Err(ModuleEvalError::new(
                                "MOD3103",
                                format!("{name}.{field} must be called"),
                            ));
                        }
                    }
                    match self.eval(base).await? {
                        Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
                        other => Err(ModuleEvalError::new(
                            "MOD3104",
                            format!("cannot access .{field} on {other:?}"),
                        )),
                    }
                }
                Expr::Call(callee, arg_exprs) => {
                    let mut args = Vec::with_capacity(arg_exprs.len());
                    for arg in arg_exprs {
                        args.push(self.eval(arg).await?);
                    }

                    if let Expr::Ident(name) = callee.as_ref() {
                        if let Some((module, function)) = self.builtin_functions.get(name).cloned()
                        {
                            if let Some(value) = self
                                .executor
                                .call_host_capability(
                                    self.capability_scope.clone(),
                                    &module,
                                    &function,
                                    args.clone(),
                                )
                                .await?
                            {
                                return Ok(value);
                            }
                        }

                        if let Some((service, function)) = self.service_functions.get(name).cloned()
                        {
                            return self.executor.call_service(&service, &function, args).await;
                        }
                        if let Some((path, function)) = self.custom_functions.get(name).cloned() {
                            return self
                                .executor
                                .call_export(&path, &function, args, self.depth + 1)
                                .await;
                        }
                        if let Some(function) = self
                            .file
                            .functions
                            .iter()
                            .find(|candidate| candidate.name == *name)
                            .cloned()
                        {
                            return self
                                .executor
                                .call_function(
                                    self.file.clone(),
                                    &function.name,
                                    args,
                                    self.depth + 1,
                                    self.capability_scope.clone(),
                                )
                                .await;
                        }
                        if self.modules.is_direct_function(name) {
                            return self.modules.call_direct(name, &args).map_err(|error| {
                                ModuleEvalError::new("MOD3200", error.to_string())
                            });
                        }
                    }

                    if let Expr::Member(base, function_name) = callee.as_ref() {
                        if let Expr::Ident(module_name) = base.as_ref() {
                            if let Some(module) = self.builtin_modules.get(module_name).cloned() {
                                if let Some(value) = self
                                    .executor
                                    .call_host_capability(
                                        self.capability_scope.clone(),
                                        &module,
                                        function_name,
                                        args.clone(),
                                    )
                                    .await?
                                {
                                    return Ok(value);
                                }
                            }

                            if let Some(service) = self.service_modules.get(module_name).cloned() {
                                return self
                                    .executor
                                    .call_service(&service, function_name, args)
                                    .await;
                            }
                            if let Some(path) = self.custom_modules.get(module_name).cloned() {
                                return self
                                    .executor
                                    .call_export(&path, function_name, args, self.depth + 1)
                                    .await;
                            }
                            if self.is_import_binding(module_name) {
                                return self
                                    .modules
                                    .call(module_name, function_name, &args)
                                    .map_err(|error| {
                                        ModuleEvalError::new("MOD3200", error.to_string())
                                    });
                            }
                        }
                    }

                    Err(ModuleEvalError::new(
                        "MOD3201",
                        "unsupported module function call",
                    ))
                }
                Expr::UnaryNot(expr) => Ok(Value::Bool(!self.eval(expr).await?.truthy())),
                Expr::Binary { left, op, right } => {
                    if matches!(op, BinaryOp::And) {
                        let left = self.eval(left).await?;
                        if !left.truthy() {
                            return Ok(Value::Bool(false));
                        }
                        return Ok(Value::Bool(self.eval(right).await?.truthy()));
                    }
                    if matches!(op, BinaryOp::Or) {
                        let left = self.eval(left).await?;
                        if left.truthy() {
                            return Ok(Value::Bool(true));
                        }
                        return Ok(Value::Bool(self.eval(right).await?.truthy()));
                    }
                    let left = self.eval(left).await?;
                    let right = self.eval(right).await?;
                    binary(op, left, right)
                }
            }
        })
    }

    fn is_import_binding(&self, name: &str) -> bool {
        self.file
            .imports
            .iter()
            .any(|import| binding_name(import) == name)
    }
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => target.as_ref(),
        other => other,
    }
}

fn value_to_json(value: Value) -> Result<JsonValue, ModuleEvalError> {
    match value {
        Value::String(value) => Ok(JsonValue::String(value)),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                ModuleEvalError::new(
                    "MOD3402",
                    "non-finite numbers cannot cross the service IPC boundary",
                )
            }),
        Value::Bool(value) => Ok(JsonValue::Bool(value)),
        Value::Null => Ok(JsonValue::Null),
        Value::Object(fields) => {
            let mut out = serde_json::Map::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, value_to_json(value)?);
            }
            Ok(JsonValue::Object(out))
        }
        Value::Array(items) => items
            .into_iter()
            .map(value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
    }
}

fn value_from_json(value: JsonValue) -> Result<Value, ModuleEvalError> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Bool(value)),
        JsonValue::Number(value) => value.as_f64().map(Value::Number).ok_or_else(|| {
            ModuleEvalError::new(
                "MOD3403",
                "service returned a JSON number that cannot be represented as f64",
            )
        }),
        JsonValue::String(value) => Ok(Value::String(value)),
        JsonValue::Array(items) => items
            .into_iter()
            .map(value_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        JsonValue::Object(fields) => {
            let mut out = HashMap::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, value_from_json(value)?);
            }
            Ok(Value::Object(out))
        }
    }
}

fn binary(op: &BinaryOp, left: Value, right: Value) -> Result<Value, ModuleEvalError> {
    match op {
        BinaryOp::Equal | BinaryOp::StrictEqual => Ok(Value::Bool(value_eq(&left, &right))),
        BinaryOp::NotEqual | BinaryOp::StrictNotEqual => Ok(Value::Bool(!value_eq(&left, &right))),
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            match (&left, &right) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Bool(match op {
                    BinaryOp::Less => a < b,
                    BinaryOp::LessEqual => a <= b,
                    BinaryOp::Greater => a > b,
                    BinaryOp::GreaterEqual => a >= b,
                    _ => unreachable!(),
                })),
                (Value::String(a), Value::String(b)) => {
                    let order = a.cmp(b);
                    Ok(Value::Bool(match op {
                        BinaryOp::Less => order.is_lt(),
                        BinaryOp::LessEqual => order.is_le(),
                        BinaryOp::Greater => order.is_gt(),
                        BinaryOp::GreaterEqual => order.is_ge(),
                        _ => unreachable!(),
                    }))
                }
                _ => Err(ModuleEvalError::new(
                    "MOD3300",
                    "comparison requires matching numbers or strings",
                )),
            }
        }
        BinaryOp::Add => match (left, right) {
            (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
            (Value::String(a), Value::String(b)) => Ok(Value::String(a + &b)),
            (a, b) => Ok(Value::String(format!("{a:?}{b:?}"))),
        },
        BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo => {
            let (a, b) = match (left, right) {
                (Value::Number(a), Value::Number(b)) => (a, b),
                _ => {
                    return Err(ModuleEvalError::new(
                        "MOD3301",
                        "arithmetic requires numbers",
                    ));
                }
            };
            Ok(Value::Number(match op {
                BinaryOp::Subtract => a - b,
                BinaryOp::Multiply => a * b,
                BinaryOp::Divide => a / b,
                BinaryOp::Modulo => a % b,
                _ => unreachable!(),
            }))
        }
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

fn value_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| value_eq(value, other)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.iter().zip(b).all(|(left, right)| value_eq(left, right)) && a.len() == b.len()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn root() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-module-eval-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("module")).unwrap();
        path
    }

    #[test]
    fn executes_helpers_and_module_dependencies() {
        let root = root();
        fs::write(
            root.join("module/b.module"),
            "export function twice(value) { return value * 2; }",
        )
        .unwrap();
        fs::write(
            root.join("module/a.module"),
            ":import[module&b]\nfunction plusOne(value) { return value + 1; }\nexport function run(value) { return b.twice(plusOne(value)); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let value =
            block_on_ready(program.call("./module/a", "run", vec![Value::Number(3.0)])).unwrap();
        assert!(matches!(value, Value::Number(value) if value == 8.0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn private_functions_cannot_be_called_from_outside() {
        let root = root();
        fs::write(
            root.join("module/a.module"),
            "function hidden() { return 7; }\nexport function visible() { return hidden(); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let error = block_on_ready(program.call("./module/a", "hidden", Vec::new())).unwrap_err();
        assert_eq!(error.code, "MOD3001");
        let visible = block_on_ready(program.call("./module/a", "visible", Vec::new())).unwrap();
        assert!(matches!(visible, Value::Number(value) if value == 7.0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recursive_helpers_stop_at_depth_limit() {
        let root = root();
        fs::write(
            root.join("module/a.module"),
            "function forever() { return forever(); }\nexport function run() { return forever(); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let error = block_on_ready(program.call("./module/a", "run", Vec::new())).unwrap_err();
        assert_eq!(error.code, "MOD3004");
        let _ = fs::remove_dir_all(root);
    }

    #[derive(Default)]
    struct FakeServices {
        calls: std::sync::Mutex<Vec<(String, String, Vec<JsonValue>)>>,
    }

    impl ServiceCaller for FakeServices {
        fn call<'a>(
            &'a self,
            service: &'a str,
            function: &'a str,
            args: Vec<JsonValue>,
        ) -> ServiceCallFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((
                    service.to_string(),
                    function.to_string(),
                    args.clone(),
                ));
                match (service, function) {
                    ("uac-cache", "get") => Ok(args.into_iter().next().unwrap_or(JsonValue::Null)),
                    ("search", "find") => Ok(serde_json::json!({
                        "found": args.into_iter().next().unwrap_or(JsonValue::Null)
                    })),
                    _ => Err(format!("unexpected fake service call {service}.{function}")),
                }
            })
        }
    }

    #[test]
    fn module_dispatches_namespace_and_direct_service_calls() {
        let root = root();
        fs::write(
            root.join("module/services.module"),
            ":import[service:uac-cache as cache, service:search.find as lookup]\nexport function run(id) { return lookup(cache.get(id)); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let caller = Arc::new(FakeServices::default());
        let executor = ModuleExecutor::with_service_caller(&program, caller.clone());
        let value = block_on_ready(executor.call(
            "./module/services",
            "run",
            vec![Value::String("abc".into())],
        ))
        .unwrap();
        let Value::Object(fields) = value else {
            panic!("expected object service response");
        };
        assert!(matches!(fields.get("found"), Some(Value::String(value)) if value == "abc"));
        assert_eq!(caller.calls.lock().unwrap().len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn service_call_without_runtime_is_explicit() {
        let root = root();
        fs::write(
            root.join("module/services.module"),
            ":import[service:uac-cache as cache]\nexport function run(id) { return cache.get(id); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let error = block_on_ready(program.call(
            "./module/services",
            "run",
            vec![Value::String("abc".into())],
        ))
        .unwrap_err();
        assert_eq!(error.code, "MOD3400");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inline_route_can_reach_module_service_chain() {
        let root = root();
        fs::write(
            root.join("module/bridge.module"),
            ":import[service:uac-cache as cache]\nexport function lookup(id) { return cache.get(id); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let caller = Arc::new(FakeServices::default());
        let executor = ModuleExecutor::with_service_caller(&program, caller.clone());
        let route = Arc::new(ModuleFile {
            imports: vec![ImportTarget::Custom("./module/bridge".into())],
            functions: vec![crate::ast::FunctionDef {
                name: "\0route-test".into(),
                params: vec!["req".into()],
                body: vec![Statement::Return(Expr::Call(
                    Box::new(Expr::Member(
                        Box::new(Expr::Ident("bridge".into())),
                        "lookup".into(),
                    )),
                    vec![Expr::Member(
                        Box::new(Expr::Ident("req".into())),
                        "id".into(),
                    )],
                ))],
            }],
            exports: Vec::new(),
        });
        let mut request = HashMap::new();
        request.insert("id".into(), Value::String("route-id".into()));
        let value = block_on_ready(executor.call_inline(
            route,
            "\0route-test",
            vec![Value::Object(request)],
        ))
        .unwrap();
        assert!(matches!(value, Value::String(value) if value == "route-id"));
        assert_eq!(caller.calls.lock().unwrap().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    struct TestHostCapability;

    impl HostCapabilityCaller for TestHostCapability {
        fn call<'a>(
            &'a self,
            scope: Option<String>,
            module: &'a str,
            function: &'a str,
            args: Vec<Value>,
        ) -> HostCapabilityFuture<'a> {
            Box::pin(async move {
                if module == "host" && function == "double" {
                    if scope.as_deref() != Some("hosted") {
                        return Err(ModuleEvalError::new(
                            "TEST",
                            "unexpected module capability scope",
                        ));
                    }
                    let Some(Value::Number(value)) = args.into_iter().next() else {
                        return Err(ModuleEvalError::new("TEST", "expected numeric argument"));
                    };
                    Ok(Some(Value::Number(value * 2.0)))
                } else {
                    Ok(None)
                }
            })
        }
    }

    #[test]
    fn host_capability_can_override_imported_builtin() {
        let root = root();
        fs::write(
            root.join("module/hosted.module"),
            ":import[host.double as twice]\nexport function run(value) { return twice(value); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let executor =
            ModuleExecutor::with_host_capabilities(&program, Arc::new(TestHostCapability));
        let value =
            block_on_ready(executor.call("./module/hosted", "run", vec![Value::Number(6.0)]))
                .unwrap();
        assert!(matches!(value, Value::Number(value) if value == 12.0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn host_capability_is_not_global_without_import() {
        let root = root();
        fs::write(
            root.join("module/unimported.module"),
            "export function run(value) { return host.double(value); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let executor =
            ModuleExecutor::with_host_capabilities(&program, Arc::new(TestHostCapability));
        let error =
            block_on_ready(executor.call("./module/unimported", "run", vec![Value::Number(6.0)]))
                .expect_err("host capability must require an explicit import");
        assert_eq!(error.code, "MOD3201");
        let _ = fs::remove_dir_all(root);
    }
}
