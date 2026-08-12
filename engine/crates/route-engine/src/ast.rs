//! AST for the v1 `.route` grammar. Deliberately tiny — see crate root
//! doc comment for the exact scope and what's out of bounds for v1.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum ImportTarget {
    /// `:import[net]` — bare identifier, resolved against the built-in
    /// module registry (real Rust functions, not JS).
    Builtin(String),
    /// `:import["./module/storage"]` — resolved against `./` = the
    /// directory the compiled backend binary lives in. Full `.module`
    /// semantics are deferred — see `modules::resolve_custom` — for
    /// now this parses successfully but any call into it errors
    /// clearly at request time rather than failing to parse.
    Custom(String),
}

#[derive(Debug, Clone)]
pub struct RouteFile {
    pub imports: Vec<ImportTarget>,
    pub class_name: String,
    pub methods: Vec<MethodDef>,
}

#[derive(Debug, Clone)]
pub struct MethodDef {
    /// HTTP verb this method handles — parsed from the method name
    /// (`get`, `post`, `put`, `delete`, `patch`, `head`, `options`),
    /// case-insensitive. Anything else is a parse error: a `.route`
    /// class method that isn't a known verb has no meaning in v1.
    pub verb: String,
    pub param_name: Option<String>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone)]
pub enum Statement {
    Const {
        name: String,
        value: Expr,
    },
    Return(Expr),
    /// A bare expression statement, e.g. a call for side effects whose
    /// result isn't used (`log.info("...")`).
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum Expr {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    Ident(String),
    /// `a.b.c` — resolved step by step at eval time.
    Member(Box<Expr>, String),
    /// `callee(args...)`
    Call(Box<Expr>, Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Array(Vec<Expr>),
}

/// What a route method actually produces after evaluation — kept
/// separate from `Expr` because this is a *value*, not syntax.
#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    Object(HashMap<String, Value>),
    Array(Vec<Value>),
}
