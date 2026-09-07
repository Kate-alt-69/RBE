//! Executable `.service` program adapter built on the shared async module VM.
//!
//! Service exports use the same expression/control-flow evaluator as `.module`
//! files. Host-only capabilities (`memory` and `quickDB`) are injected
//! explicitly instead of becoming ambient powers of the language runtime.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use service_runtime::{
    ServiceExecutionError, ServiceExecutionFuture, ServiceExecutor, ServiceLifecycle,
    ServiceLifecycleFuture, ServiceMemory,
};

use crate::ast::{FunctionDef, MethodDef, ModuleFile, ServiceProgram, Value};
use crate::module_eval::{
    HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError, ModuleExecutor,
};
use crate::module_runtime::ModuleProgram;

#[path = "quickdb.rs"]
mod quickdb;
use quickdb::{FilterConfig, FilterKind, FilterStats, QuickDb, QuickDbError};

#[derive(Clone)]
struct ServiceHostCapabilities {
    memory: ServiceMemory,
    quick_db: QuickDb,
}

impl ServiceHostCapabilities {
    fn new(memory: ServiceMemory) -> Self {
        Self {
            memory,
            quick_db: QuickDb::default(),
        }
    }

    fn call_memory(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        let module = "memory";
        match function {
            "get" => {
                expect_arity(module, function, args, 1)?;
                let key = expect_string(module, function, &args[0], 0)?;
                Ok(self
                    .memory
                    .get(key)
                    .map(json_to_value)
                    .transpose()?
                    .unwrap_or(Value::Null))
            }
            "set" => {
                expect_arity(module, function, args, 2)?;
                let key = expect_string(module, function, &args[0], 0)?.to_string();
                let value = value_to_json(args[1].clone())?;
                self.memory.set(key, value);
                Ok(Value::Bool(true))
            }
            "delete" => {
                expect_arity(module, function, args, 1)?;
                let key = expect_string(module, function, &args[0], 0)?;
                Ok(Value::Bool(self.memory.delete(key)))
            }
            "clear" => {
                expect_arity(module, function, args, 0)?;
                self.memory.clear();
                Ok(Value::Bool(true))
            }
            "len" => {
                expect_arity(module, function, args, 0)?;
                Ok(Value::Number(self.memory.len() as f64))
            }
            "isEmpty" | "is_empty" => {
                expect_arity(module, function, args, 0)?;
                Ok(Value::Bool(self.memory.is_empty()))
            }
            other => Err(eval_error(
                "SVC4201",
                format!("unknown service host capability memory.{other}()"),
            )),
        }
    }

