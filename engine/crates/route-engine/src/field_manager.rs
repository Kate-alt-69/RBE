//! Request-time FieldManager resolution.
//!
//! Request parsing remains owned by the HTTP edge. This layer consumes the
//! immutable REL request snapshot, applies validated `.field` programs once,
//! and exposes the same resolved values to the Route `field` namespace.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::{
    FieldBinding, FieldBindingMode, FieldFile, FieldValueType, ImportTarget, ModuleFile, RouteFile,
    Value, DEFAULT_DYNAMIC_FIELD_MATCHES,
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
                dynamic_values(
                    &self.query,
                    "query",
                    key,
                    strip_prefix,
                    DEFAULT_DYNAMIC_FIELD_MATCHES,
                )
                .map(Value::Object)
                .map_err(|error| field_module_error(error.to_string()))
            }
            _ => unreachable!("direct FieldManager function allowlist checked above"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FieldRoutePlan {
    direct_enabled: bool,
    inline_bindings: Vec<FieldBinding>,
    resolvers: Vec<(String, Arc<FieldFile>)>,
}

impl FieldRoutePlan {
    pub(crate) fn from_route(
        image: &RuntimeImage,
        route: &RouteFile,
        route_logical_name: &str,
    ) -> Result<Self, String> {
        let mut direct_enabled = false;
        let inline_bindings = route.field_bindings.clone();
        if !inline_bindings.is_empty() && !route.imports.iter().any(is_direct_field_import) {
            return Err(format!(
                "Route {route_logical_name:?} uses route-local fields but does not import `:import[field]`"
            ));
        }
        let mut resolver_names = inline_bindings
            .iter()
            .map(|binding| binding.name.clone())
            .collect::<HashSet<_>>();
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
                        return Err(format!(
                            "Route {route_logical_name:?} declares duplicate FieldManager resolver name {function:?}"
                        ));
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
                        format!(
                            "FieldManager source {} has no executable snapshot",
                            source.id
                        )
                    })?;
                    resolvers.push((function.clone(), field));
                }
                _ => {}
            }
        }

        Ok(Self {
            direct_enabled,
            inline_bindings,
            resolvers,
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.direct_enabled || !self.inline_bindings.is_empty() || !self.resolvers.is_empty()
    }

    pub(crate) async fn resolve(
        &self,
        request: &Value,
        program: &ModuleProgram,
    ) -> Result<Arc<FieldRuntimeContext>, FieldResolveError> {
        let query = request_query(request)?.clone();
        let mut resolved = HashMap::new();
        let mut allowed_resolvers = HashSet::new();
        for binding in &self.inline_bindings {
            let value = resolve_binding(request, binding, "route-local")?;
            resolved.insert(binding.name.clone(), value);
            allowed_resolvers.insert(binding.name.clone());
        }
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
    // Reusable FieldManager sources are intentionally local to the importing
    // Route's directory. A nested Route must never fall back to an API-root
    // `.field`, because that would recreate an implicit global registry and
    // make dependency ownership depend on unrelated files elsewhere in `api/`.
    let logical = route_logical_name
        .rsplit_once('/')
        .map(|(directory, _)| format!("{directory}/{name}"))
        .unwrap_or_else(|| name.to_string());
    vec![logical]
}

async fn resolve_field_file(
    file: &FieldFile,
    request: &Value,
    program: &ModuleProgram,
    resolver_name: &str,
) -> Result<Value, FieldResolveError> {
    if !file.bindings.is_empty() {
        let mut output = HashMap::with_capacity(file.bindings.len());
        for binding in &file.bindings {
            output.insert(
                binding.name.clone(),
                resolve_binding(request, binding, resolver_name)?,
            );
        }
        return Ok(Value::Object(output));
    }

    let raw = if let Some(key) = file.directive.key.as_deref() {
        let values = request_source_map(request, &file.directive.source, resolver_name)?;
        let value = values.and_then(|values| source_lookup(values, &file.directive.source, key));
        match value {
            Some(value) => {
                match coerce_value(value, file.directive.value_type, &file.directive.source) {
                    Ok(value) => value,
                    Err(_) if file.directive.optional => Value::Null,
                    Err(message) => {
                        return Err(resolve_error(
                            "FLD4002",
                            resolver_name,
                            format!(
                                "{} field {key:?} is invalid: {message}",
                                file.directive.source
                            ),
                        ));
                    }
                }
            }
            None if file.directive.optional => Value::Null,
            None => {
                return Err(resolve_error(
                    "FLD4001",
                    resolver_name,
                    format!(
                        "required {} field {key:?} is missing",
                        file.directive.source
                    ),
                ));
            }
        }
    } else {
        request_source_value(request, &file.directive.source, resolver_name)?.clone()
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
    request: &Value,
    binding: &FieldBinding,
    resolver_name: &str,
) -> Result<Value, FieldResolveError> {
    let values = request_source_map(request, &binding.source, &binding.name)?;
    if binding.mode == FieldBindingMode::Dynamic {
        return match values {
            Some(values) => dynamic_values(
                values,
                &binding.source,
                &binding.lookup,
                binding.strip_prefix,
                binding.max_matches,
            )
            .map(Value::Object)
            .map_err(|error| match error {
                error @ DynamicValuesError::TooMany { .. } => resolve_error(
                    "FLD4004",
                    &binding.name,
                    format!("{error} for resolver {resolver_name:?}"),
                ),
                DynamicValuesError::NonString { key } => resolve_error(
                    "FLD4002",
                    &binding.name,
                    format!(
                        "{} dynamic field {key:?} for resolver {resolver_name:?} must be a string",
                        binding.source
                    ),
                ),
            }),
            None => Ok(Value::Object(HashMap::new())),
        };
    }

    let raw = values.and_then(|values| source_lookup(values, &binding.source, &binding.lookup));
    let Some(raw) = raw else {
        return match binding.mode {
            FieldBindingMode::Required => Err(resolve_error(
                "FLD4001",
                &binding.name,
                format!(
                    "required {} field {:?} for resolver {resolver_name:?} is missing",
                    binding.source, binding.lookup
                ),
            )),
            FieldBindingMode::Optional => Ok(binding.default.clone().unwrap_or(Value::Null)),
            FieldBindingMode::Dynamic => unreachable!(),
        };
    };

    match coerce_value(raw, binding.value_type, &binding.source) {
        Ok(value) => Ok(value),
        Err(_) if binding.mode == FieldBindingMode::Optional => Ok(Value::Null),
        Err(message) => Err(resolve_error(
            "FLD4002",
            &binding.name,
            format!(
                "{} field {:?} for resolver {resolver_name:?} is invalid: {message}",
                binding.source, binding.lookup
            ),
        )),
    }
}

fn coerce_value(value: &Value, value_type: FieldValueType, source: &str) -> Result<Value, String> {
    match value_type {
        FieldValueType::String => match value {
            Value::String(value) => Ok(Value::String(value.clone())),
            _ => Err(format!("{source} value is not a string")),
        },
        FieldValueType::Int => match value {
            Value::String(raw) => raw
                .parse::<i64>()
                .map(|value| Value::Number(value as f64))
                .map_err(|_| "expected an integer".into()),
            Value::Number(value)
                if value.is_finite()
                    && value.fract() == 0.0
                    && *value >= i64::MIN as f64
                    && *value <= i64::MAX as f64 =>
            {
                Ok(Value::Number(*value))
            }
            _ => Err("expected an integer".into()),
        },
        FieldValueType::Bool => match value {
            Value::Bool(value) => Ok(Value::Bool(*value)),
            Value::String(raw) => match raw.to_ascii_lowercase().as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => Err("expected true or false".into()),
            },
            _ => Err("expected true or false".into()),
        },
    }
}

#[derive(Debug)]
enum DynamicValuesError {
    TooMany { prefix: String, max_matches: usize },
    NonString { key: String },
}

impl std::fmt::Display for DynamicValuesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany {
                prefix,
                max_matches,
            } => write!(
                f,
                "dynamic FieldManager prefix {prefix:?} matched more than {max_matches} fields"
            ),
            Self::NonString { key } => {
                write!(f, "dynamic FieldManager field {key:?} is not a string")
            }
        }
    }
}

