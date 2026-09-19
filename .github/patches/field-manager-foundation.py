from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# FieldManager AST / first-class source identity.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/ast.rs",
    """#[derive(Debug, Clone)]
pub struct ModuleFile {
    pub imports: Vec<ImportTarget>,
    pub functions: Vec<FunctionDef>,
    pub exports: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceProgram {""",
    """#[derive(Debug, Clone)]
pub struct ModuleFile {
    pub imports: Vec<ImportTarget>,
    pub functions: Vec<FunctionDef>,
    pub exports: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldValueType {
    String,
    Int,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldBindingMode {
    Required,
    Optional,
    Dynamic,
}

#[derive(Debug, Clone)]
pub struct FieldDirective {
    pub source: String,
    pub key: Option<String>,
    pub optional: bool,
    pub value_type: FieldValueType,
}

#[derive(Debug, Clone)]
pub struct FieldBinding {
    pub name: String,
    pub lookup: String,
    pub mode: FieldBindingMode,
    pub value_type: FieldValueType,
    pub default: Option<Value>,
    pub strip_prefix: bool,
}

#[derive(Debug, Clone)]
pub struct FieldFile {
    pub imports: Vec<ImportTarget>,
    pub directive: FieldDirective,
    pub bindings: Vec<FieldBinding>,
    pub resolver: Option<FunctionDef>,
}

#[derive(Debug, Clone)]
pub struct ServiceProgram {""",
    "FieldManager AST",
)

replace_once(
    "engine/crates/route-engine/src/source_registry.rs",
    """/// This is deliberately separate from grammar: Route, Module, Service and
/// Server REL share the language grammar while exposing different runtime
/// capabilities and lifecycle surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelSourceKind {
    Route,
    Module,
    Service,
    Server,
}
""",
    """/// This is deliberately separate from grammar: Route, Module, Service, Field
/// and Server REL share the language grammar while exposing different runtime
/// capabilities and lifecycle surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelSourceKind {
    Route,
    Module,
    Service,
    Field,
    Server,
}
""",
    "Field source kind",
)
replace_once(
    "engine/crates/route-engine/src/source_registry.rs",
    """            Self::Route => "route",
            Self::Module => "module",
            Self::Service => "service",
            Self::Server => "server",
""",
    """            Self::Route => "route",
            Self::Module => "module",
            Self::Service => "service",
            Self::Field => "field",
            Self::Server => "server",
""",
    "Field source label",
)
replace_once(
    "engine/crates/route-engine/src/source_registry.rs",
    """            Some("route") => Some(Self::Route),
            Some("module") => Some(Self::Module),
            Some("service") => Some(Self::Service),
            Some("server")
""",
    """            Some("route") => Some(Self::Route),
            Some("module") => Some(Self::Module),
            Some("service") => Some(Self::Service),
            Some("field") => Some(Self::Field),
            Some("server")
""",
    "Field physical extension",
)

replace_once(
    "engine/crates/route-engine/src/embedded_rel.rs",
    """//! Extraction happens before Server REL lexing so embedded Route/Module/Service
//! syntax is never interpreted as Server policy syntax. The cleaned Server REL
""",
    """//! Extraction happens before Server REL lexing so embedded Route/Module/Service/Field
//! syntax is never interpreted as Server policy syntax. The cleaned Server REL
""",
    "embedded Field docs",
)
replace_once(
    "engine/crates/route-engine/src/embedded_rel.rs",
    """        "route" => Ok(RelSourceKind::Route),
        "module" => Ok(RelSourceKind::Module),
        "service" => Ok(RelSourceKind::Service),
        "server" => Ok(RelSourceKind::Server),
""",
    """        "route" => Ok(RelSourceKind::Route),
        "module" => Ok(RelSourceKind::Module),
        "service" => Ok(RelSourceKind::Service),
        "field" => Ok(RelSourceKind::Field),
        "server" => Ok(RelSourceKind::Server),
""",
    "embedded Field kind",
)

