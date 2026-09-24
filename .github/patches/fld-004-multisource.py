from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected snippet not found in {path}: {old[:120]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# AST: each declarative binding owns an explicit source, defaulting to query.
ast = ROOT / "engine/crates/route-engine/src/ast.rs"
replace_once(
    ast,
    """pub struct FieldBinding {\n    pub name: String,\n    pub lookup: String,\n    pub mode: FieldBindingMode,\n""",
    """pub struct FieldBinding {\n    pub name: String,\n    pub lookup: String,\n    pub source: String,\n    pub mode: FieldBindingMode,\n""",
)

# Parser: accept all request snapshot sources, inherit .field directive source,
# and allow route-local bindings to override the query default per binding.
parser = ROOT / "engine/crates/route-engine/src/parser.rs"
replace_once(
    parser,
    """                    let binding = self.parse_field_binding(name)?;\n""",
    """                    let binding = self.parse_field_binding(name, &directive.source)?;\n""",
)
replace_once(
    parser,
    """            let binding = self.parse_field_binding(name)?;\n""",
    """            let binding = self.parse_field_binding(name, \"query\")?;\n""",
)
replace_once(
    parser,
    """            \"source\" => {\n                let source = self.expect_ident()?;\n                if source != \"query\" {\n                    return Err(\n                        self.error_here(\"FieldManager source currently supports only `query`\")\n                    );\n                }\n                directive.source = source;\n            }\n""",
    """            \"source\" => directive.source = self.parse_field_source()?,\n""",
)
replace_once(
    parser,
    """    fn parse_field_type(&mut self) -> Result<FieldValueType, ParseError> {\n""",
    """    fn parse_field_source(&mut self) -> Result<String, ParseError> {\n        let source = self.expect_ident()?;\n        match source.as_str() {\n            \"query\" | \"body\" | \"param\" | \"header\" | \"cookie\" => Ok(source),\n            other => Err(self.error_here(&format!(\n                \"unknown FieldManager source {other:?}; expected query, body, param, header, or cookie\"\n            ))),\n        }\n    }\n\n    fn parse_field_type(&mut self) -> Result<FieldValueType, ParseError> {\n""",
)
replace_once(
    parser,
    """    fn parse_field_binding(&mut self, name: String) -> Result<FieldBinding, ParseError> {\n""",
    """    fn parse_field_binding(\n        &mut self,\n        name: String,\n        default_source: &str,\n    ) -> Result<FieldBinding, ParseError> {\n""",
)
replace_once(
    parser,
    """        let mut value_type = FieldValueType::String;\n        let mut default = None;\n        let mut strip_prefix = false;\n""",
    """        let mut source = default_source.to_string();\n        let mut value_type = FieldValueType::String;\n        let mut default = None;\n        let mut strip_prefix = false;\n""",
)
replace_once(
    parser,
    """            match option.as_str() {\n                \"type\" => value_type = self.parse_field_type()?,\n""",
    """            match option.as_str() {\n                \"source\" => source = self.parse_field_source()?,\n                \"type\" => value_type = self.parse_field_type()?,\n""",
)
replace_once(
    parser,
    """        Ok(FieldBinding {\n            name,\n            lookup,\n            mode,\n""",
    """        Ok(FieldBinding {\n            name,\n            lookup,\n            source,\n            mode,\n""",
)

# Runtime: source selection is performed against the existing immutable request
# snapshot. No new body/header/cookie parsing happens in FieldManager itself.
manager = ROOT / "engine/crates/route-engine/src/field_manager.rs"
text = manager.read_text(encoding="utf-8")
text = text.replace(
    "//! Query parsing remains owned by the HTTP edge. This layer consumes the\n//! immutable REL request snapshot, applies validated `.field` programs once,\n",
    "//! Request parsing remains owned by the HTTP edge. This layer consumes the\n//! immutable REL request snapshot, applies validated `.field` programs once,\n",
    1,
)
text = text.replace(
    """        for binding in &self.inline_bindings {\n            let value = resolve_binding(&query, binding, \"route-local\")?;\n""",
    """        for binding in &self.inline_bindings {\n            let value = resolve_binding(request, binding, \"route-local\")?;\n""",
    1,
)
start = text.index("async fn resolve_field_file(")
end = text.index("fn string_arg<'a>(", start)
replacement = r'''async fn resolve_field_file(
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
            Some(value) => match coerce_value(value, file.directive.value_type, &file.directive.source) {
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
            },
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
        return Ok(Value::Object(match values {
            Some(values) => dynamic_values(values, &binding.lookup, binding.strip_prefix),
            None => HashMap::new(),
        }));
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

fn dynamic_values(
    values: &HashMap<String, Value>,
    prefix: &str,
    strip_prefix: bool,
) -> HashMap<String, Value> {
    values
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

'''
manager.write_text(text[:start] + replacement + text[end:], encoding="utf-8")

# Add regression coverage without disturbing existing helper fixtures.
manager_text = manager.read_text(encoding="utf-8")
insert_at = manager_text.rindex("\n}")
tests = r'''

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
        assert!(error.message.contains("expected query, body, param, header, or cookie"));
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
        assert!(matches!(context.call("token", &[]).unwrap(), Value::String(value) if value == "Bearer abc"));
        assert!(matches!(context.call("session", &[]).unwrap(), Value::String(value) if value == "cookie-value"));
        assert!(matches!(context.call("enabled", &[]).unwrap(), Value::Bool(true)));
        assert!(matches!(context.call("count", &[]).unwrap(), Value::Number(value) if value == 7.0));
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
'''
manager.write_text(manager_text[:insert_at] + tests + manager_text[insert_at:], encoding="utf-8")

# Docs: preserve the query shorthand while documenting the multi-source contract.
doc = ROOT / "doc/field-manager.md"
doc_text = doc.read_text(encoding="utf-8")
doc_text = doc_text.replace(
    "FieldManager is RBE's typed URL/query-field layer.",
    "FieldManager is RBE's typed request-field layer.",
    1,
)
doc_text += r'''

## Multi-source fields (FLD-004)

FieldManager can resolve from the immutable request snapshot without re-parsing the HTTP request. Supported declarative sources are:

```text
query
body
param
header
cookie
```

A reusable `.field` chooses its default source in metadata:

```text
:field[source = header, key = "authorization", optional = true]
```

Declarative bindings inherit that source, while an individual binding may override it:

```text
:field[source = body, optional = true]
resolve {
    email = required("email");
    trace = optional("x-trace-id", source = header);
}
```

Route-local fields default to query for compatibility and can select a source per binding:

```text
:import[field]

fields {
    id = required("id", source = param);
    token = required("Authorization", source = header);
    session = optional("session", source = cookie);
    enabled = optional("enabled", source = body, type = bool, default = false);
}
```

Header lookup is ASCII case-insensitive. Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.

The direct helpers `field.required(...)`, `field.optional(...)`, `field.has(...)`, and `field.dynamic(...)` intentionally remain query shorthands. Use declarative bindings when selecting another source so source ownership remains visible in compiled Field IR.
'''
doc.write_text(doc_text, encoding="utf-8")
