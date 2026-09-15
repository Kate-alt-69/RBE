from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    "use crate::ast::{Expr, FunctionDef, ImportTarget, RouteFile, Statement};",
    "use crate::ast::{BinaryOp, Expr, FunctionDef, ImportTarget, RouteFile, Statement, Value};",
    "AST imports",
)
text = replace_once(
    text,
    """/// Generation 8 carries the evaluator-visible `req.body` through one strict
/// linked Module parameter into an exact host-capability call. Direct
/// Route-to-Storage remains unreachable and capability ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 8;""",
    """/// Generation 9 folds request-independent REL statements and expressions into
/// deterministic native WASM results while preserving generation 8's strict
/// linked `req.body` capability path. Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 9;""",
    "compiler generation",
)
text = replace_once(
    text,
    """/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler generation v8 keeps ABI v3 and permits
/// one immutable linked Module function supplied by RELC. The linked function
/// must itself be exactly one return of one direct HTTP, Video, Service, or
/// Storage host call. Arguments may be static JSON, or one Route `req.body`
/// value may pass unchanged through one Module parameter into the host call.
/// Namespace imports, transformed dynamic arguments, wider Module bodies, and
/// nested chains remain interpreter-only.""",
    """/// Native lowering remains intentionally strict: one HTTP method and no Route
/// helper functions. Compiler generation v9 keeps ABI v3, adds deterministic
/// folding of request-independent `const`, expression, `if`, and `return`
/// statements, and retains generation 8's immutable linked Module capability
/// path. Dynamic transformations, wider Module bodies, namespace imports, and
/// nested host-call chains remain interpreter-only.""",
    "compiler contract",
)

start = text.index("    let method = &file.methods[0];")
end = text.index("\n    let sha256 = hex::encode(Sha256::digest(&bytes));", start)
old_body = text[start:end]
new_body = r'''    let method = &file.methods[0];
    let (bytes, input) = if let Some(import) = host_import.as_ref() {
        let [Statement::Return(expr)] = method.body.as_slice() else {
            return fallback(
                "native host capability route body currently requires one return statement",
            );
        };
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
    } else if let Some(value) = static_route_result(&method.body) {
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
'''
text = text[:start] + new_body + text[end:]

insert_anchor = "\nfn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {"
if insert_anchor not in text:
    raise SystemExit("static fold insertion anchor missing")
static_fold = r'''
#[derive(Debug, Clone)]
enum StaticFlow {
    Continue,
    Return(Value),
}

fn static_route_result(body: &[Statement]) -> Option<serde_json::Value> {
    let mut scope = BTreeMap::<String, Value>::new();
    let value = match static_exec_block(body, &mut scope)? {
        StaticFlow::Continue => Value::Null,
        StaticFlow::Return(value) => value,
    };
    Some(static_value_to_json(&value))
}

fn static_exec_block(
    body: &[Statement],
    scope: &mut BTreeMap<String, Value>,
) -> Option<StaticFlow> {
    for statement in body {
        match statement {
            Statement::Const { name, value } => {
                let value = static_eval_expr(value, scope)?;
                scope.insert(name.clone(), value);
            }
            Statement::Return(expr) => {
                return Some(StaticFlow::Return(static_eval_expr(expr, scope)?));
            }
            Statement::Expr(expr) => {
                static_eval_expr(expr, scope)?;
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let condition = static_eval_expr(condition, scope)?;
                let branch = if condition.truthy() {
                    then_body
                } else {
                    else_body
                };
                match static_exec_block(branch, scope)? {
                    StaticFlow::Continue => {}
                    flow @ StaticFlow::Return(_) => return Some(flow),
                }
            }
        }
    }
    Some(StaticFlow::Continue)
}

fn static_eval_expr(expr: &Expr, scope: &BTreeMap<String, Value>) -> Option<Value> {
    match expr {
        Expr::String(value) => Some(Value::String(value.clone())),
        Expr::Number(value) => Some(Value::Number(*value)),
        Expr::Bool(value) => Some(Value::Bool(*value)),
        Expr::Null => Some(Value::Null),
        Expr::Ident(name) => scope.get(name).cloned(),
        Expr::Member(base, field) => {
            let base = static_eval_expr(base, scope)?;
            crate::transpiled_support::member_get(&base, field).ok()
        }
        Expr::Call(_, _) => None,
        Expr::Object(fields) => {
            let mut values = std::collections::HashMap::with_capacity(fields.len());
            for (name, value) in fields {
                values.insert(name.clone(), static_eval_expr(value, scope)?);
            }
            Some(Value::Object(values))
        }
        Expr::Array(items) => items
            .iter()
            .map(|item| static_eval_expr(item, scope))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        Expr::UnaryNot(value) => Some(crate::transpiled_support::unary_not(static_eval_expr(
            value, scope,
        )?)),
        Expr::Binary { left, op, right } => match op {
            BinaryOp::And => {
                let left = static_eval_expr(left, scope)?;
                if !left.truthy() {
                    Some(Value::Bool(false))
                } else {
                    Some(Value::Bool(static_eval_expr(right, scope)?.truthy()))
                }
            }
            BinaryOp::Or => {
                let left = static_eval_expr(left, scope)?;
                if left.truthy() {
                    Some(Value::Bool(true))
                } else {
                    Some(Value::Bool(static_eval_expr(right, scope)?.truthy()))
                }
            }
            _ => crate::transpiled_support::binary(
                *op,
                static_eval_expr(left, scope)?,
                static_eval_expr(right, scope)?,
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
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
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
'''
text = text.replace(insert_anchor, "\n" + static_fold + insert_anchor, 1)

test_anchor = """    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {"""
if test_anchor not in text:
    raise SystemExit("static control-flow test anchor missing")
new_tests = r'''    #[test]
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

'''
text = text.replace(test_anchor, new_tests + test_anchor, 1)
path.write_text(text)
