//! Small runtime helpers that **transpiled** `.route` code (see
//! [`crate::transpiler`]) calls into — the non-literal, non-local-
//! variable parts of evaluation that still need a real function call
//! at runtime: object-field access and module calls.
//!
//! Everything else a `.route` body can do (string/number/bool/null
//! literals, object/array construction, `const` bindings, `return`)
//! transpiles directly to plain Rust with no helper needed — a `const`
//! becomes a `let`, a literal becomes a literal. These two functions
//! exist because they're the only two operations in the v1 grammar
//! that can genuinely *fail* at runtime in a way the transpiler can't
//! rule out ahead of time (member access on a non-object; calling a
//! module function that errors) — see [`crate::interpreter`]'s `eval`
//! function for the tree-walking version of the exact same logic.
//!
//! **This module and the interpreter must stay in sync.** They
//! implement the same two operations twice — once for the tree-walker,
//! once for generated code to call into — on purpose (the transpiler's
//! whole point is generating straight-line code instead of walking the
//! tree at request time), but that means a semantics change to one
//! needs the same change made here. There are no tests here separate
//! from route-engine's existing interpreter tests; anything that would
//! catch a interpreter/transpiler mismatch belongs in
//! `transpiler`'s own tests, which run BOTH the interpreter and the
//! transpiled-equivalent Rust logic against the same `.route` source
//! and assert they agree — see that module's `tests` section.

use std::collections::HashMap;

use crate::ast::Value;
use crate::interpreter::EvalError;
use crate::modules::ModuleRegistry;

/// `base.field` — mirrors `interpreter::eval`'s `Expr::Member` arm
/// exactly (the non-module-access case; module member access without
/// a call is rejected earlier, at transpile time, by
/// [`crate::transpiler`] itself, since which names are modules is
/// known statically then — see that module's doc comment).
pub fn member_get(base: &Value, field: &str) -> Result<Value, EvalError> {
    match base {
        Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
        other => Err(EvalError::new(format!(
            "cannot access .{field} on {other:?} — not an object"
        ))),
    }
}

/// `module_name.function_name(args)` — mirrors `interpreter::eval`'s
/// `Expr::Call` arm's module-call case exactly (the only call form v1
/// supports, and the only one the transpiler ever generates a call
/// site for).
pub fn call_module(
    modules: &ModuleRegistry,
    module_name: &str,
    function_name: &str,
    args: Vec<Value>,
) -> Result<Value, EvalError> {
    modules
        .call(module_name, function_name, &args)
        .map_err(|e| EvalError::new(e.to_string()))
}

/// Builds a `Value::Object` from `(String, Value)` pairs — a tiny
/// convenience so generated code reads as `object_value([("k".into(), v)])`
/// instead of hand-rolling a `HashMap` literal at every object-literal
/// call site; purely a readability choice for the generated `.rs`
/// artifacts (see [`crate::transpiler`]'s doc comment on why "readable"
/// is a real design goal here, not an afterthought), functionally
/// identical to constructing the `HashMap` directly.
pub fn object_value(fields: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = HashMap::new();
    for (key, value) in fields {
        map.insert(key, value);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_get_reads_existing_field() {
        let obj = object_value([("ok".to_string(), Value::Bool(true))]);
        let result = member_get(&obj, "ok").unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn member_get_missing_field_is_null_not_error() {
        let obj = object_value([]);
        let result = member_get(&obj, "missing").unwrap();
        assert!(matches!(result, Value::Null));
    }

    #[test]
    fn member_get_on_non_object_is_an_error() {
        let result = member_get(&Value::Number(1.0), "x");
        assert!(result.is_err());
    }
}