fn dynamic_suffix<'a>(key: &'a str, source: &str, prefix: &str) -> Option<&'a str> {
    if source == "header" {
        let candidate = key.get(..prefix.len())?;
        if candidate.eq_ignore_ascii_case(prefix) {
            key.get(prefix.len()..)
        } else {
            None
        }
    } else {
        key.strip_prefix(prefix)
    }
}

fn dynamic_values(
    values: &HashMap<String, Value>,
    source: &str,
    prefix: &str,
    strip_prefix: bool,
    max_matches: usize,
) -> Result<HashMap<String, Value>, DynamicValuesError> {
    // Count before inspecting value types so overflow always wins regardless of
    // HashMap iteration order. Dynamic families are bounded before output work.
    let matched = values
        .keys()
        .filter(|key| dynamic_suffix(key, source, prefix).is_some())
        .count();
    if matched > max_matches {
        return Err(DynamicValuesError::TooMany {
            prefix: prefix.to_string(),
            max_matches,
        });
    }

    // Pick the first invalid key lexicographically so a malformed body produces
    // a stable diagnostic even though request objects are stored in a HashMap.
    if let Some(key) = values
        .iter()
        .filter(|(key, _)| dynamic_suffix(key, source, prefix).is_some())
        .filter(|(_, value)| !matches!(value, Value::String(_)))
        .map(|(key, _)| key)
        .min()
    {
        return Err(DynamicValuesError::NonString { key: key.clone() });
    }

    let mut output = HashMap::with_capacity(matched);
    for (key, value) in values {
        let Some(suffix) = dynamic_suffix(key, source, prefix) else {
            continue;
        };
        let Value::String(value) = value else {
            unreachable!("dynamic value types were validated before output construction");
        };
        let key = if strip_prefix {
            suffix.to_string()
        } else {
            key.clone()
        };
        output.insert(key, Value::String(value.clone()));
    }
    Ok(output)
}

