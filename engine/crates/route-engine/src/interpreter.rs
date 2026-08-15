//! Executes a parsed `.route` method body against a real request,
//! calling into real Rust functions for module imports. This is the
//! "v1 is an interpreter, not a compiler" piece — see the crate root
//! doc comment for why, and what upgrading to real Rust codegen later
//! would change.

use std::collections::HashMap;

use crate::ast::{Expr, MethodDef, Statement, Value};
use crate::modules::ModuleRegistry;

#[derive(Debug)]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Request context available inside a `.route` method as the bound
/// parameter name (conventionally `req`). Deliberately small in v1 —
/// no dynamic path segments yet (see `discovery`'s doc comment), so
/// `params` is always empty for now.
pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub params: HashMap<String, Value>,
    pub query: HashMap<String, Value>,
}

impl RequestContext {
    fn as_value(&self) -> Value {
        let mut fields = HashMap::new();
        fields.insert("method".to_string(), Value::String(self.method.clone()));
        fields.insert("path".to_string(), Value::String(self.path.clone()));
        fields.insert("params".to_string(), Value::Object(self.params.clone()));
        fields.insert("query".to_string(), Value::Object(self.query.clone()));
        Value::Object(fields)
    }
}

/// A binding a name can resolve to during evaluation: either a plain
/// value, or a reference to an imported module (so `storage.get(...)`
/// can be recognized as "call into module `storage`", not "call a
/// method on a data value").
enum Binding {
    Value(Value),
    Module,
}

pub struct Interpreter<'a> {
    modules: &'a ModuleRegistry,
    scope: HashMap<String, Binding>,
}

impl<'a> Interpreter<'a> {
    pub fn new(modules: &'a ModuleRegistry) -> Self {
        Self {
            modules,
            scope: HashMap::new(),
        }
    }

    /// Runs a method body against a request, returning whatever its
    /// `return` statement produced (or `Value::Null` if it falls off
    /// the end without one — same as a JS function implicitly
    /// returning `undefined`, just represented as `null` here since
    /// there's no `undefined` in this value model).
    pub fn run(
        &mut self,
        method: &MethodDef,
        req: &RequestContext,
        module_names: &[String],
    ) -> Result<Value, EvalError> {
        if let Some(param_name) = &method.param_name {
            self.scope
                .insert(param_name.clone(), Binding::Value(req.as_value()));
        }
        for name in module_names {
            self.scope
                .insert(name.clone(), Binding::Module);
        }

        for stmt in &method.body {
            match stmt {
                Statement::Const { name, value } => {
                    let v = self.eval(value)?;
                    self.scope.insert(name.clone(), Binding::Value(v));
                }
                Statement::Return(expr) => {
                    return self.eval(expr);
                }
                Statement::Expr(expr) => {
                    self.eval(expr)?;
                }
            }
        }

        Ok(Value::Null)
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, EvalError> {
        match expr {
            Expr::String(s) => Ok(Value::String(s.clone())),
            Expr::Number(n) => Ok(Value::Number(*n)),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),

            Expr::Ident(name) => match self.scope.get(name) {
                Some(Binding::Value(v)) => Ok(v.clone()),
                Some(Binding::Module) => Err(EvalError::new(format!(
                    "{name} is a module, not a value — did you mean to call one of its functions?"
                ))),
                None => Err(EvalError::new(format!("{name} is not defined"))),
            },

            Expr::Object(fields) => {
                let mut map = HashMap::new();
                for (key, value_expr) in fields {
                    map.insert(key.clone(), self.eval(value_expr)?);
                }
                Ok(Value::Object(map))
            }

            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item)?);
                }
                Ok(Value::Array(values))
            }

            Expr::Member(base, field) => {
                // Special-case: `module.field` where `module` names an
                // imported module isn't a real member access — it's
                // only meaningful as the callee half of `module.fn(...)`,
                // handled in the `Call` arm below. Bare member access on
                // a module (no call) is an error.
                if let Expr::Ident(name) = base.as_ref() {
                    if matches!(self.scope.get(name), Some(Binding::Module)) {
                        return Err(EvalError::new(format!(
                            "{name}.{field} — accessing a module field without calling it isn't supported in v1"
                        )));
                    }
                }

                let base_val = self.eval(base)?;
                match base_val {
                    Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
                    other => Err(EvalError::new(format!(
                        "cannot access .{field} on {other:?} — not an object"
                    ))),
                }
            }

            Expr::Call(callee, arg_exprs) => {
                // Recognize `module.function(args)` calls into imported
                // modules — the one real form of "call" v1 supports.
                if let Expr::Member(base, function_name) = callee.as_ref() {
                    if let Expr::Ident(module_name) = base.as_ref() {
                        if matches!(self.scope.get(module_name), Some(Binding::Module)) {
                            let mut args = Vec::with_capacity(arg_exprs.len());
                            for a in arg_exprs {
                                args.push(self.eval(a)?);
                            }
                            return self
                                .modules
                                .call(module_name, function_name, &args)
                                .map_err(|e| EvalError::new(e.to_string()));
                        }
                    }
                }

                Err(EvalError::new(
                    "only `module.function(args)` calls into an imported module are supported in v1",
                ))
            }
        }
    }
}