# ---------------------------------------------------------------------------
# Parser: :field[...] metadata, declarative resolve{}, executable resolve(...),
# and :import[field:NAME] using the existing BuiltinFunction representation.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """use crate::ast::{
    BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceClassDef,
    ServiceProgram, Statement, Value,
};""",
    """use crate::ast::{
    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,
    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceClassDef, ServiceProgram,
    Statement, Value,
};""",
    "Field parser imports",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """    pub fn parse_service_file(mut self) -> Result<ServiceProgram, ParseError> {
""",
    r'''    pub fn parse_field_file(mut self) -> Result<FieldFile, ParseError> {
        let mut imports = Vec::new();
        let mut directive = None;

        while self.check(&TokenKind::Colon) {
            if self.is_import_directive() {
                imports.extend(self.parse_imports()?);
                continue;
            }
            if directive.is_some() {
                return Err(self.error_here("duplicate :field[...] declaration"));
            }
            directive = Some(self.parse_field_directive()?);
        }

        if directive.is_none() && self.is_ident("field") {
            directive = Some(self.parse_field_block_directive()?);
        }
        let directive = directive.ok_or_else(|| {
            self.error_here(".field source requires :field[...] or field { ... } metadata")
        })?;

        let mut bindings = Vec::new();
        let mut resolver = None;
        if self.is_ident("resolve") {
            self.advance();
            if self.check(&TokenKind::LBrace) {
                self.advance();
                while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
                    let name = self.expect_ident()?;
                    self.expect(TokenKind::Eq)?;
                    let binding = self.parse_field_binding(name)?;
                    self.expect(TokenKind::Semicolon)?;
                    if bindings.iter().any(|existing: &FieldBinding| existing.name == binding.name) {
                        return Err(self.error_here("duplicate FieldManager binding"));
                    }
                    bindings.push(binding);
                }
                self.expect(TokenKind::RBrace)?;
            } else if self.check(&TokenKind::LParen) {
                let params = self.parse_params()?;
                if params.len() > 2 {
                    return Err(self.error_here(
                        "FieldManager resolve(raw, context) accepts at most two parameters",
                    ));
                }
                let body = self.parse_block()?;
                resolver = Some(FunctionDef {
                    name: "resolve".into(),
                    params,
                    body,
                });
            } else {
                return Err(self.error_here("expected `{` or `(` after FieldManager resolve"));
            }
        }

        if !self.check(&TokenKind::Eof) {
            return Err(self.error_here("unexpected content after FieldManager resolver"));
        }
        if bindings.is_empty() && resolver.is_none() && directive.key.is_none() {
            return Err(self.error_here(
                "FieldManager source must declare a key, declarative resolve bindings, or resolve(raw, context)",
            ));
        }

        Ok(FieldFile {
            imports,
            directive,
            bindings,
            resolver,
        })
    }

    fn is_ident(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(TokenKind::Ident(name)) if name == expected
        )
    }

    fn parse_field_directive(&mut self) -> Result<FieldDirective, ParseError> {
        self.expect(TokenKind::Colon)?;
        match self.advance().kind {
            TokenKind::Ident(name) if name == "field" => {}
            other => {
                return Err(self.error_here(&format!(
                    "expected :field[...] declaration, got {other:?}"
                )));
            }
        }
        self.expect(TokenKind::LBracket)?;
        let mut directive = FieldDirective {
            source: "query".into(),
            key: None,
            optional: false,
            value_type: FieldValueType::String,
        };
        while !self.check(&TokenKind::RBracket) {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            self.apply_field_directive_option(&mut directive, &name)?;
            if self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    return Err(self.error_here("trailing commas are not allowed in :field[...]"));
                }
            } else if !self.check(&TokenKind::RBracket) {
                return Err(self.error_here("expected `,` between :field[...] options"));
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(directive)
    }

    fn parse_field_block_directive(&mut self) -> Result<FieldDirective, ParseError> {
        let name = self.expect_ident()?;
        debug_assert_eq!(name, "field");
        self.expect(TokenKind::LBrace)?;
        let mut directive = FieldDirective {
            source: "query".into(),
            key: None,
            optional: false,
            value_type: FieldValueType::String,
        };
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let option = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            self.apply_field_directive_option(&mut directive, &option)?;
            self.expect(TokenKind::Semicolon)?;
        }
        self.expect(TokenKind::RBrace)?;
        Ok(directive)
    }

    fn apply_field_directive_option(
        &mut self,
        directive: &mut FieldDirective,
        name: &str,
    ) -> Result<(), ParseError> {
        match name {
            "source" => {
                let source = self.expect_ident()?;
                if source != "query" {
                    return Err(self.error_here(
                        "FieldManager source currently supports only `query`",
                    ));
                }
                directive.source = source;
            }
            "key" => match self.advance().kind {
                TokenKind::String(value) if !value.is_empty() => directive.key = Some(value),
                other => {
                    return Err(self.error_here(&format!(
                        "FieldManager key must be a non-empty string, got {other:?}"
                    )));
                }
            },
            "optional" => {
                directive.optional = match self.advance().kind {
                    TokenKind::True => true,
                    TokenKind::False => false,
                    other => {
                        return Err(self.error_here(&format!(
                            "FieldManager optional must be true or false, got {other:?}"
                        )));
                    }
                };
            }
            "required" => {
                let required = match self.advance().kind {
                    TokenKind::True => true,
                    TokenKind::False => false,
                    other => {
                        return Err(self.error_here(&format!(
                            "FieldManager required must be true or false, got {other:?}"
                        )));
                    }
                };
                directive.optional = !required;
            }
            "type" => directive.value_type = self.parse_field_type()?,
            other => {
                return Err(self.error_here(&format!(
                    "unknown FieldManager metadata option {other:?}"
                )));
            }
        }
        Ok(())
    }

    fn parse_field_type(&mut self) -> Result<FieldValueType, ParseError> {
        match self.expect_ident()?.as_str() {
            "string" => Ok(FieldValueType::String),
            "int" => Ok(FieldValueType::Int),
            "bool" => Ok(FieldValueType::Bool),
            other => Err(self.error_here(&format!(
                "unknown FieldManager type {other:?}; expected string, int, or bool"
            ))),
        }
    }

    fn parse_field_binding(&mut self, name: String) -> Result<FieldBinding, ParseError> {
        let mode = match self.expect_ident()?.as_str() {
            "required" => FieldBindingMode::Required,
            "optional" => FieldBindingMode::Optional,
            "dynamic" => FieldBindingMode::Dynamic,
            other => {
                return Err(self.error_here(&format!(
                    "unknown FieldManager resolver {other:?}; expected required, optional, or dynamic"
                )));
            }
        };
        self.expect(TokenKind::LParen)?;
        let lookup = match self.advance().kind {
            TokenKind::String(value) if !value.is_empty() => value,
            other => {
                return Err(self.error_here(&format!(
                    "FieldManager lookup key/prefix must be a non-empty string, got {other:?}"
                )));
            }
        };
        let mut value_type = FieldValueType::String;
        let mut default = None;
        let mut strip_prefix = false;
        while self.check(&TokenKind::Comma) {
            self.advance();
            let option = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            match option.as_str() {
                "type" => value_type = self.parse_field_type()?,
                "default" => {
                    let expr = self.parse_expression()?;
                    default = Some(self.bound_constant_value(&expr)?);
                }
                "stripPrefix" => {
                    strip_prefix = match self.advance().kind {
                        TokenKind::True => true,
                        TokenKind::False => false,
                        other => {
                            return Err(self.error_here(&format!(
                                "stripPrefix must be true or false, got {other:?}"
                            )));
                        }
                    };
                }
                other => {
                    return Err(self.error_here(&format!(
                        "unknown FieldManager resolver option {other:?}"
                    )));
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        if mode == FieldBindingMode::Required && default.is_some() {
            return Err(self.error_here("required(...) cannot declare a default"));
        }
        if mode == FieldBindingMode::Dynamic
            && (default.is_some() || value_type != FieldValueType::String)
        {
            return Err(self.error_here(
                "dynamic(...) supports stripPrefix only; dynamic values stay strings",
            ));
        }
        Ok(FieldBinding {
            name,
            lookup,
            mode,
            value_type,
            default,
            strip_prefix,
        })
    }

    pub fn parse_service_file(mut self) -> Result<ServiceProgram, ParseError> {
''',
    "Field parser",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """                } else if name == "service" && self.check(&TokenKind::Colon) {
                    self.advance();
                    let service = self.expect_ident()?;
""",
    """                } else if name == "field" && self.check(&TokenKind::Colon) {
                    self.advance();
                    let field = self.expect_ident()?;
                    Ok(ImportTarget::BuiltinFunction {
                        module: "field".into(),
                        function: field,
                    })
                } else if name == "service" && self.check(&TokenKind::Colon) {
                    self.advance();
                    let service = self.expect_ident()?;
""",
    "field:resolver import syntax",
)

# ---------------------------------------------------------------------------
# Deterministic math/regx helpers and FieldManager import namespace awareness.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/Cargo.toml",
    'hex = "0.4"\n',
    'hex = "0.4"\nregex = "1"\n',
    "regex dependency",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """    Request,
    Security,
    Response,
    VideoManager,
""",
    """    Request,
    Security,
    Response,
    Math,
    Regx,
    VideoManager,
""",
    "math/regx module kinds",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '            | "response"\n            | "private"\n',
    '            | "response"\n            | "private"\n            | "math"\n            | "regx"\n            | "field"\n',
    "route deterministic Field helpers",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """        "response" => matches!(
            function,
""",
    """        "math" => matches!(
            function,
            "trim" | "abs" | "floor" | "ceil" | "round" | "min" | "max" | "clamp"
        ),
        "regx" => matches!(function, "test" | "raw"),
        // Reusable `.field` resolver names are application-defined and are
        // validated against the Field source registry during RELC linking.
        "field" => !function.is_empty(),
        "response" => matches!(
            function,
""",
    "math/regx/field builtin functions",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """        ImportTarget::Service(name) => name.clone(),
        ImportTarget::ServiceFunction { function, .. } => function.clone(),
""",
    """        ImportTarget::Service(name) => name.clone(),
        ImportTarget::ServiceFunction { function, .. } => function.clone(),
""",
    "binding anchor sanity",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
""",
    """                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "math" => ModuleKind::Builtin(BuiltinModule::Math),
                        "regx" => ModuleKind::Builtin(BuiltinModule::Regx),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
""",
    "namespace math/regx registry",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
                        _ => ModuleKind::CustomUnimplemented {
""",
    """                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "math" => ModuleKind::Builtin(BuiltinModule::Math),
                        "regx" => ModuleKind::Builtin(BuiltinModule::Regx),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
                        _ => ModuleKind::CustomUnimplemented {
""",
    "direct math/regx registry",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """            ModuleKind::Builtin(BuiltinModule::Response) => call_response(function_name, args),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {
""",
    """            ModuleKind::Builtin(BuiltinModule::Response) => call_response(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Math) => call_math(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Regx) => call_regx(function_name, args),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {
""",
    "math/regx dispatch",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """fn request_object(value: Option<&Value>) -> Result<&HashMap<String, Value>, ModuleError> {
""",
    r'''fn math_number(value: Option<&Value>, label: &str) -> Result<f64, ModuleError> {
    match value {
        Some(Value::Number(value)) if value.is_finite() => Ok(*value),
        _ => Err(ModuleError {
            message: format!("{label} must be a finite number"),
        }),
    }
}

fn call_math(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "trim" => match args.first() {
            Some(Value::String(value)) => Ok(Value::String(value.trim().to_string())),
            _ => Err(ModuleError {
                message: "math.trim() requires a string".into(),
            }),
        },
        "abs" | "floor" | "ceil" | "round" => {
            let value = math_number(args.first(), "math value")?;
            let value = match function_name {
                "abs" => value.abs(),
                "floor" => value.floor(),
                "ceil" => value.ceil(),
                _ => value.round(),
            };
            Ok(Value::Number(value))
        }
        "min" | "max" => {
            let left = math_number(args.first(), "left math value")?;
            let right = math_number(args.get(1), "right math value")?;
            Ok(Value::Number(if function_name == "min" {
                left.min(right)
            } else {
                left.max(right)
            }))
        }
        "clamp" => {
            let value = math_number(args.first(), "math value")?;
            let min = math_number(args.get(1), "minimum")?;
            let max = math_number(args.get(2), "maximum")?;
            if min > max {
                return Err(ModuleError {
                    message: "math.clamp() minimum must not exceed maximum".into(),
                });
            }
            Ok(Value::Number(value.clamp(min, max)))
        }
        other => Err(ModuleError {
            message: format!("math.{other}() does not exist"),
        }),
    }
}

const REGX_MAX_PATTERN_BYTES: usize = 4096;
const REGX_MAX_INPUT_BYTES: usize = 1024 * 1024;

fn regx_string<'a>(value: Option<&'a Value>, label: &str, limit: usize) -> Result<&'a str, ModuleError> {
    match value {
        Some(Value::String(value)) if value.len() <= limit => Ok(value),
        Some(Value::String(_)) => Err(ModuleError {
            message: format!("{label} exceeds {limit} bytes"),
        }),
        _ => Err(ModuleError {
            message: format!("{label} must be a string"),
        }),
    }
}

fn call_regx(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let pattern = regx_string(args.first(), "regx pattern", REGX_MAX_PATTERN_BYTES)?;
    let compiled = regex::Regex::new(pattern).map_err(|error| ModuleError {
        message: format!("invalid regx pattern: {error}"),
    })?;
    match function_name {
        "raw" => Ok(Value::String(compiled.as_str().to_string())),
        "test" => {
            let value = regx_string(args.get(1), "regx input", REGX_MAX_INPUT_BYTES)?;
            Ok(Value::Bool(compiled.is_match(value)))
        }
        other => Err(ModuleError {
            message: format!("regx.{other}() does not exist"),
        }),
    }
}

fn request_object(value: Option<&Value>) -> Result<&HashMap<String, Value>, ModuleError> {
''',
    "math/regx implementations",
)

# ---------------------------------------------------------------------------
# Runtime Image / RELC first-class Field source.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    "use crate::ast::{ModuleFile, RouteFile, ServiceProgram};",
    "use crate::ast::{FieldFile, ModuleFile, RouteFile, ServiceProgram};",
    "RuntimeImage Field AST import",
)
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    """pub enum RuntimeExecutable {
    Route(Arc<RouteFile>),
    Module(Arc<ModuleFile>),
    Service(Arc<ServiceProgram>),
    Server(Arc<ServerProgram>),
}
""",
    """pub enum RuntimeExecutable {
    Route(Arc<RouteFile>),
    Module(Arc<ModuleFile>),
    Service(Arc<ServiceProgram>),
    Field(Arc<FieldFile>),
    Server(Arc<ServerProgram>),
}
""",
    "RuntimeExecutable Field",
)
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    """    pub routes: Vec<SourceId>,
    pub modules: Vec<SourceId>,
    pub services: Vec<SourceId>,
    pub sources: Vec<RuntimeSourceManifest>,
""",
    """    pub routes: Vec<SourceId>,
    pub modules: Vec<SourceId>,
    pub services: Vec<SourceId>,
    pub fields: Vec<SourceId>,
    pub sources: Vec<RuntimeSourceManifest>,
""",
    "RuntimeImage Field list",
)
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    """    pub fn route_file(&self, id: &SourceId) -> Option<Arc<RouteFile>> {
""",
    """    pub fn field_file(&self, id: &SourceId) -> Option<Arc<FieldFile>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Field(file)) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn route_file(&self, id: &SourceId) -> Option<Arc<RouteFile>> {
""",
    "RuntimeImage Field accessor",
)

replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """use crate::ast::{
    Expr, FunctionDef, ImportTarget, ModuleFile, RouteFile, ServiceProgram, Statement,
};""",
    """use crate::ast::{
    Expr, FieldFile, FunctionDef, ImportTarget, ModuleFile, RouteFile, ServiceProgram, Statement,
};""",
    "RELC Field AST import",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    "use crate::modules::binding_name;",
    "use crate::modules::{binding_name, builtin_function_exists};",
    "RELC builtin helper validation import",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """    collect_physical_dir(api_dir, RelSourceKind::Route, "route", &mut out)?;
    collect_physical_dir(module_dir, RelSourceKind::Module, "module", &mut out)?;
""",
    """    collect_physical_dir(api_dir, RelSourceKind::Route, "route", &mut out)?;
    collect_physical_dir(api_dir, RelSourceKind::Field, "field", &mut out)?;
    collect_physical_dir(module_dir, RelSourceKind::Module, "module", &mut out)?;
""",
    "discover physical Field sources",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """    let mut routes = Vec::new();
    let mut modules = Vec::new();
    let mut services = Vec::new();
""",
    """    let mut routes = Vec::new();
    let mut modules = Vec::new();
    let mut services = Vec::new();
    let mut fields = Vec::new();
""",
    "RELC Field source list",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """            RelSourceKind::Service => {
                services.push(source.id().clone());
                service_assignments.insert(
                    source.logical_name().to_string(),
                    "service-manager:auto".into(),
                );
            }
            RelSourceKind::Server => {}
""",
    """            RelSourceKind::Service => {
                services.push(source.id().clone());
                service_assignments.insert(
                    source.logical_name().to_string(),
                    "service-manager:auto".into(),
                );
            }
            RelSourceKind::Field => fields.push(source.id().clone()),
            RelSourceKind::Server => {}
""",
    "RELC Field manifest classification",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """        routes,
        modules,
        services,
        sources,
""",
    """        routes,
        modules,
        services,
        fields,
        sources,
""",
    "RuntimeImage Field initialization",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """        RelSourceKind::Service => Parser::new(tokens)
            .parse_service_file()
            .map(CompiledUnit::Service),
        RelSourceKind::Server => unreachable!("Server REL is compiled before registry parsing"),
""",
    """        RelSourceKind::Service => Parser::new(tokens)
            .parse_service_file()
            .map(CompiledUnit::Service),
        RelSourceKind::Field => Parser::new(tokens)
            .parse_field_file()
            .map(CompiledUnit::Field),
        RelSourceKind::Server => unreachable!("Server REL is compiled before registry parsing"),
""",
    "parse registered Field source",
)

# Field source imports are strict allowlist, not a blacklist. No ambient host or
# application dependencies are permitted in this deterministic request layer.
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """fn validate_capabilities(
    source: &SourceId,
    kind: RelSourceKind,
    imports: &[ImportTarget],
) -> Result<(), RelcError> {
    for import in imports {
""",
    r'''fn validate_capabilities(
    source: &SourceId,
    kind: RelSourceKind,
    imports: &[ImportTarget],
) -> Result<(), RelcError> {
    if kind == RelSourceKind::Field {
        for import in imports {
            match import_base(import) {
                ImportTarget::Builtin(name) if matches!(name.as_str(), "math" | "regx") => {}
                ImportTarget::BuiltinFunction { module, function }
                    if matches!(module.as_str(), "math" | "regx")
                        && builtin_function_exists(module, function) => {}
                _ => {
                    return Err(RelcError::Capability {
                        code: "RELC2101",
                        source: source.clone(),
                        message: "Field REL is pure request preprocessing; only deterministic `math` and `regx` imports are allowed (no DB/network/service/module/filesystem/ENV/crypto/randomness)".into(),
                    });
                }
            }
        }
        return Ok(());
    }

    for import in imports {
''',
    "Field import allowlist",
)

replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """        if matches!(
            base,
            ImportTarget::Service(_) | ImportTarget::ServiceFunction { .. }
        ) && kind == RelSourceKind::Route
""",
    r'''        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } = base {
            if name == "field" && kind != RelSourceKind::Route {
                return Err(RelcError::Capability {
                    code: "RELC2101",
                    source: source.clone(),
                    message: "FieldManager request resolution is Route-owned; reusable logic belongs in a .field source imported by the Route".into(),
                });
            }
        }
        if matches!(
            base,
            ImportTarget::Service(_) | ImportTarget::ServiceFunction { .. }
        ) && kind == RelSourceKind::Route
''',
    "FieldManager Route ownership",
)

replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """                ImportTarget::Service(service) | ImportTarget::ServiceFunction { service, .. }
                    if registry
                        .get_logical(RelSourceKind::Service, service)
                        .is_none() =>
                {
                    return Err(RelcError::Link(format!(
                        "{source_id} imports missing service `{service}`"
                    )));
                }
                _ => {}
""",
    r'''                ImportTarget::Service(service) | ImportTarget::ServiceFunction { service, .. }
                    if registry
                        .get_logical(RelSourceKind::Service, service)
                        .is_none() =>
                {
                    return Err(RelcError::Link(format!(
                        "{source_id} imports missing service `{service}`"
                    )));
                }
                ImportTarget::BuiltinFunction { module, function }
                    if module == "field"
                        && !matches!(function.as_str(), "required" | "optional" | "has" | "dynamic")
                        && registry
                            .get_logical(RelSourceKind::Field, function)
                            .is_none() =>
                {
                    return Err(RelcError::Link(format!(
                        "{source_id} imports missing FieldManager source `{function}.field`"
                    )));
                }
                _ => {}
''',
    "Field import target linking",
)

replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """enum CompiledUnit {
    Route(RouteFile),
    Module(ModuleFile),
    Service(ServiceProgram),
    Server(ServerProgram),
}
""",
    """enum CompiledUnit {
    Route(RouteFile),
    Module(ModuleFile),
    Service(ServiceProgram),
    Field(FieldFile),
    Server(ServerProgram),
}
""",
    "Compiled Field unit",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """            Self::Service(file) => RuntimeExecutable::Service(Arc::new(file.clone())),
            Self::Server(file) => RuntimeExecutable::Server(Arc::new(file.clone())),
""",
    """            Self::Service(file) => RuntimeExecutable::Service(Arc::new(file.clone())),
            Self::Field(file) => RuntimeExecutable::Field(Arc::new(file.clone())),
            Self::Server(file) => RuntimeExecutable::Server(Arc::new(file.clone())),
""",
    "Field runtime executable",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """            Self::Service(file) => &file.imports,
            Self::Server(file) => &file.imports,
""",
    """            Self::Service(file) => &file.imports,
            Self::Field(file) => &file.imports,
            Self::Server(file) => &file.imports,
""",
    "Field compiled imports",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """            Self::Service(file) => file.exports.clone(),
            Self::Server(_) => Vec::new(),
""",
    """            Self::Service(file) => file.exports.clone(),
            Self::Field(file) => file.resolver.as_ref().map(|_| vec!["resolve".into()]).unwrap_or_default(),
            Self::Server(_) => Vec::new(),
""",
    "Field compiled exports",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """            Self::Service(file) => {
                add_functions(&mut out, &file.functions);
                for method in &file.lifecycle {
                    out.push((format!("Service.{}", method.verb), method.body.clone()));
                }
                for class in &file.classes {
                    for method in &class.methods {
                        out.push((
                            format!("{}.{}", class.name, method.name),
                            method.body.clone(),
                        ));
                    }
                }
            }
            Self::Server(file) => add_functions(&mut out, &file.functions),
""",
    """            Self::Service(file) => {
                add_functions(&mut out, &file.functions);
                for method in &file.lifecycle {
                    out.push((format!("Service.{}", method.verb), method.body.clone()));
                }
                for class in &file.classes {
                    for method in &class.methods {
                        out.push((
                            format!("{}.{}", class.name, method.name),
                            method.body.clone(),
                        ));
                    }
                }
            }
            Self::Field(file) => {
                if let Some(resolver) = &file.resolver {
                    out.push(("resolve".into(), resolver.body.clone()));
                }
            }
            Self::Server(file) => add_functions(&mut out, &file.functions),
""",
    "Field symbol body",
)

# ---------------------------------------------------------------------------
# Public exports + tests.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    """pub use ast::{
    BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceProgram,
    Statement, Value,
};""",
    """pub use ast::{
    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,
    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceProgram, Statement, Value,
};""",
    "public Field AST exports",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    """pub fn parse_service_source(source: &str) -> Result<ServiceProgram, ParseError> {
""",
    r'''pub fn parse_field_source(source: &str) -> Result<FieldFile, ParseError> {
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .map_err(|error| ParseError {
            message: error.message,
            line: error.line,
            column: error.column,
        })?;
    parser::Parser::new(tokens).parse_field_file()
}

pub fn parse_service_source(source: &str) -> Result<ServiceProgram, ParseError> {
''',
    "public Field parser",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    """    #[test]
    fn parses_executable_service_program() {
""",
    r'''    #[test]
    fn parses_field_manager_declarative_source() {
        let file = parse_field_source(
            r#"
            :import[math, regx]
            :field[source = query, key = "awesomeness", optional = true]
            resolve {
                page = optional("page", type = int, default = 1);
                debug = optional("debug", type = bool, default = false);
                tracking = dynamic("utm_", stripPrefix = true);
                cookie = required("cookie");
            }
            "#,
        )
        .expect("field parse failed");
        assert_eq!(file.imports.len(), 2);
        assert_eq!(file.directive.source, "query");
        assert_eq!(file.directive.key.as_deref(), Some("awesomeness"));
        assert!(file.directive.optional);
        assert_eq!(file.bindings.len(), 4);
        assert_eq!(file.bindings[0].value_type, FieldValueType::Int);
        assert_eq!(file.bindings[2].mode, FieldBindingMode::Dynamic);
        assert!(file.bindings[2].strip_prefix);
    }

    #[test]
    fn parses_field_manager_executable_resolver_and_import_shorthand() {
        let field = parse_field_source(
            r#"
            :import[math, regx]
            field { source = query; type = string; required = true; key = "slug"; }
            resolve(raw, context) { return math.trim(raw); }
            "#,
        )
        .expect("field resolver parse failed");
        assert_eq!(field.resolver.as_ref().unwrap().params, vec!["raw", "context"]);

        let route = parse(
            r#":import[field, field:awesomeness]
               class Route { get(req) { return true; } }"#,
        );
        assert_eq!(route.imports.len(), 2);
        assert!(matches!(
            &route.imports[1],
            ImportTarget::BuiltinFunction { module, function }
                if module == "field" && function == "awesomeness"
        ));
    }

    #[test]
    fn deterministic_math_and_regx_helpers_execute() {
        let imports = vec![ImportTarget::Builtin("math".into()), ImportTarget::Builtin("regx".into())];
        let modules = ModuleRegistry::from_imports(&imports);
        assert!(matches!(
            modules.call("math", "trim", &[Value::String("  hi  ".into())]).unwrap(),
            Value::String(value) if value == "hi"
        ));
        assert!(matches!(
            modules.call(
                "regx",
                "test",
                &[Value::String("^[a-z]+$".into()), Value::String("hello".into())]
            ).unwrap(),
            Value::Bool(true)
        ));
    }

    #[test]
    fn parses_executable_service_program() {
''',
    "Field parser/helper tests",
)

# RELC coverage: first-class source + allowlist + reusable import linking.
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """    #[test]
    fn direct_http_get_links_native_artifact_with_exact_network_requirement() {
""",
    r'''    #[test]
    fn field_sources_link_as_first_class_pure_runtime_sources() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Field,
                "awesomeness",
                "api/awesomeness.field",
                r#":import[math, regx]
                   :field[source = query, optional = true]
                   resolve { page = optional("page", type = int, default = 1); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "uses-field",
                "api/uses-field.route",
                r#":import[field:awesomeness]
                   class Route { get(req) { return true; } }"#,
            ),
        ];
        let image = compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        assert_eq!(image.fields.len(), 1);
        let field = &image.fields[0];
        assert!(image.field_file(field).is_some());
        assert!(image.capability_requirements(field).unwrap().is_empty());
        assert!(image.source(field).is_some_and(|source| source.kind == RelSourceKind::Field));
    }

    #[test]
    fn field_sources_reject_ambient_capabilities() {
        for forbidden in ["http", "storage", "service:db", "crypto", "ENV"] {
            let source = format!(
                ":import[{forbidden}]\n:field[source = query, key = \"id\"]\nresolve {{ id = required(\"id\"); }}"
            );
            let fields = vec![PhysicalRelSource::new(
                RelSourceKind::Field,
                "unsafe",
                "api/unsafe.field",
                source,
            )];
            let error = compile_runtime_image("server Main {}", fields, &serde_json::json!({}))
                .expect_err("Field REL must reject ambient capabilities");
            assert_eq!(error.code(), "RELC2101");
        }
    }

    #[test]
    fn route_import_of_missing_field_source_fails_link() {
        let routes = vec![PhysicalRelSource::new(
            RelSourceKind::Route,
            "missing-field",
            "api/missing-field.route",
            r#":import[field:ghost]
               class Route { get(req) { return true; } }"#,
        )];
        let error = compile_runtime_image("server Main {}", routes, &serde_json::json!({}))
            .expect_err("missing .field source must fail linking");
        assert!(error.to_string().contains("ghost.field"));
    }

    #[test]
    fn direct_http_get_links_native_artifact_with_exact_network_requirement() {
''',
    "Field RELC tests",
)

# Source registry test hook by extending path classification assertion block.
replace_once(
    "engine/crates/route-engine/src/source_registry.rs",
    """        assert_eq!(
            RelSourceKind::from_path(Path::new("api/account.route")),
            Some(RelSourceKind::Route)
        );
""",
    """        assert_eq!(
            RelSourceKind::from_path(Path::new("api/account.route")),
            Some(RelSourceKind::Route)
        );
        assert_eq!(
            RelSourceKind::from_path(Path::new("api/account.field")),
            Some(RelSourceKind::Field)
        );
""",
    "Field source classification test",
)

# Documentation baseline for the compiler foundation. Request-time resolution is
# intentionally left to the next slice; say that explicitly rather than implying
# the runtime already wires field values into a Route.
Path("doc/field-manager.md").write_text(r'''# FieldManager

FieldManager is RBE's typed URL/query-field layer. `.field` is a first-class REL source with normal shared REL expression grammar but a deliberately restricted capability surface.

## Imports

Route-local direct mode:

```text
:import[field]
```

Reusable sibling resolver:

```text
:import[field:awesomeness]
```

This binds the reusable `awesomeness.field` resolver to the FieldManager namespace (`field.awesomeness()` once request-time resolution is enabled).

A `.field` source may import only deterministic local helpers:

```text
:import[math, regx]
```

Field REL cannot import DB/network/service/module/filesystem/ENV/crypto/random capabilities.

## Declarative `.field`

```text
:import[math, regx]
:field[source = query, key = "awesomeness", optional = true]
resolve {
    page = optional("page", type = int, default = 1);
    debug = optional("debug", type = bool, default = false);
    tracking = dynamic("utm_", stripPrefix = true);
    cookie = required("cookie");
}
```

`required(...)`, `optional(...)`, and `dynamic(...)` compile into typed Field IR. `dynamic(prefix, stripPrefix = true)` represents a prefix family such as `utm_*`.

An empty `:field[]` uses the normal defaults (`source = query`, string type, required unless marked optional); a source must still provide a key, bindings, or an executable resolver.

## Executable deterministic resolver

```text
:import[math, regx]
field {
    source = query;
    type = string;
    required = true;
    key = "slug";
}
resolve(raw, context) {
    return math.trim(raw);
}
```

The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` provides bounded deterministic regex matching and `regx.raw(pattern)` validates/returns a raw regex pattern.

## Current implementation boundary

FLD-001 makes `.field` a first-class RELC source, discovers physical/embedded Field sources, validates its pure import allowlist, links `field:NAME` imports, carries Field AST/IR in the immutable Runtime Image, and provides deterministic `math`/`regx` helpers.

Request-time FieldManager resolution, route-local `fields { ... }`, structured required-field HTTP 400 responses, optional `null`, and `field.NAME()` execution are the next runtime slice. They must not be documented as active before that slice lands.
''', encoding="utf-8")

replace_once(
    "doc/README.md",
    """- [`storage.md`](storage.md) — Environment Storage, unified `storage.write(...)`, frozen `$$/` ProjectRoot semantics, encoding, and Data-Level status.
""",
    """- [`storage.md`](storage.md) — Environment Storage, unified `storage.write(...)`, frozen `$$/` ProjectRoot semantics, encoding, and Data-Level status.
- [`field-manager.md`](field-manager.md) — first-class `.field` sources, typed query-field IR, deterministic helpers, and current runtime boundary.
""",
    "FieldManager docs index",
)