    fn call_quick_db(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        const MODULE: &str = "quickDB";
        match function {
            "create" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let options = expect_object(MODULE, function, &args[1], 1)?;
                let kind = options
                    .get("kind")
                    .map(|value| expect_string(MODULE, function, value, 1))
                    .transpose()?
                    .map(FilterKind::parse)
                    .transpose()
                    .map_err(quick_db_error)?
                    .unwrap_or(FilterKind::Bloom);
                let capacity = options
                    .get("capacity")
                    .ok_or_else(|| {
                        eval_error(
                            "SVC4212",
                            "quickDB.create() options.capacity is required",
                        )
                    })
                    .and_then(|value| expect_usize(MODULE, function, value, "capacity"))?;
                let false_positive_rate = options
                    .get("falsePositiveRate")
                    .or_else(|| options.get("false_positive_rate"))
                    .map(|value| {
                        expect_probability(
                            MODULE,
                            function,
                            value,
                            "falsePositiveRate",
                        )
                    })
                    .transpose()?
                    .unwrap_or(0.01);

                self.quick_db
                    .create(
                        name,
                        FilterConfig {
                            kind,
                            capacity,
                            false_positive_rate,
                        },
                    )
                    .map_err(quick_db_error)?;
                self.quick_db
                    .stats(name)
                    .map(|stats| quick_db_stats_value(stats, false))
                    .map_err(quick_db_error)
            }
            "add" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let value = expect_string(MODULE, function, &args[1], 1)?;
                self.quick_db.add(name, value).map_err(quick_db_error)?;
                Ok(Value::Bool(true))
            }
            "addMany" | "add_many" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let values = expect_array(MODULE, function, &args[1], 1)?;
                let mut strings = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    strings.push(expect_string(MODULE, function, value, index + 1)?);
                }
                self.quick_db
                    .add_many(name, strings)
                    .map(|added| Value::Number(added as f64))
                    .map_err(quick_db_error)
            }
            "seal" => {
                expect_arity(MODULE, function, args, 1)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                self.quick_db.seal(name).map_err(quick_db_error)?;
                Ok(Value::Bool(true))
            }
            "isReady" | "is_ready" => {
                expect_arity(MODULE, function, args, 1)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                self.quick_db
                    .is_ready(name)
                    .map(Value::Bool)
                    .map_err(quick_db_error)
            }
            "mightContain" | "might_contain" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let value = expect_string(MODULE, function, &args[1], 1)?;
                self.quick_db
                    .might_contain(name, value)
                    .map(Value::Bool)
                    .map_err(quick_db_error)
            }
            "definitelyMissing" | "definitely_missing" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let value = expect_string(MODULE, function, &args[1], 1)?;
                self.quick_db
                    .definitely_missing(name, value)
                    .map(Value::Bool)
                    .map_err(quick_db_error)
            }
            "remove" => {
                expect_arity(MODULE, function, args, 2)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let value = expect_string(MODULE, function, &args[1], 1)?;
                self.quick_db
                    .remove(name, value)
                    .map(Value::Bool)
                    .map_err(quick_db_error)
            }
            "clear" => {
                expect_arity(MODULE, function, args, 1)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                self.quick_db.clear(name).map_err(quick_db_error)?;
                Ok(Value::Bool(true))
            }
            "drop" | "dropFilter" | "drop_filter" => {
                expect_arity(MODULE, function, args, 1)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                self.quick_db
                    .drop_filter(name)
                    .map(Value::Bool)
                    .map_err(quick_db_error)
            }
            "stats" => {
                expect_arity(MODULE, function, args, 1)?;
                let name = expect_string(MODULE, function, &args[0], 0)?;
                let stats = self.quick_db.stats(name).map_err(quick_db_error)?;
                let ready = self.quick_db.is_ready(name).map_err(quick_db_error)?;
                Ok(quick_db_stats_value(stats, ready))
            }
            "len" => {
                expect_arity(MODULE, function, args, 0)?;
                self.quick_db
                    .len()
                    .map(|len| Value::Number(len as f64))
                    .map_err(quick_db_error)
            }
            other => Err(eval_error(
                "SVC4210",
                format!("unknown service host capability quickDB.{other}()"),
            )),
        }
    }
}

impl HostCapabilityCaller for ServiceHostCapabilities {
    fn call<'a>(
        &'a self,
        _scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            let value = match module {
                "memory" => self.call_memory(function, &args)?,
                "quickDB" => self.call_quick_db(function, &args)?,
                _ => return Ok(None),
            };
            Ok(Some(value))
        })
    }
}

/// Executes one parsed `.service` program while keeping service-specific host
/// powers explicit. The executable function bodies themselves are evaluated by
/// the same VM used by `.module` files.
pub struct ServiceProgramExecutor {
    modules: ModuleProgram,
    file: Arc<ModuleFile>,
    lifecycle: Vec<MethodDef>,
    host_capabilities: Arc<dyn HostCapabilityCaller>,
}

