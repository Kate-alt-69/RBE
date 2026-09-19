from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


FIELD_MANAGER_RS = r'''//! Request-time FieldManager resolution.
//!
//! Query parsing remains owned by the HTTP edge. This layer consumes the
//! immutable REL request snapshot, applies validated `.field` programs once,
//! and exposes the same resolved values to the Route `field` namespace.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::{
    FieldBinding, FieldBindingMode, FieldFile, FieldValueType, ImportTarget, ModuleFile, RouteFile,
    Value,
};
use crate::module_eval::{ModuleEvalError, ModuleExecutor};
use crate::module_runtime::ModuleProgram;
use crate::runtime_image::RuntimeImage;
use crate::source_registry::RelSourceKind;

const DIRECT_FIELD_FUNCTIONS: &[&str] = &["required", "optional", "has", "dynamic"];

#[derive(Debug, Clone)]
pub struct FieldResolveError {
    pub code: &'static str,
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for FieldResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for FieldResolveError {}

#[derive(Debug, Clone, Default)]
pub struct FieldRuntimeContext {
    query: HashMap<String, Value>,
    resolved: HashMap<String, Value>,
    allowed_resolvers: HashSet<String>,
    direct_enabled: bool,
}

impl FieldRuntimeContext {
    pub fn resolved_object(&self) -> Value {
        Value::Object(self.resolved.clone())
    }

    pub fn call(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        if DIRECT_FIELD_FUNCTIONS.contains(&function) {
            if !self.direct_enabled {
                return Err(field_module_error(format!(
                    "field.{function}() requires `:import[field]`"
                )));
            }
            return self.call_direct(function, args);
        }

        if !self.allowed_resolvers.contains(function) {
            return Err(field_module_error(format!(
                "FieldManager resolver {function:?} was not imported by this Route"
            )));
        }
        if !args.is_empty() {
            return Err(field_module_error(format!(
                "field.{function}() takes no arguments; it reads the current request snapshot"
            )));
        }
        self.resolved.get(function).cloned().ok_or_else(|| {
            field_module_error(format!(
                "FieldManager resolver {function:?} has no request-time value"
            ))
        })
    }

    fn call_direct(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        let key = string_arg(args.first(), "FieldManager key/prefix")?;
        match function {
            "has" => {
                require_arity(function, args, 1, 1)?;
                Ok(Value::Bool(self.query.contains_key(key)))
            }
            "required" => {
                require_arity(function, args, 1, 1)?;
                self.query.get(key).cloned().ok_or_else(|| {
                    field_module_error(format!("required query field {key:?} is missing"))
                })
            }
            "optional" => {
                require_arity(function, args, 1, 1)?;
                Ok(self.query.get(key).cloned().unwrap_or(Value::Null))
            }
            "dynamic" => {
                require_arity(function, args, 1, 2)?;
                let strip_prefix = match args.get(1) {
                    None => false,
                    Some(Value::Bool(value)) => *value,
                    Some(_) => {
                        return Err(field_module_error(
                            "field.dynamic() second argument must be a boolean",
                        ));
                    }
                };
                Ok(Value::Object(dynamic_values(&self.query, key, strip_prefix)))
            }
            _ => unreachable!("direct FieldManager function allowlist checked above"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FieldRoutePlan {
    direct_enabled: bool,
    resolvers: Vec<(String, Arc<FieldFile>)>,
}

impl FieldRoutePlan {
    pub(crate) fn from_route(
        image: &RuntimeImage,
        route: &RouteFile,
        route_logical_name: &str,
    ) -> Result<Self, String> {
        let mut direct_enabled = false;
        let mut resolver_names = HashSet::new();
        let mut resolvers = Vec::new();

        for import in &route.imports {
            match import_base(import) {
                ImportTarget::Builtin(module) if module == "field" => direct_enabled = true,
                ImportTarget::BuiltinFunction { module, function } if module == "field" => {
                    if DIRECT_FIELD_FUNCTIONS.contains(&function.as_str()) {
                        direct_enabled = true;
                        continue;
                    }
                    if !resolver_names.insert(function.clone()) {
                        continue;
                    }
                    let source = field_logical_candidates(route_logical_name, function)
                        .into_iter()
                        .find_map(|logical| {
                            image.sources.iter().find(|source| {
                                source.kind == RelSourceKind::Field && source.logical_name == logical
                            })
                        })
                        .ok_or_else(|| {
                            format!(
                                "Route {route_logical_name:?} imports missing FieldManager source {function:?}"
                            )
                        })?;
                    let field = image.field_file(&source.id).ok_or_else(|| {
                        format!("FieldManager source {} has no executable snapshot", source.id)
                    })?;
                    resolvers.push((function.clone(), field));
                }
                _ => {}
            }
        }

        Ok(Self {
            direct_enabled,
            resolvers,
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.direct_enabled || !self.resolvers.is_empty()
    }

    pub(crate) async fn resolve(
        &self,
        request: &Value,
        program: &ModuleProgram,
    ) -> Result<Arc<FieldRuntimeContext>, FieldResolveError> {
        let query = request_query(request)?.clone();
        let mut resolved = HashMap::new();
        let mut allowed_resolvers = HashSet::new();
        for (name, file) in &self.resolvers {
            let value = resolve_field_file(file.as_ref(), request, program, name).await?;
            resolved.insert(name.clone(), value);
            allowed_resolvers.insert(name.clone());
        }
        Ok(Arc::new(FieldRuntimeContext {
            query,
            resolved,
            allowed_resolvers,
            direct_enabled: self.direct_enabled,
        }))
    }
}

pub(crate) fn field_logical_candidates(route_logical_name: &str, name: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    if let Some((directory, _)) = route_logical_name.rsplit_once('/') {
        out.push(format!("{directory}/{name}"));
    }
    if !out.iter().any(|candidate| candidate == name) {
        out.push(name.to_string());
    }
    out
}

async fn resolve_field_file(
    file: &FieldFile,
    request: &Value,
    program: &ModuleProgram,
    resolver_name: &str,
) -> Result<Value, FieldResolveError> {
    let query = request_query(request)?;

    if !file.bindings.is_empty() {
        let mut output = HashMap::with_capacity(file.bindings.len());
        for binding in &file.bindings {
            output.insert(
                binding.name.clone(),
                resolve_binding(query, binding, resolver_name)?,
            );
        }
        return Ok(Value::Object(output));
    }

    let raw = if let Some(key) = file.directive.key.as_deref() {
        match query.get(key) {
            Some(value) => match coerce_value(value, file.directive.value_type) {
                Ok(value) => value,
                Err(message) if file.directive.optional => Value::Null,
                Err(message) => {
                    return Err(resolve_error(
                        "FLD4002",
                        resolver_name,
                        format!("query field {key:?} is invalid: {message}"),
                    ));
                }
            },
            None if file.directive.optional => Value::Null,
            None => {
                return Err(resolve_error(
                    "FLD4001",
                    resolver_name,
                    format!("required query field {key:?} is missing"),
                ));
            }
        }
    } else {
        Value::Object(query.clone())
    };

    let Some(resolver) = file.resolver.clone() else {
        return Ok(raw);
    };

    let args = match resolver.params.len() {
        0 => Vec::new(),
        1 => vec![raw],
        2 => vec![raw, request.clone()],
        count => {
            return Err(resolve_error(
                "FLD5001",
                resolver_name,
                format!("compiled Field resolver unexpectedly has {count} parameters"),
            ));
        }
    };
    let synthetic = Arc::new(ModuleFile {
        imports: file.imports.clone(),
        functions: Vec::new(),
        exports: Vec::new(),
    });
    ModuleExecutor::new(program)
        .call_inline_definition(synthetic, resolver, args)
        .await
        .map_err(|error| {
            resolve_error(
                "FLD4003",
                resolver_name,
                format!("Field resolver rejected the request: {}", error.message),
            )
        })
}

fn resolve_binding(
    query: &HashMap<String, Value>,
    binding: &FieldBinding,
    resolver_name: &str,
) -> Result<Value, FieldResolveError> {
    if binding.mode == FieldBindingMode::Dynamic {
        return Ok(Value::Object(dynamic_values(
            query,
            &binding.lookup,
            binding.strip_prefix,
        )));
    }

    let Some(raw) = query.get(&binding.lookup) else {
        return match binding.mode {
            FieldBindingMode::Required => Err(resolve_error(
                "FLD4001",
                &binding.name,
                format!(
                    "required query field {:?} for resolver {resolver_name:?} is missing",
                    binding.lookup
                ),
            )),
            FieldBindingMode::Optional => Ok(binding.default.clone().unwrap_or(Value::Null)),
            FieldBindingMode::Dynamic => unreachable!(),
        };
    };

    match coerce_value(raw, binding.value_type) {
        Ok(value) => Ok(value),
        Err(message) if binding.mode == FieldBindingMode::Optional => Ok(Value::Null),
        Err(message) => Err(resolve_error(
            "FLD4002",
            &binding.name,
            format!(
                "query field {:?} for resolver {resolver_name:?} is invalid: {message}",
                binding.lookup
            ),
        )),
    }
}

fn coerce_value(value: &Value, value_type: FieldValueType) -> Result<Value, String> {
    let Value::String(raw) = value else {
        return Err("query value is not a string".into());
    };
    match value_type {
        FieldValueType::String => Ok(Value::String(raw.clone())),
        FieldValueType::Int => raw
            .parse::<i64>()
            .map(|value| Value::Number(value as f64))
            .map_err(|_| "expected an integer".into()),
        FieldValueType::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err("expected true or false".into()),
        },
    }
}

fn dynamic_values(
    query: &HashMap<String, Value>,
    prefix: &str,
    strip_prefix: bool,
) -> HashMap<String, Value> {
    query
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(prefix).map(|suffix| {
                let key = if strip_prefix {
                    suffix.to_string()
                } else {
                    key.clone()
                };
                (key, value.clone())
            })
        })
        .collect()
}

fn request_query(request: &Value) -> Result<&HashMap<String, Value>, FieldResolveError> {
    let Value::Object(request) = request else {
        return Err(resolve_error(
            "FLD5000",
            "request",
            "FieldManager request snapshot is not an object",
        ));
    };
    match request.get("query") {
        Some(Value::Object(query)) => Ok(query),
        _ => Err(resolve_error(
            "FLD5000",
            "query",
            "FieldManager request snapshot has no query object",
        )),
    }
}

fn string_arg<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, ModuleEvalError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(field_module_error(format!(
            "{label} must be a non-empty string"
        ))),
    }
}

fn require_arity(
    function: &str,
    args: &[Value],
    min: usize,
    max: usize,
) -> Result<(), ModuleEvalError> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(field_module_error(format!(
            "field.{function}() expects {} argument(s), got {}",
            if min == max {
                min.to_string()
            } else {
                format!("{min}..={max}")
            },
            args.len()
        )))
    }
}

fn resolve_error(
    code: &'static str,
    field: impl Into<String>,
    message: impl Into<String>,
) -> FieldResolveError {
    FieldResolveError {
        code,
        field: field.into(),
        message: message.into(),
    }
}

fn field_module_error(message: impl Into<String>) -> ModuleEvalError {
    ModuleEvalError {
        code: "FLD4000",
        message: message.into(),
    }
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => target.as_ref(),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use crate::lexer::Lexer;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

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

    fn field(source: &str) -> FieldFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_field_file().unwrap()
    }

    fn request(entries: &[(&str, &str)]) -> Value {
        Value::Object(HashMap::from([(
            "query".into(),
            Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).into(), Value::String((*value).into())))
                    .collect(),
            ),
        )]))
    }

    #[test]
    fn declarative_fields_coerce_defaults_and_dynamic_prefixes() {
        let file = field(
            r#":field[source = query, optional = true]
               resolve {
                   page = optional("page", type = int, default = 1);
                   debug = optional("debug", type = bool, default = false);
                   tracking = dynamic("utm_", stripPrefix = true);
                   cookie = required("cookie");
               }"#,
        );
        let program = ModuleProgram::from_runtime_image(&RuntimeImage::test_empty()).unwrap();
        let value = block_on_ready(resolve_field_file(
            &file,
            &request(&[("debug", "true"), ("utm_source", "chat"), ("cookie", "abc")]),
            &program,
            "awesome",
        ))
        .unwrap();
        let Value::Object(values) = value else { panic!("expected object") };
        assert!(matches!(values.get("page"), Some(Value::Number(value)) if *value == 1.0));
        assert!(matches!(values.get("debug"), Some(Value::Bool(true))));
        let Some(Value::Object(tracking)) = values.get("tracking") else { panic!("tracking") };
        assert!(matches!(tracking.get("source"), Some(Value::String(value)) if value == "chat"));
    }

    #[test]
    fn required_and_optional_invalid_fields_follow_failure_contract() {
        let required = field(
            r#":field[source = query]
               resolve { page = required("page", type = int); }"#,
        );
        let optional = field(
            r#":field[source = query]
               resolve { page = optional("page", type = int); }"#,
        );
        let program = ModuleProgram::from_runtime_image(&RuntimeImage::test_empty()).unwrap();
        let error = block_on_ready(resolve_field_file(
            &required,
            &request(&[("page", "nope")]),
            &program,
            "required",
        ))
        .unwrap_err();
        assert_eq!(error.code, "FLD4002");
        let value = block_on_ready(resolve_field_file(
            &optional,
            &request(&[("page", "nope")]),
            &program,
            "optional",
        ))
        .unwrap();
        let Value::Object(values) = value else { panic!("expected object") };
        assert!(matches!(values.get("page"), Some(Value::Null)));
    }

    #[test]
    fn direct_field_namespace_reads_the_same_query_snapshot() {
        let context = FieldRuntimeContext {
            query: HashMap::from([
                ("cookie".into(), Value::String("abc".into())),
                ("utm_source".into(), Value::String("chat".into())),
            ]),
            resolved: HashMap::from([("awesome".into(), Value::Number(7.0))]),
            allowed_resolvers: HashSet::from(["awesome".into()]),
            direct_enabled: true,
        };
        assert!(matches!(
            context.call("required", &[Value::String("cookie".into())]).unwrap(),
            Value::String(value) if value == "abc"
        ));
        assert!(matches!(context.call("awesome", &[]).unwrap(), Value::Number(7.0)));
    }
}
'''
Path("engine/crates/route-engine/src/field_manager.rs").write_text(FIELD_MANAGER_RS, encoding="utf-8")

