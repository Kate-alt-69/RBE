//! Runtime interpreter for `.route` files.

use std::collections::HashMap;

use crate::ast::{BinaryOp, Expr, FunctionDef, MethodDef, Statement, Value};
use crate::modules::ModuleRegistry;

#[derive(Debug, Clone)]
pub struct EvalError { pub message: String }
impl EvalError { pub(crate) fn new(message: impl Into<String>) -> Self { Self { message: message.into() } } }
impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.message) }
}

pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub params: HashMap<String, Value>,
    pub query: HashMap<String, Value>,
}

impl RequestContext {
    fn as_value(&self) -> Value {
        let mut fields = HashMap::new();
        fields.insert("method".into(), Value::String(self.method.clone()));
        fields.insert("path".into(), Value::String(self.path.clone()));
        fields.insert("params".into(), Value::Object(self.params.clone()));
        fields.insert("query".into(), Value::Object(self.query.clone()));
        Value::Object(fields)
    }
}

enum Binding { Value(Value), Module }
enum Flow { Continue, Return(Value) }

pub struct Interpreter<'a> {
    modules: &'a ModuleRegistry,
    functions: HashMap<String, FunctionDef>,
    scope: HashMap<String, Binding>,
}

impl<'a> Interpreter<'a> {
    pub fn new(modules: &'a ModuleRegistry) -> Self {
        Self { modules, functions: HashMap::new(), scope: HashMap::new() }
    }

    pub fn with_functions(mut self, functions: &[FunctionDef]) -> Self {
        for function in functions {
            self.functions.insert(function.name.clone(), function.clone());
        }
        self
    }

    pub fn run(&mut self, method: &MethodDef, req: &RequestContext, module_names: &[String]) -> Result<Value, EvalError> {
        self.scope.clear();
        if let Some(param_name) = &method.param_name {
            self.scope.insert(param_name.clone(), Binding::Value(req.as_value()));
        }
        for name in module_names {
            if !self.modules.is_direct_function(name) {
                self.scope.insert(name.clone(), Binding::Module);
            }
        }

        match self.exec_block(&method.body)? {
            Flow::Continue => Ok(Value::Null),
            Flow::Return(value) => Ok(value),
        }
    }

    fn call_function(&self, name: &str, args: Vec<Value>) -> Result<Value, EvalError> {
        let Some(function) = self.functions.get(name) else {
            return Err(EvalError::new(format!("function {name} is not defined")));
        };
        if args.len() != function.params.len() {
            return Err(EvalError::new(format!(
                "function {name} expected {} argument(s), got {}",
                function.params.len(), args.len()
            )));
        }

        let all_functions: Vec<FunctionDef> = self.functions.values().cloned().collect();
        let mut child = Interpreter::new(self.modules).with_functions(&all_functions);
        for (param, value) in function.params.iter().zip(args) {
            child.scope.insert(param.clone(), Binding::Value(value));
        }

        match child.exec_block(&function.body)? {
            Flow::Continue => Ok(Value::Null),
            Flow::Return(value) => Ok(value),
        }
    }

    fn exec_block(&mut self, body: &[Statement]) -> Result<Flow, EvalError> {
        for stmt in body {
            match stmt {
                Statement::Const { name, value } => {
                    let value = self.eval(value)?;
                    self.scope.insert(name.clone(), Binding::Value(value));
                }
                Statement::Return(expr) => return Ok(Flow::Return(self.eval(expr)?)),
                Statement::Expr(expr) => { self.eval(expr)?; }
                Statement::If { condition, then_body, else_body } => {
                    let branch = if self.eval(condition)?.truthy() { then_body } else { else_body };
                    match self.exec_block(branch)? {
                        Flow::Continue => {}
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, EvalError> {
        match expr {
            Expr::String(s) => Ok(Value::String(s.clone())),
            Expr::Number(n) => Ok(Value::Number(*n)),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Ident(name) => match self.scope.get(name) {
                Some(Binding::Value(v)) => Ok(v.clone()),
                Some(Binding::Module) => Err(EvalError::new(format!("{name} is a module, not a value — call one of its functions"))),
                None if self.functions.contains_key(name) => Err(EvalError::new(format!("function {name} must be called, not used as a value"))),
                None if self.modules.is_direct_function(name) => Err(EvalError::new(format!("{name} is an imported function, not a value — call it"))),
                None => Err(EvalError::new(format!("{name} is not defined"))),
            },
            Expr::Object(fields) => {
                let mut map = HashMap::new();
                for (key, expr) in fields { map.insert(key.clone(), self.eval(expr)?); }
                Ok(Value::Object(map))
            }
            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items { values.push(self.eval(item)?); }
                Ok(Value::Array(values))
            }
            Expr::Member(base, field) => {
                if let Expr::Ident(name) = base.as_ref() {
                    if matches!(self.scope.get(name), Some(Binding::Module)) {
                        return Err(EvalError::new(format!("{name}.{field} must be called")));
                    }
                }
                match self.eval(base)? {
                    Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
                    other => Err(EvalError::new(format!("cannot access .{field} on {other:?}"))),
                }
            }
            Expr::Call(callee, arg_exprs) => {
                let mut args = Vec::with_capacity(arg_exprs.len());
                for arg in arg_exprs { args.push(self.eval(arg)?); }

                if let Expr::Ident(name) = callee.as_ref() {
                    if self.modules.is_direct_function(name) {
                        return self.modules.call_direct(name, &args).map_err(|e| EvalError::new(e.to_string()));
                    }
                    if self.functions.contains_key(name) {
                        return self.call_function(name, args);
                    }
                }

                if let Expr::Member(base, function_name) = callee.as_ref() {
                    if let Expr::Ident(module_name) = base.as_ref() {
                        if matches!(self.scope.get(module_name), Some(Binding::Module)) {
                            return self.modules.call(module_name, function_name, &args)
                                .map_err(|e| EvalError::new(e.to_string()));
                        }
                    }
                }

                Err(EvalError::new("unsupported function call; use a local function or imported capability"))
            }
            Expr::UnaryNot(expr) => Ok(Value::Bool(!self.eval(expr)?.truthy())),
            Expr::Binary { left, op, right } => {
                match op {
                    BinaryOp::And => {
                        let left = self.eval(left)?;
                        if !left.truthy() { return Ok(Value::Bool(false)); }
                        return Ok(Value::Bool(self.eval(right)?.truthy()));
                    }
                    BinaryOp::Or => {
                        let left = self.eval(left)?;
                        if left.truthy() { return Ok(Value::Bool(true)); }
                        return Ok(Value::Bool(self.eval(right)?.truthy()));
                    }
                    _ => {}
                }
                let left = self.eval(left)?;
                let right = self.eval(right)?;
                self.binary(op, left, right)
            }
        }
    }

    fn binary(&self, op: &BinaryOp, left: Value, right: Value) -> Result<Value, EvalError> {
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
                        let ord = a.cmp(b);
                        Ok(Value::Bool(match op {
                            BinaryOp::Less => ord.is_lt(),
                            BinaryOp::LessEqual => ord.is_le(),
                            BinaryOp::Greater => ord.is_gt(),
                            BinaryOp::GreaterEqual => ord.is_ge(),
                            _ => unreachable!(),
                        }))
                    }
                    _ => Err(EvalError::new("comparison requires matching numbers or strings")),
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
                    _ => return Err(EvalError::new("arithmetic requires numbers")),
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
}

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).map(|x| value_eq(v, x)).unwrap_or(false)),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| value_eq(x, y)),
        _ => false,
    }
}