impl ServiceProgramExecutor {
    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        let ServiceProgram {
            imports,
            functions,
            exports,
            lifecycle,
            ..
        } = program;
        let file = Arc::new(ModuleFile {
            imports,
            functions,
            exports,
        });
        Self {
            modules,
            file,
            lifecycle,
            host_capabilities: Arc::new(ServiceHostCapabilities::new(memory)),
        }
    }

    fn lifecycle_method(&self, phase: ServiceLifecycle) -> Option<MethodDef> {
        self.lifecycle
            .iter()
            .find(|method| method.verb == phase.as_str())
            .cloned()
    }
}

impl ServiceExecutor for ServiceProgramExecutor {
    fn call<'a>(&'a self, function: &'a str, args: Vec<JsonValue>) -> ServiceExecutionFuture<'a> {
        Box::pin(async move {
            let args = args
                .into_iter()
                .map(json_to_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(service_error)?;
            let executor = ModuleExecutor::with_host_capabilities(
                &self.modules,
                self.host_capabilities.clone(),
            );
            let value = executor
                .call_inline(self.file.clone(), function, args)
                .await
                .map_err(service_error)?;
            value_to_json(value).map_err(service_error)
        })
    }

    fn lifecycle<'a>(
        &'a self,
        phase: ServiceLifecycle,
        argument: JsonValue,
    ) -> ServiceLifecycleFuture<'a> {
        Box::pin(async move {
            let Some(method) = self.lifecycle_method(phase) else {
                return Ok(None);
            };
            let args = if method.param_name.is_some() {
                vec![json_to_value(argument).map_err(service_error)?]
            } else {
                Vec::new()
            };
            let function = FunctionDef {
                name: format!("Service.{}", phase.as_str()),
                params: method.param_name.into_iter().collect(),
                body: method.body,
            };
            let executor = ModuleExecutor::with_host_capabilities(
                &self.modules,
                self.host_capabilities.clone(),
            );
            let value = executor
                .call_inline_definition(self.file.clone(), function, args)
                .await
                .map_err(service_error)?;
            value_to_json(value).map(Some).map_err(service_error)
        })
    }
}

fn expect_arity(
    module: &str,
    function: &str,
    args: &[Value],
    expected: usize,
) -> Result<(), ModuleEvalError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(eval_error(
            "SVC4202",
            format!(
                "{module}.{function}() expects {expected} argument(s), got {}",
                args.len()
            ),
        ))
    }
}

fn expect_string<'a>(
    module: &str,
    function: &str,
    value: &'a Value,
    index: usize,
) -> Result<&'a str, ModuleEvalError> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(eval_error(
            "SVC4203",
            format!("{module}.{function}() argument {index} must be a string"),
        )),
    }
}

fn expect_object<'a>(
    module: &str,
    function: &str,
    value: &'a Value,
    index: usize,
) -> Result<&'a std::collections::HashMap<String, Value>, ModuleEvalError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(eval_error(
            "SVC4212",
            format!("{module}.{function}() argument {index} must be an object"),
        )),
    }
}

fn expect_array<'a>(
    module: &str,
    function: &str,
    value: &'a Value,
    index: usize,
) -> Result<&'a [Value], ModuleEvalError> {
    match value {
        Value::Array(value) => Ok(value),
        _ => Err(eval_error(
            "SVC4212",
            format!("{module}.{function}() argument {index} must be an array"),
        )),
    }
}

fn expect_usize(
    module: &str,
    function: &str,
    value: &Value,
    field: &str,
) -> Result<usize, ModuleEvalError> {
    let Value::Number(value) = value else {
        return Err(eval_error(
            "SVC4212",
            format!("{module}.{function}() options.{field} must be a number"),
        ));
    };
    if !value.is_finite()
        || *value <= 0.0
        || value.fract() != 0.0
        || *value > usize::MAX as f64
    {
        return Err(eval_error(
            "SVC4212",
            format!(
                "{module}.{function}() options.{field} must be a positive integer within this platform's address space"
            ),
        ));
    }
    Ok(*value as usize)
}

