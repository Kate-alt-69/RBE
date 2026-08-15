//! Runtime helpers used by generated Rust route artifacts.

use std::collections::HashMap;

use crate::ast::{BinaryOp, Value};
use crate::interpreter::EvalError;
use crate::modules::ModuleRegistry;

pub fn member_get(base: &Value, field: &str) -> Result<Value, EvalError> {
    match base {
        Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
        other => Err(EvalError::new(format!("cannot access .{field} on {other:?}"))),
    }
}

pub fn call_module(
    modules: &ModuleRegistry,
    module_name: &str,
    function_name: &str,
    args: Vec<Value>,
) -> Result<Value, EvalError> {
    modules.call(module_name, function_name, &args)
        .map_err(|e| EvalError::new(e.to_string()))
}

pub fn call_direct(
    modules: &ModuleRegistry,
    binding: &str,
    args: Vec<Value>,
) -> Result<Value, EvalError> {
    modules.call_direct(binding, &args)
        .map_err(|e| EvalError::new(e.to_string()))
}

pub fn object_value(fields: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = HashMap::new();
    for (key, value) in fields { map.insert(key, value); }
    Value::Object(map)
}

pub fn truthy(value: &Value) -> bool { value.truthy() }

pub fn unary_not(value: Value) -> Value { Value::Bool(!value.truthy()) }

pub fn binary(op: BinaryOp, left: Value, right: Value) -> Result<Value, EvalError> {
    match op {
        BinaryOp::And => Ok(Value::Bool(left.truthy() && right.truthy())),
        BinaryOp::Or => Ok(Value::Bool(left.truthy() || right.truthy())),
        BinaryOp::Equal | BinaryOp::StrictEqual => Ok(Value::Bool(eq(&left, &right))),
        BinaryOp::NotEqual | BinaryOp::StrictNotEqual => Ok(Value::Bool(!eq(&left, &right))),
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
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            match (left, right) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Bool(match op {
                    BinaryOp::Less => a < b,
                    BinaryOp::LessEqual => a <= b,
                    BinaryOp::Greater => a > b,
                    BinaryOp::GreaterEqual => a >= b,
                    _ => unreachable!(),
                })),
                (Value::String(a), Value::String(b)) => {
                    let ord = a.cmp(&b);
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
    }
}

fn eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).map(|x| eq(v, x)).unwrap_or(false)),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq(x, y)),
        _ => false,
    }
}