fn request_object(request: &Value) -> Result<&HashMap<String, Value>, FieldResolveError> {
    let Value::Object(request) = request else {
        return Err(resolve_error(
            "FLD5000",
            "request",
            "FieldManager request snapshot is not an object",
        ));
    };
    Ok(request)
}

fn request_source_key(source: &str) -> Result<&'static str, FieldResolveError> {
    match source {
        "query" => Ok("query"),
        "body" => Ok("body"),
        "param" => Ok("params"),
        "header" => Ok("headers"),
        "cookie" => Ok("cookies"),
        other => Err(resolve_error(
            "FLD5000",
            "source",
            format!("compiled FieldManager source {other:?} is invalid"),
        )),
    }
}

fn request_source_value<'a>(
    request: &'a Value,
    source: &str,
    field: &str,
) -> Result<&'a Value, FieldResolveError> {
    let request = request_object(request)?;
    let key = request_source_key(source)?;
    request.get(key).ok_or_else(|| {
        resolve_error(
            "FLD5000",
            field,
            format!("FieldManager request snapshot has no {source} source"),
        )
    })
}

fn request_source_map<'a>(
    request: &'a Value,
    source: &str,
    field: &str,
) -> Result<Option<&'a HashMap<String, Value>>, FieldResolveError> {
    let value = request_source_value(request, source, field)?;
    match value {
        Value::Object(values) => Ok(Some(values)),
        Value::Null if source == "body" => Ok(None),
        _ if source == "body" => Err(resolve_error(
            "FLD4002",
            field,
            "body source must be a JSON object for named FieldManager bindings",
        )),
        _ => Err(resolve_error(
            "FLD5000",
            field,
            format!("FieldManager request snapshot {source} source is not an object"),
        )),
    }
}

fn source_lookup<'a>(
    values: &'a HashMap<String, Value>,
    source: &str,
    key: &str,
) -> Option<&'a Value> {
    if source == "header" {
        values
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    } else {
        values.get(key)
    }
}