fn expect_probability(
    module: &str,
    function: &str,
    value: &Value,
    field: &str,
) -> Result<f64, ModuleEvalError> {
    let Value::Number(value) = value else {
        return Err(eval_error(
            "SVC4212",
            format!("{module}.{function}() options.{field} must be a number"),
        ));
    };
    if !value.is_finite() || *value <= 0.0 || *value >= 1.0 {
        return Err(eval_error(
            "SVC4212",
            format!("{module}.{function}() options.{field} must be greater than 0 and less than 1"),
        ));
    }
    Ok(*value)
}

fn quick_db_stats_value(stats: FilterStats, ready: bool) -> Value {
    let mut fields = std::collections::HashMap::new();
    fields.insert(
        "kind".to_string(),
        Value::String(stats.kind.as_str().to_string()),
    );
    fields.insert("capacity".to_string(), Value::Number(stats.capacity as f64));
    fields.insert("writes".to_string(), Value::Number(stats.writes as f64));
    fields.insert(
        "bitSlots".to_string(),
        Value::Number(stats.bit_slots as f64),
    );
    fields.insert(
        "allocatedBytes".to_string(),
        Value::Number(stats.allocated_bytes as f64),
    );
    fields.insert(
        "hashFunctions".to_string(),
        Value::Number(stats.hash_functions as f64),
    );
    fields.insert("layers".to_string(), Value::Number(stats.layers as f64));
    fields.insert(
        "falsePositiveRate".to_string(),
        Value::Number(stats.target_false_positive_rate),
    );
    fields.insert("ready".to_string(), Value::Bool(ready));
    Value::Object(fields)
}

fn quick_db_error(error: QuickDbError) -> ModuleEvalError {
    eval_error("SVC4213", error.to_string())
}

fn service_error(error: ModuleEvalError) -> ServiceExecutionError {
    ServiceExecutionError::new(error.code, error.message)
}

fn eval_error(code: &'static str, message: impl Into<String>) -> ModuleEvalError {
    ModuleEvalError {
        code,
        message: message.into(),
    }
}

