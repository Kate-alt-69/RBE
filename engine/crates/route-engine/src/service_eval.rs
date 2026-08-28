//! Executable `.service` program adapter built on the shared async module VM.
//!
//! Service exports use the same expression/control-flow evaluator as `.module`
//! files. Host-only capabilities (currently `memory`) are injected explicitly
//! instead of becoming ambient powers of the language runtime.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use service_runtime::{
    ServiceExecutionError, ServiceExecutionFuture, ServiceExecutor, ServiceMemory,
};

use crate::ast::{ModuleFile, ServiceProgram, Value};
use crate::module_eval::{
    HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError, ModuleExecutor,
};
use crate::module_runtime::ModuleProgram;

#[derive(Clone)]
struct ServiceHostCapabilities {
    memory: ServiceMemory,
}

impl ServiceHostCapabilities {
    fn new(memory: ServiceMemory) -> Self {
        Self { memory }
    }
}

impl HostCapabilityCaller for ServiceHostCapabilities {
    fn call<'a>(
        &'a self,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            if module != "memory" {
                return Ok(None);
            }

            let value = match function {
                "get" => {
                    expect_arity(module, function, &args, 1)?;
                    let key = expect_string(module, function, &args[0], 0)?;
                    self.memory
                        .get(key)
                        .map(json_to_value)
                        .transpose()?
                        .unwrap_or(Value::Null)
                }
                "set" => {
                    expect_arity(module, function, &args, 2)?;
                    let key = expect_string(module, function, &args[0], 0)?.to_string();
                    let value = value_to_json(args[1].clone())?;
                    self.memory.set(key, value);
                    Value::Bool(true)
                }
                "delete" => {
                    expect_arity(module, function, &args, 1)?;
                    let key = expect_string(module, function, &args[0], 0)?;
                    Value::Bool(self.memory.delete(key))
                }
                "clear" => {
                    expect_arity(module, function, &args, 0)?;
                    self.memory.clear();
                    Value::Bool(true)
                }
                "len" => {
                    expect_arity(module, function, &args, 0)?;
                    Value::Number(self.memory.len() as f64)
                }
                "isEmpty" | "is_empty" => {
                    expect_arity(module, function, &args, 0)?;
                    Value::Bool(self.memory.is_empty())
                }
                other => {
                    return Err(eval_error(
                        "SVC4201",
                        format!("unknown service host capability memory.{other}()"),
                    ));
                }
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
    host_capabilities: Arc<dyn HostCapabilityCaller>,
}

impl ServiceProgramExecutor {
    pub fn new(program: ServiceProgram, modules: ModuleProgram, memory: ServiceMemory) -> Self {
        let file = Arc::new(ModuleFile {
            imports: program.imports,
            functions: program.functions,
            exports: program.exports,
        });
        Self {
            modules,
            file,
            host_capabilities: Arc::new(ServiceHostCapabilities::new(memory)),
        }
    }
}

impl ServiceExecutor for ServiceProgramExecutor {
    fn call<'a>(
        &'a self,
        function: &'a str,
        args: Vec<JsonValue>,
    ) -> ServiceExecutionFuture<'a> {
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
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-service-eval-test-{}-{nonce}",
            std::process::id()
        ));
        let modules = ModuleProgram::load(&root.join("module")).expect("module load failed");
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
}