fn request_query(request: &Value) -> Result<&HashMap<String, Value>, FieldResolveError> {
    let request = request_object(request)?;
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

fn is_direct_field_import(import: &ImportTarget) -> bool {
    matches!(import, ImportTarget::Builtin(module) if module == "field")
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
    use crate::lexer::Lexer;
    use crate::parser::Parser;
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

    fn route(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
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

    fn empty_program(name: &str) -> ModuleProgram {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-field-{name}-test-{}-{nonce}",
            std::process::id()
        ));
        ModuleProgram::load(&root.join("module")).expect("module load failed")
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
        let program = empty_program("resolver");
        let value = block_on_ready(resolve_field_file(
            &file,
            &request(&[("debug", "true"), ("utm_source", "chat"), ("cookie", "abc")]),
            &program,
            "awesome",
        ))
        .unwrap();
        let Value::Object(values) = value else {
            panic!("expected object")
        };
        assert!(matches!(values.get("page"), Some(Value::Number(value)) if *value == 1.0));
        assert!(matches!(values.get("debug"), Some(Value::Bool(true))));
        let Some(Value::Object(tracking)) = values.get("tracking") else {
            panic!("tracking")
        };
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
        let program = empty_program("failure-contract");
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
        let Value::Object(values) = value else {
            panic!("expected object")
        };
        assert!(matches!(values.get("page"), Some(Value::Null)));
    }

    #[test]
    fn route_local_fields_share_the_existing_resolver_runtime() {
        let route = route(
            r#":import[field]
               fields {
                   page = optional("page", type = int, default = 1);
                   debug = optional("debug", type = bool, default = false);
                   tracking = dynamic("utm_", stripPrefix = true);
                   cookie = required("cookie");
               }
               class Route { get(req) { return field.page(); } }"#,
        );
        assert_eq!(route.field_bindings.len(), 4);
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("route-local");
        let context = block_on_ready(plan.resolve(
            &request(&[("debug", "true"), ("utm_source", "chat"), ("cookie", "abc")]),
            &program,
        ))
        .unwrap();

        assert!(matches!(context.call("page", &[]).unwrap(), Value::Number(value) if value == 1.0));
        assert!(matches!(
            context.call("debug", &[]).unwrap(),
            Value::Bool(true)
        ));
        assert!(
            matches!(context.call("cookie", &[]).unwrap(), Value::String(value) if value == "abc")
        );
        let Value::Object(tracking) = context.call("tracking", &[]).unwrap() else {
            panic!("expected tracking object")
        };
        assert!(matches!(tracking.get("source"), Some(Value::String(value)) if value == "chat"));
    }

    #[test]
    fn route_local_fields_require_direct_field_import() {
        let tokens = Lexer::new(
            r#"fields { page = optional("page", type = int, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
        )
        .tokenize()
        .unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error.message.contains(":import[field]"));
    }

    #[test]
    fn route_local_fields_reject_reserved_direct_helper_names() {
        let tokens = Lexer::new(
            r#":import[field]
               fields { required = optional("required"); }
               class Route { get(req) { return req.fields; } }"#,
        )
        .tokenize()
        .unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error.message.contains("reserved"));
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
        assert!(matches!(
            context.call("awesome", &[]).unwrap(),
            Value::Number(7.0)
        ));
    }

    #[test]
    fn parser_accepts_all_field_sources_and_rejects_unknown_sources() {
        for source in ["query", "body", "param", "header", "cookie"] {
            let parsed = field(&format!(
                ":field[source = {source}, key = \"value\", optional = true]"
            ));
            assert_eq!(parsed.directive.source, source);
        }

        let tokens = Lexer::new(":field[source = socket, key = \"value\"]")
            .tokenize()
            .unwrap();
        let error = Parser::new(tokens).parse_field_file().unwrap_err();
        assert!(error
            .message
            .contains("expected query, body, param, header, or cookie"));
    }

    #[test]
    fn route_local_bindings_can_select_request_sources() {
        let route = route(
            r#":import[field]
               fields {
                   id = required("id", source = param);
                   token = required("Authorization", source = header);
                   session = optional("session", source = cookie);
                   enabled = required("enabled", source = body, type = bool);
                   count = required("count", source = body, type = int);
               }
               class Route { get(req) { return req.fields; } }"#,
        );
        assert_eq!(route.field_bindings[0].source, "param");
        assert_eq!(route.field_bindings[1].source, "header");
        assert_eq!(route.field_bindings[2].source, "cookie");
        assert_eq!(route.field_bindings[3].source, "body");

        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            (
                "params".into(),
                Value::Object(HashMap::from([("id".into(), Value::String("42".into()))])),
            ),
            (
                "headers".into(),
                Value::Object(HashMap::from([(
                    "authorization".into(),
                    Value::String("Bearer abc".into()),
                )])),
            ),
            (
                "cookies".into(),
                Value::Object(HashMap::from([(
                    "session".into(),
                    Value::String("cookie-value".into()),
                )])),
            ),
            (
                "body".into(),
                Value::Object(HashMap::from([
                    ("enabled".into(), Value::Bool(true)),
                    ("count".into(), Value::Number(7.0)),
                ])),
            ),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("multi-source");
        let context = block_on_ready(plan.resolve(&request, &program)).unwrap();
        assert!(matches!(context.call("id", &[]).unwrap(), Value::String(value) if value == "42"));
        assert!(
            matches!(context.call("token", &[]).unwrap(), Value::String(value) if value == "Bearer abc")
        );
        assert!(
            matches!(context.call("session", &[]).unwrap(), Value::String(value) if value == "cookie-value")
        );
        assert!(matches!(
            context.call("enabled", &[]).unwrap(),
            Value::Bool(true)
        ));
        assert!(
            matches!(context.call("count", &[]).unwrap(), Value::Number(value) if value == 7.0)
        );
    }

    #[test]
    fn dynamic_bindings_are_bounded_and_configurable() {
        let route = route(
            r#":import[field]
               fields { tracking = dynamic("utm_", stripPrefix = true, maxMatches = 2); }
               class Route { get(req) { return req.fields; } }"#,
        );
        assert_eq!(route.field_bindings[0].max_matches, 2);

        let request = Value::Object(HashMap::from([
            (
                "query".into(),
                Value::Object(HashMap::from([
                    ("utm_a".into(), Value::String("a".into())),
                    ("utm_b".into(), Value::String("b".into())),
                    ("utm_c".into(), Value::String("c".into())),
                ])),
            ),
            ("params".into(), Value::Object(HashMap::new())),
            ("headers".into(), Value::Object(HashMap::new())),
            ("cookies".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("dynamic-bound");
        let error = block_on_ready(plan.resolve(&request, &program)).unwrap_err();
        assert_eq!(error.code, "FLD4004");
        assert!(error.message.contains("more than 2 fields"));
    }

    #[test]
    fn dynamic_binding_limits_are_validated_at_parse_time() {
        for source in [
            r#":import[field]
               fields { tracking = dynamic("utm_", maxMatches = 0); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { tracking = dynamic("utm_", maxMatches = 257); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { page = optional("page", maxMatches = 2); }
               class Route { get(req) { return req.fields; } }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_file().unwrap_err();
            assert!(error.message.contains("maxMatches"));
        }
    }

    #[test]
    fn direct_dynamic_helper_uses_the_default_bound() {
        let query = (0..=DEFAULT_DYNAMIC_FIELD_MATCHES)
            .map(|index| (format!("utm_{index}"), Value::String(index.to_string())))
            .collect();
        let context = FieldRuntimeContext {
            query,
            resolved: HashMap::new(),
            allowed_resolvers: HashSet::new(),
            direct_enabled: true,
        };
        let error = context
            .call("dynamic", &[Value::String("utm_".into())])
            .unwrap_err();
        assert!(error.message.contains("more than 64 fields"));
    }

    #[test]
    fn field_directive_source_is_inherited_by_declarative_bindings() {
        let file = field(
            r#":field[source = header, optional = true]
               resolve { token = required("Authorization"); }"#,
        );
        assert_eq!(file.bindings[0].source, "header");
        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("params".into(), Value::Object(HashMap::new())),
            (
                "headers".into(),
                Value::Object(HashMap::from([(
                    "authorization".into(),
                    Value::String("Bearer abc".into()),
                )])),
            ),
            ("cookies".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
        ]));
        let program = empty_program("header-source");
        let value = block_on_ready(resolve_field_file(&file, &request, &program, "auth")).unwrap();
        let Value::Object(values) = value else {
            panic!("expected object")
        };
        assert!(matches!(values.get("token"), Some(Value::String(value)) if value == "Bearer abc"));
    }

    #[test]
    fn body_dynamic_bindings_reject_non_string_values() {
        let route = route(
            r#":import[field]
               fields { tracking = dynamic("utm_", source = body, maxMatches = 4); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("params".into(), Value::Object(HashMap::new())),
            ("headers".into(), Value::Object(HashMap::new())),
            ("cookies".into(), Value::Object(HashMap::new())),
            (
                "body".into(),
                Value::Object(HashMap::from([
                    ("utm_source".into(), Value::String("chat".into())),
                    ("utm_count".into(), Value::Number(7.0)),
                ])),
            ),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("dynamic-body-type");
        let error = block_on_ready(plan.resolve(&request, &program)).unwrap_err();
        assert_eq!(error.code, "FLD4002");
        assert_eq!(error.field, "tracking");
        assert!(error.message.contains("utm_count"));
        assert!(error.message.contains("must be a string"));
    }

    #[test]
    fn dynamic_overflow_precedes_value_type_errors() {
        let route = route(
            r#":import[field]
               fields { tracking = dynamic("utm_", source = body, maxMatches = 1); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("params".into(), Value::Object(HashMap::new())),
            ("headers".into(), Value::Object(HashMap::new())),
            ("cookies".into(), Value::Object(HashMap::new())),
            (
                "body".into(),
                Value::Object(HashMap::from([
                    ("utm_source".into(), Value::String("chat".into())),
                    ("utm_count".into(), Value::Number(7.0)),
                ])),
            ),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("dynamic-overflow-priority");
        let error = block_on_ready(plan.resolve(&request, &program)).unwrap_err();
        assert_eq!(error.code, "FLD4004");
        assert!(error.message.contains("more than 1 fields"));
    }

    #[test]
    fn header_dynamic_prefixes_are_ascii_case_insensitive() {
        let route = route(
            r#":import[field]
               fields { trace = dynamic("X-Trace-", source = header, stripPrefix = true); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("params".into(), Value::Object(HashMap::new())),
            (
                "headers".into(),
                Value::Object(HashMap::from([
                    ("x-trace-id".into(), Value::String("abc".into())),
                    ("x-trace-span".into(), Value::String("root".into())),
                    (
                        "content-type".into(),
                        Value::String("application/json".into()),
                    ),
                ])),
            ),
            ("cookies".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("dynamic-header-case");
        let context = block_on_ready(plan.resolve(&request, &program)).unwrap();
        let Value::Object(trace) = context.call("trace", &[]).unwrap() else {
            panic!("expected trace object");
        };
        assert!(matches!(trace.get("id"), Some(Value::String(value)) if value == "abc"));
        assert!(matches!(trace.get("span"), Some(Value::String(value)) if value == "root"));
        assert_eq!(trace.len(), 2);
    }

    #[test]
    fn non_header_dynamic_prefixes_remain_case_sensitive() {
        let route = route(
            r#":import[field]
               fields { tracking = dynamic("utm_"); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let request = Value::Object(HashMap::from([
            (
                "query".into(),
                Value::Object(HashMap::from([(
                    "UTM_source".into(),
                    Value::String("chat".into()),
                )])),
            ),
            ("params".into(), Value::Object(HashMap::new())),
            ("headers".into(), Value::Object(HashMap::new())),
            ("cookies".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("dynamic-query-case");
        let context = block_on_ready(plan.resolve(&request, &program)).unwrap();
        let Value::Object(tracking) = context.call("tracking", &[]).unwrap() else {
            panic!("expected tracking object");
        };
        assert!(tracking.is_empty());
    }

    #[test]
    fn resolver_options_fail_closed_when_used_by_the_wrong_mode() {
        let error_for = |binding: &str| {
            let source = format!(
                ":import[field]\nfields {{ value = {binding}; }}\nclass Route {{ get(req) {{ return req.fields; }} }}"
            );
            let tokens = Lexer::new(&source).tokenize().unwrap();
            Parser::new(tokens).parse_file().unwrap_err()
        };

        let error = error_for(r#"optional("page", maxMatches = 64)"#);
        assert!(error
            .message
            .contains("maxMatches is only valid for dynamic"));

        let error = error_for(r#"required("page", stripPrefix = false)"#);
        assert!(error
            .message
            .contains("stripPrefix is only valid for dynamic"));

        let error = error_for(r#"dynamic("utm_", type = string)"#);
        assert!(error.message.contains("dynamic(...) supports source"));
    }

    #[test]
    fn resolver_options_reject_duplicates_instead_of_last_value_wins() {
        let source = r#":import[field]
            fields { value = optional("page", source = query, source = body); }
            class Route { get(req) { return req.fields; } }"#;
        let tokens = Lexer::new(source).tokenize().unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error
            .message
            .contains("duplicate FieldManager resolver option"));
        assert!(error.message.contains("source"));
    }

    #[test]
    fn field_metadata_rejects_duplicate_options_in_both_syntaxes() {
        for source in [
            r#":field[source = query, source = body, key = "value"]"#,
            r#"field {
                   source = query;
                   source = body;
                   key = "value";
               }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_field_file().unwrap_err();
            assert!(error
                .message
                .contains("duplicate FieldManager metadata option"));
            assert!(error.message.contains("source"));
        }
    }

    #[test]
    fn field_metadata_rejects_required_optional_alias_collisions() {
        for source in [
            r#":field[required = true, optional = false, key = "value"]"#,
            r#"field {
                   optional = true;
                   required = false;
                   key = "value";
               }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_field_file().unwrap_err();
            assert!(error
                .message
                .contains("cannot declare both `required` and `optional`"));
        }
    }

    #[test]
    fn route_local_defaults_must_match_declared_types_regardless_of_option_order() {
        for source in [
            r#":import[field]
               fields { page = optional("page", type = int, default = "oops"); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { page = optional("page", default = "oops", type = int); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { debug = optional("debug", type = bool, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { label = optional("label", type = string, default = false); }
               class Route { get(req) { return req.fields; } }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_file().unwrap_err();
            assert!(error.message.contains("default"));
            assert!(error.message.contains("declared type"));
        }
    }

    #[test]
    fn route_local_defaults_accept_matching_values_and_null() {
        let parsed = route(
            r#":import[field]
               fields {
                   page = optional("page", type = int, default = 1);
                   debug = optional("debug", type = bool, default = false);
                   label = optional("label", type = string, default = "ok");
                   nullable = optional("nullable", type = int, default = null);
               }
               class Route { get(req) { return req.fields; } }"#,
        );
        assert!(
            matches!(parsed.field_bindings[0].default, Some(Value::Number(value)) if value == 1.0)
        );
        assert!(matches!(
            parsed.field_bindings[1].default,
            Some(Value::Bool(false))
        ));
        assert!(
            matches!(parsed.field_bindings[2].default, Some(Value::String(ref value)) if value == "ok")
        );
        assert!(matches!(
            parsed.field_bindings[3].default,
            Some(Value::Null)
        ));
    }

    #[test]
    fn reusable_field_lookup_is_strictly_route_directory_scoped() {
        assert_eq!(
            field_logical_candidates("shop/item", "auth"),
            vec!["shop/auth".to_string()]
        );
        assert_eq!(
            field_logical_candidates("shop/admin/item", "auth"),
            vec!["shop/admin/auth".to_string()]
        );
    }

    #[test]
    fn root_routes_still_resolve_root_sibling_fields() {
        assert_eq!(
            field_logical_candidates("item", "auth"),
            vec!["auth".to_string()]
        );
    }
}