fn value_to_json(value: Value) -> Result<JsonValue, ModuleEvalError> {
    match value {
        Value::String(value) => Ok(JsonValue::String(value)),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                eval_error(
                    "SVC4204",
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

fn json_to_value(value: JsonValue) -> Result<Value, ModuleEvalError> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Bool(value)),
        JsonValue::Number(value) => value.as_f64().map(Value::Number).ok_or_else(|| {
            eval_error(
                "SVC4205",
                "service IPC number cannot be represented by the RBE numeric type",
            )
        }),
        JsonValue::String(value) => Ok(Value::String(value)),
        JsonValue::Array(items) => items
            .into_iter()
            .map(json_to_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        JsonValue::Object(fields) => {
            let mut out = std::collections::HashMap::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, json_to_value(value)?);
            }
            Ok(Value::Object(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn modules_for_test(name: &str) -> ModuleProgram {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-{name}-test-{}-{nonce}",
            std::process::id()
        ));
        ModuleProgram::load(&root.join("module")).expect("module load failed")
    }

    #[test]
    fn executes_service_exports_with_shared_memory() {
        let source = r#"
            :import[memory]
            :service[name = cache]
            export function remember(key, value) {
                memory.set(key, value);
                return memory.get(key);
            }
        "#;
        let program = crate::parse_service_source(source).expect("service parse failed");
        let modules = modules_for_test("service-eval");
        let memory = ServiceMemory::default();
        let observer = memory.clone();
        let executor = ServiceProgramExecutor::new(program, modules, memory);
        let value = block_on_ready(ServiceExecutor::call(
            &executor,
            "remember",
            vec![serde_json::json!("answer"), serde_json::json!(42)],
        ))
        .expect("service execution failed");
        assert_eq!(value, serde_json::json!(42.0));
        assert_eq!(observer.get("answer"), Some(serde_json::json!(42.0)));
    }

    #[test]
    fn executes_quick_db_membership_capability() {
        let source = r#"
            :import[quickDB]
            :service[name = user-index]
            export function setup() {
                quickDB.create("users", {
                    kind: "bloom",
                    capacity: 10000,
                    falsePositiveRate: 0.01
                });
                quickDB.add("users", "kate");
                quickDB.seal("users");
                return quickDB.mightContain("users", "kate");
            }
            export function ready() {
                return quickDB.isReady("users");
            }
        "#;
        let program = crate::parse_service_source(source).expect("service parse failed");
        let modules = modules_for_test("quick-db");
        let executor =
            ServiceProgramExecutor::new(program, modules, ServiceMemory::default());

        let contains = block_on_ready(ServiceExecutor::call(&executor, "setup", vec![]))
            .expect("quickDB setup failed");
        assert_eq!(contains, serde_json::json!(true));

        let ready = block_on_ready(ServiceExecutor::call(&executor, "ready", vec![]))
            .expect("quickDB ready check failed");
        assert_eq!(ready, serde_json::json!(true));
    }

    #[test]
    fn quick_db_rejects_membership_before_seal() {
        let source = r#"
            :import[quickDB]
            :service[name = user-index]
            export function unsafeCheck() {
                quickDB.create("users", { capacity: 1000 });
                quickDB.add("users", "kate");
                return quickDB.mightContain("users", "kate");
            }
        "#;
        let program = crate::parse_service_source(source).expect("service parse failed");
        let modules = modules_for_test("quick-db-unsealed");
        let executor =
            ServiceProgramExecutor::new(program, modules, ServiceMemory::default());

        let error = block_on_ready(ServiceExecutor::call(&executor, "unsafeCheck", vec![]))
            .expect_err("unsealed quickDB membership must fail");
        assert!(error.to_string().contains("quickDB.seal"));
    }

    #[test]
    fn executes_service_lifecycle_with_shared_memory() {
        let source = r#"
            :import[memory]
            :service[name = lifecycle]
            export function read() {
                return memory.get("phase");
            }
            class Service {
                start() {
                    memory.set("phase", "started");
                }
                event(event) {
                    memory.set("event", event);
                    return event;
                }
                health() {
                    return memory.get("phase");
                }
                stop() {
                    memory.set("phase", "stopped");
                }
            }
        "#;
        let program = crate::parse_service_source(source).expect("service parse failed");
        let modules = modules_for_test("service-lifecycle");
        let memory = ServiceMemory::default();
        let observer = memory.clone();
        let executor = ServiceProgramExecutor::new(program, modules, memory);

        let start = block_on_ready(ServiceExecutor::lifecycle(
            &executor,
            ServiceLifecycle::Start,
            serde_json::json!({"service": "lifecycle"}),
        ))
        .expect("start lifecycle failed");
        assert_eq!(start, Some(serde_json::Value::Null));

        let health = block_on_ready(ServiceExecutor::lifecycle(
            &executor,
            ServiceLifecycle::Health,
            serde_json::json!({"service": "lifecycle"}),
        ))
        .expect("health lifecycle failed");
        assert_eq!(health, Some(serde_json::json!("started")));

        let event = block_on_ready(ServiceExecutor::lifecycle(
            &executor,
            ServiceLifecycle::Event,
            serde_json::json!({"kind": "refresh"}),
        ))
        .expect("event lifecycle failed");
        assert_eq!(event, Some(serde_json::json!({"kind": "refresh"})));
        assert_eq!(
            observer.get("event"),
            Some(serde_json::json!({"kind": "refresh"}))
        );

        block_on_ready(ServiceExecutor::lifecycle(
            &executor,
            ServiceLifecycle::Stop,
            serde_json::json!({"service": "lifecycle"}),
        ))
        .expect("stop lifecycle failed");
        assert_eq!(observer.get("phase"), Some(serde_json::json!("stopped")));
    }
}
