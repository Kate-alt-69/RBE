//! AST for the RBE `.route` language.
//!
//! The surface syntax is intentionally JavaScript-shaped, but this is
//! RBE's own restricted language. It is parsed into typed Rust data
//! before interpretation/transpilation; arbitrary JavaScript is never
//! executed.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum ImportTarget {
    Builtin(String),
    BuiltinFunction { module: String, function: String },
    Custom(String),
    CustomFunction { path: String, function: String },
}

#[derive(Debug, Clone)]
pub struct RouteFile {
    pub imports: Vec<ImportTarget>,
    pub functions: Vec<FunctionDef>,
    pub class_name: String,
    pub methods: Vec<MethodDef>,
}

#[derive(Debug, Clone)]
pub struct FunctionDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone)]
pub struct MethodDef {
    pub verb: String,
    pub param_name: Option<String>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone)]
pub enum Statement {
    Const { name: String, value: Expr },
    Return(Expr),
    Expr(Expr),
    If { condition: Expr, then_body: Vec<Statement>, else_body: Vec<Statement> },
}

#[derive(Debug, Clone)]
pub enum Expr {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    Ident(String),
    Member(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Array(Vec<Expr>),
    UnaryNot(Box<Expr>),
    Binary { left: Box<Expr>, op: BinaryOp, right: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Equal, StrictEqual, NotEqual, StrictNotEqual,
    Less, LessEqual, Greater, GreaterEqual,
    And, Or, Add, Subtract, Multiply, Divide, Modulo,
}

#[derive(Debug, Clone)]
pub enum Value {
    String(String), Number(f64), Bool(bool), Null,
    Object(HashMap<String, Value>), Array(Vec<Value>),
}

impl Value {
    pub fn truthy(&self) -> bool {
        match self {
            Self::Bool(v) => *v,
            Self::Null => false,
            Self::Number(v) => *v != 0.0 && !v.is_nan(),
            Self::String(v) => !v.is_empty(),
            Self::Object(_) | Self::Array(_) => true,
        }
    }
}