# Wire the runtime module/public error type.
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "mod discovery;\npub mod embedded_rel;",
    "mod discovery;\nmod field_manager;\npub mod embedded_rel;",
    "FieldManager runtime module",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use execution_tracker::{\n",
    "pub use field_manager::{FieldResolveError, FieldRuntimeContext};\npub use execution_tracker::{\n",
    "FieldManager public runtime exports",
)

# Field resolver shorthand binds a namespace (`field.NAME()`), not a stray
# direct function. Aliases still deliberately override the binding name.
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """        ImportTarget::BuiltinFunction { function, .. } => function.clone(),
""",
    """        ImportTarget::BuiltinFunction { module, function } => {
            if module == "field" {
                "field".into()
            } else {
                function.clone()
            }
        }
""",
    "Field namespace binding name",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """    Regx,
    VideoManager,
""",
    """    Regx,
    Field,
    VideoManager,
""",
    "Field builtin module kind",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '                        "regx" => ModuleKind::Builtin(BuiltinModule::Regx),\n                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),',
    '                        "regx" => ModuleKind::Builtin(BuiltinModule::Regx),\n                        "field" => ModuleKind::Builtin(BuiltinModule::Field),\n                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),',
    "Field namespace registry",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''                ImportTarget::BuiltinFunction { module, function } => {
                    let kind = match module.as_str() {''',
    '''                ImportTarget::BuiltinFunction { module, function } => {
                    if module == "field" {
                        modules
                            .entry(binding_name(target))
                            .or_insert(ModuleKind::Builtin(BuiltinModule::Field));
                        continue;
                    }
                    let kind = match module.as_str() {''',
    "Field resolver registry namespace",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    """            ModuleKind::Builtin(BuiltinModule::Regx) => call_regx(function_name, args),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {
""",
    """            ModuleKind::Builtin(BuiltinModule::Regx) => call_regx(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Field) => Err(ModuleError {
                message: format!(
                    "field.{function_name}() requires the active request FieldManager context"
                ),
            }),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {
""",
    "Field namespace fallback",
)

# Analyzer merges multiple reusable Field imports into one namespace binding.
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    """            ImportTarget::BuiltinFunction { module, function } => {
                if !builtin_function_exists(module, function) {
""",
    """            ImportTarget::BuiltinFunction { module, function } => {
                if !builtin_function_exists(module, function) {
""",
    "analyzer anchor sanity",
)
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    """                SymbolKind::DirectFunction
            }
            ImportTarget::Custom(_) => SymbolKind::Module,
""",
    """                if module == "field" {
                    SymbolKind::Module
                } else {
                    SymbolKind::DirectFunction
                }
            }
            ImportTarget::Custom(_) => SymbolKind::Module,
""",
    "Field analyzer namespace kind",
)
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    """        if globals
            .insert(name.clone(), Symbol { kind, used: false })
            .is_some()
        {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2002",
                message: format!("duplicate import binding `{name}`"),
                symbol: Some(name),
            });
        }
""",
    """        let merged_field_namespace = name == "field"
            && kind == SymbolKind::Module
            && globals
                .get(&name)
                .is_some_and(|existing| existing.kind == SymbolKind::Module);
        if !merged_field_namespace
            && globals
                .insert(name.clone(), Symbol { kind, used: false })
                .is_some()
        {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2002",
                message: format!("duplicate import binding `{name}`"),
                symbol: Some(name),
            });
        }
""",
    "merge Field analyzer namespace imports",
)

# AOT validation sees field:NAME as the namespace as well.
replace_once(
    "engine/crates/route-engine/src/transpiler.rs",
    """                ImportTarget::BuiltinFunction { .. }
                | ImportTarget::CustomFunction { .. }
""",
    """                ImportTarget::BuiltinFunction { module, .. } if module == "field" => {
                    names.insert(crate::modules::binding_name(import), NameKind::Module);
                }
                ImportTarget::BuiltinFunction { .. }
                | ImportTarget::CustomFunction { .. }
""",
    "Field transpiler namespace",
)

# Async evaluator routes field:NAME member calls through the local runtime host.
replace_once(
    "engine/crates/route-engine/src/module_eval.rs",
    """                ImportTarget::BuiltinFunction { module, function } => {
                    builtin_functions.insert(binding, (module.clone(), function.clone()));
                }
""",
    """                ImportTarget::BuiltinFunction { module, function } => {
                    if module == "field" {
                        builtin_modules.insert(binding, module.clone());
                    } else {
                        builtin_functions.insert(binding, (module.clone(), function.clone()));
                    }
                }
""",
    "Field evaluator namespace",
)

# Local host capability gets one immutable FieldRuntimeContext per request.
replace_once(
    "engine/crates/route-engine/src/video_host.rs",
    "use crate::ast::Value;\n",
    "use crate::ast::Value;\nuse crate::field_manager::FieldRuntimeContext;\n",
    "Field host runtime import",
)
replace_once(
    "engine/crates/route-engine/src/video_host.rs",
    """pub struct RuntimeHostCapabilities {
    video: VideoLanguage,
    image: Arc<RuntimeImage>,
}
""",
    """pub struct RuntimeHostCapabilities {
    video: VideoLanguage,
    image: Arc<RuntimeImage>,
    fields: Option<Arc<FieldRuntimeContext>>,
}
""",
    "Field host context slot",
)
replace_once(
    "engine/crates/route-engine/src/video_host.rs",
    """        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
        }
    }
}
""",
    """        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
            fields: None,
        }
    }

    pub fn from_state_image_and_fields(
        state: &AppState,
        image: Arc<RuntimeImage>,
        fields: Arc<FieldRuntimeContext>,
    ) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
            fields: Some(fields),
        }
    }
}
""",
    "Field host constructor",
)
replace_once(
    "engine/crates/route-engine/src/video_host.rs",
    """        Box::pin(async move {
            if module == "ENV" {
""",
    """        Box::pin(async move {
            if module == "field" {
                let fields = self.fields.as_ref().ok_or_else(|| ModuleEvalError {
                    code: "FLD4000",
                    message: "FieldManager context is unavailable for this request".into(),
                })?;
                return fields.call(function, &args).map(Some);
            }
            if module == "ENV" {
""",
    "Field host dispatch",
)

# RELC resolves field:NAME relative to the importing Route directory first,
# then falls back to the API root logical name.
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    "use crate::execution_tracker::InvocationTracker;\n",
    "use crate::execution_tracker::InvocationTracker;\nuse crate::field_manager::field_logical_candidates;\n",
    "RELC Field logical resolver import",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    '''                ImportTarget::BuiltinFunction { module, function }
                    if module == "field"
                        && !matches!(
                            function.as_str(),
                            "required" | "optional" | "has" | "dynamic"
                        )
                        && registry
                            .get_logical(RelSourceKind::Field, function)
                            .is_none() =>
                {
                    return Err(RelcError::Link(format!(
                        "{source_id} imports missing FieldManager source `{function}.field`"
                    )));
                }
''',
    '''                ImportTarget::BuiltinFunction { module, function }
                    if module == "field"
                        && !matches!(
                            function.as_str(),
                            "required" | "optional" | "has" | "dynamic"
                        ) =>
                {
                    let owner = registry.get(source_id).ok_or_else(|| {
                        RelcError::Link(format!("compiled source {source_id} is not registered"))
                    })?;
                    let found = field_logical_candidates(owner.logical_name(), function)
                        .into_iter()
                        .any(|logical| {
                            registry
                                .get_logical(RelSourceKind::Field, &logical)
                                .is_some()
                        });
                    if !found {
                        return Err(RelcError::Link(format!(
                            "{source_id} imports missing FieldManager source `{function}.field` (checked sibling and API-root Field sources)"
                        )));
                    }
                }
''',
    "relative Field import validation",
)

# HTTP edge resolves FieldManager before any Route interpreter/native execution.
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    "use crate::lexer::Lexer;\n",
    "use crate::field_manager::{FieldResolveError, FieldRoutePlan};\nuse crate::lexer::Lexer;\n",
    "Field runtime discovery import",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """struct RouteHandlerPlan {
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    takes_request: bool,
}
""",
    """struct RouteHandlerPlan {
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    field_plan: Arc<FieldRoutePlan>,
    takes_request: bool,
}
""",
    "Route Field plan",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """        module_program,
        native_plan,
        takes_request,
    } = plan;
""",
    """        module_program,
        native_plan,
        field_plan,
        takes_request,
    } = plan;
""",
    "execute Field plan destructure",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    '''    let args = if takes_request {
        match request_value(&state, params, query, request).await {
            Ok(request) => vec![request],
            Err(response) => return *response,
        }
    } else {
        // Even handlers without a request parameter must consume the request
        // body so connection reuse/backpressure behavior remains predictable.
        let _ = to_bytes(
            request.into_body(),
            state.config.security.max_json_payload_bytes,
        )
        .await;
        Vec::new()
    };
''',
    '''    let needs_snapshot = takes_request || field_plan.is_active();
    let mut request_snapshot = if needs_snapshot {
        match request_value(&state, params, query, request).await {
            Ok(request) => Some(request),
            Err(response) => return *response,
        }
    } else {
        // Even handlers without a request parameter must consume the request
        // body so connection reuse/backpressure behavior remains predictable.
        let _ = to_bytes(
            request.into_body(),
            state.config.security.max_json_payload_bytes,
        )
        .await;
        None
    };

    let field_context = if field_plan.is_active() {
        let snapshot = request_snapshot
            .as_ref()
            .expect("FieldManager-active Route always builds a request snapshot");
        match field_plan.resolve(snapshot, module_program.as_ref()).await {
            Ok(context) => Some(context),
            Err(error) => return field_validation_response(error),
        }
    } else {
        None
    };

    if let (Some(snapshot), Some(fields)) = (request_snapshot.as_mut(), field_context.as_ref()) {
        if let Value::Object(request) = snapshot {
            request.insert("fields".into(), fields.resolved_object());
        }
    }
    let args = if takes_request {
        vec![request_snapshot.take().unwrap_or(Value::Null)]
    } else {
        Vec::new()
    };
''',
    "pre-route Field resolution",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    '''    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(RuntimeHostCapabilities::from_state_and_image(&state, image)),
    );
''',
    '''    let host = match field_context {
        Some(fields) => RuntimeHostCapabilities::from_state_image_and_fields(&state, image, fields),
        None => RuntimeHostCapabilities::from_state_and_image(&state, image),
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(host),
    );
''',
    "Field-aware runtime host",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    '''        Err(err) => {
            tracing::error!(error = %err, path = %path, "route evaluation failed");
            append_runtime_error(&path, &err.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
''',
    '''        Err(err) if err.code.starts_with("FLD4") => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "field_validation_failed",
                "code": err.code,
                "message": err.message,
            })),
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, path = %path, "route evaluation failed");
            append_runtime_error(&path, &err.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
''',
    "direct Field 400 mapping",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
) -> MethodRouter<AppState> {
""",
    """fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    field_plan: Arc<FieldRoutePlan>,
) -> MethodRouter<AppState> {
""",
    "Field method router signature",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """            module_program: module_program.clone(),
            native_plan: native_plan.clone(),
            takes_request: method_def.param_name.is_some(),
""",
    """            module_program: module_program.clone(),
            native_plan: native_plan.clone(),
            field_plan: field_plan.clone(),
            takes_request: method_def.param_name.is_some(),
""",
    "Field handler plan creation",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """            build_method_router(&route_file, module_program.clone(), None),
""",
    """            build_method_router(
                &route_file,
                module_program.clone(),
                None,
                Arc::new(FieldRoutePlan::default()),
            ),
""",
    "legacy empty Field plan",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    '''        let native_plan = image.route_wasm_artifact(id).map(|artifact| {
            Arc::new(NativeRoutePlan {
                runtime_image: image.image_id.clone(),
                source_id: id.clone(),
                artifact: artifact.clone(),
            })
        });
        router = router.route(
            &url_path,
            build_method_router(route_file.as_ref(), module_program.clone(), native_plan),
        );
''',
    '''        let field_plan = Arc::new(
            FieldRoutePlan::from_route(image, route_file.as_ref(), &manifest.logical_name)
                .map_err(anyhow::Error::msg)?,
        );
        // FLD-002 keeps Field-backed Routes on the linked evaluator path. The
        // Field context is resolved before dispatch; native lowering can adopt
        // the same pre-resolved input contract in a later compiler generation.
        let native_plan = if field_plan.is_active() {
            None
        } else {
            image.route_wasm_artifact(id).map(|artifact| {
                Arc::new(NativeRoutePlan {
                    runtime_image: image.image_id.clone(),
                    source_id: id.clone(),
                    artifact: artifact.clone(),
                })
            })
        };
        router = router.route(
            &url_path,
            build_method_router(
                route_file.as_ref(),
                module_program.clone(),
                native_plan,
                field_plan,
            ),
        );
''',
    "image Field route plan",
)
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """fn request_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}
""",
    """fn request_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn field_validation_response(error: FieldResolveError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "field_validation_failed",
            "code": error.code,
            "field": error.field,
            "message": error.message,
        })),
    )
        .into_response()
}
""",
    "structured Field 400 response",
)

# RELC regression for sibling-first Field resolution.
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """    #[test]
    fn route_import_of_missing_field_source_fails_link() {
""",
    r'''    #[test]
    fn route_field_import_resolves_sibling_before_api_root() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Field,
                "shop/awesomeness",
                "api/shop/awesomeness.field",
                r#":field[source = query, key = "awesome", optional = true]"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "shop/item",
                "api/shop/item.route",
                r#":import[field:awesomeness]
                   class Route { get(req) { return field.awesomeness(); } }"#,
            ),
        ];
        let image = compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        assert_eq!(image.fields.len(), 1);
    }

    #[test]
    fn route_import_of_missing_field_source_fails_link() {
''',
    "sibling Field RELC regression",
)

# Analyzer regression: multiple reusable resolvers merge into one field namespace.
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    """    #[test]
    fn used_import_is_not_reported_unused() {
""",
    r'''    #[test]
    fn reusable_field_imports_merge_into_field_namespace() {
        let source = r#":import[field:first, field:second]
            class Route { get(req) { return { a: field.first(), b: field.second() }; } }"#;
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let file = crate::parser::Parser::new(tokens).parse_file().unwrap();
        let diagnostics = analyze(&file);
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.severity == Severity::Error));
    }

    #[test]
    fn used_import_is_not_reported_unused() {
''',
    "Field analyzer namespace regression",
)

# Update docs from compiler-only boundary to live reusable/direct runtime behavior.
replace_once(
    "doc/field-manager.md",
    '''## Current implementation boundary

FLD-001 makes `.field` a first-class RELC source, discovers physical/embedded Field sources, validates its pure import allowlist, links `field:NAME` imports, carries Field AST/IR in the immutable Runtime Image, and provides deterministic `math`/`regx` helpers.

Request-time FieldManager resolution, route-local `fields { ... }`, structured required-field HTTP 400 responses, optional `null`, and `field.NAME()` execution are the next runtime slice. They must not be documented as active before that slice lands.
''',
    '''## Request-time runtime

FLD-002 resolves reusable `.field` sources exactly once from the existing immutable request query snapshot before Route execution. A reusable import is a namespace entry:

```text
:import[field:awesomeness]

class Route {
    get(req) {
        return field.awesomeness();
    }
}
```

For a Route such as `api/shop/item.route`, `field:awesomeness` checks `api/shop/awesomeness.field` first and then the API-root `api/awesomeness.field` fallback.

`:import[field]` enables the direct request helpers over the same snapshot:

```text
field.required("cookie")
field.optional("utm_source")
field.has("preview")
field.dynamic("utm_", true) // strip prefix
```

Required missing/invalid reusable fields fail before Route execution with HTTP 400 and a structured `field_validation_failed` response. Optional missing values resolve to their declared default or `null`; optional type failures resolve `null`. Dynamic prefix bindings resolve to an object. The resolved reusable values are also attached as `req.fields` for inspection.

Field-backed Routes deliberately remain on the linked evaluator path in FLD-002. Native Route-WASM adoption must consume the same pre-resolved Field context; it must not invent a second resolution model.

## Remaining runtime slice

Route-local declarative `fields { ... }` blocks are the next FLD-003 syntax/runtime slice. They will compile into the same resolver engine rather than duplicating request parsing or validation.
''',
    "FieldManager runtime docs",
)
