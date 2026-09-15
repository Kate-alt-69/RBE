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
    """/// Generation 9 folds request-independent REL statements and expressions into
/// deterministic native WASM results while preserving generation 8's strict
/// linked `req.body` capability path. Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 9;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;""",
    """/// Generation 10 folds request-independent local Route helpers in addition to
/// generation 9's static statements and expressions. Dynamic request data and
/// host-dependent helper calls remain outside the native subset. Capability ABI
/// v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 10;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STATIC_HELPER_CALL_DEPTH: usize = 32;""",
    "compiler generation",
)

text = replace_once(
    text,
    """/// Native lowering remains intentionally strict: one HTTP method and no Route
/// helper functions. Compiler generation v9 keeps ABI v3, adds deterministic
/// folding of request-independent `const`, expression, `if`, and `return`
/// statements, and retains generation 8's immutable linked Module capability
/// path. Dynamic transformations, wider Module bodies, namespace imports, and
/// nested host-call chains remain interpreter-only.""",
    """/// Native lowering remains intentionally strict: one HTTP method. Compiler
/// generation v10 keeps ABI v3, adds bounded deterministic folding of pure
/// request-independent local Route helpers, and retains generation 9's static
/// statements plus generation 8's immutable linked Module capability path.
/// Dynamic transformations, host-dependent helpers, wider Module bodies,
/// namespace imports, and nested host-call chains remain interpreter-only.""",
    "compiler contract",
)

text = text.replace("native Route-WASM v8 only supports", "native Route-WASM v10 only supports")
text = text.replace("native Route-WASM v8 supports", "native Route-WASM v10 supports")

text = replace_once(
    text,
    """    if !file.functions.is_empty() {
        return fallback("route helper functions are not WASM-native yet");
    }
    if file.methods.len() != 1 {""",
    """    if file.methods.len() != 1 {""",
    "route helper rejection",
)

text = replace_once(
    text,
    """    } else if let Some(value) = static_route_result(&method.body) {""",
    """    } else if let Some(value) = static_route_result(&method.body, &file.functions) {""",
    "static route invocation",
)

start = text.index("#[derive(Debug, Clone)]\nenum StaticFlow")
end = text.index("\nfn static_value_to_json", start)
new_block = r'''#[derive(Debug, Clone)]
enum StaticFlow {
    Continue,
    Return(Value),
}

fn static_route_result(
    body: &[Statement],
    functions: &[FunctionDef],
) -> Option<serde_json::Value> {
    let mut function_map = BTreeMap::<String, FunctionDef>::new();
    for function in functions {
        if function_map
            .insert(function.name.clone(), function.clone())
            .is_some()
        {
            return None;
        }
    }

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
                match static_exec_block(
                    branch,
                    scope,
                    functions,
                    remaining_helper_depth,
                )? {
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
'''
text = text[:start] + new_block + text[end:]

anchor = """    #[test]
    fn request_dependent_if_stays_interpreter_fallback() {"""
if anchor not in text:
    raise SystemExit("helper test anchor missing")
new_tests = r'''    #[test]
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

'''
text = text.replace(anchor, new_tests + anchor, 1)

path.write_text(text)
