from pathlib import Path


def replace(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:80]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


AST = "engine/crates/route-engine/src/ast.rs"
PARSER = "engine/crates/route-engine/src/parser.rs"
FIELD = "engine/crates/route-engine/src/field_manager.rs"
DOC = "doc/field-manager.md"

replace(
    AST,
    "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum FieldBindingMode {\n    Required,\n    Optional,\n    Dynamic,\n}\n\n#[derive(Debug, Clone)]\npub struct FieldDirective",
    "pub const DEFAULT_DYNAMIC_FIELD_MATCHES: usize = 64;\npub const MAX_DYNAMIC_FIELD_MATCHES: usize = 256;\n\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum FieldBindingMode {\n    Required,\n    Optional,\n    Dynamic,\n}\n\n#[derive(Debug, Clone)]\npub struct FieldDirective",
)
replace(
    AST,
    "    pub default: Option<Value>,\n    pub strip_prefix: bool,\n}\n\n#[derive(Debug, Clone)]\npub struct FieldFile",
    "    pub default: Option<Value>,\n    pub strip_prefix: bool,\n    pub max_matches: usize,\n}\n\n#[derive(Debug, Clone)]\npub struct FieldFile",
)

replace(
    PARSER,
    "    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,\n    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceClassDef, ServiceProgram,\n    Statement, Value,\n};",
    "    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,\n    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceClassDef, ServiceProgram,\n    Statement, Value, DEFAULT_DYNAMIC_FIELD_MATCHES, MAX_DYNAMIC_FIELD_MATCHES,\n};",
)
replace(
    PARSER,
    "        let mut default = None;\n        let mut strip_prefix = false;\n        while self.check(&TokenKind::Comma) {",
    "        let mut default = None;\n        let mut strip_prefix = false;\n        let mut max_matches = DEFAULT_DYNAMIC_FIELD_MATCHES;\n        while self.check(&TokenKind::Comma) {",
)
replace(
    PARSER,
    '''                "stripPrefix" => {\n                    strip_prefix = match self.advance().kind {\n                        TokenKind::True => true,\n                        TokenKind::False => false,\n                        other => {\n                            return Err(self.error_here(&format!(\n                                "stripPrefix must be true or false, got {other:?}"\n                            )));\n                        }\n                    };\n                }\n                other => {''',
    '''                "stripPrefix" => {\n                    strip_prefix = match self.advance().kind {\n                        TokenKind::True => true,\n                        TokenKind::False => false,\n                        other => {\n                            return Err(self.error_here(&format!(\n                                "stripPrefix must be true or false, got {other:?}"\n                            )));\n                        }\n                    };\n                }\n                "maxMatches" => {\n                    max_matches = match self.advance().kind {\n                        TokenKind::Number(value)\n                            if value.is_finite()\n                                && value.fract() == 0.0\n                                && value >= 1.0\n                                && value <= MAX_DYNAMIC_FIELD_MATCHES as f64 =>\n                        {\n                            value as usize\n                        }\n                        other => {\n                            return Err(self.error_here(&format!(\n                                "maxMatches must be an integer from 1 to {MAX_DYNAMIC_FIELD_MATCHES}, got {other:?}"\n                            )));\n                        }\n                    };\n                }\n                other => {''',
)
replace(
    PARSER,
    '''        if mode == FieldBindingMode::Dynamic\n            && (default.is_some() || value_type != FieldValueType::String)\n        {\n            return Err(self.error_here(\n                "dynamic(...) supports stripPrefix only; dynamic values stay strings",\n            ));\n        }\n        Ok(FieldBinding {\n            name,\n            lookup,\n            source,\n            mode,\n            value_type,\n            default,\n            strip_prefix,\n        })''',
    '''        if mode == FieldBindingMode::Dynamic\n            && (default.is_some() || value_type != FieldValueType::String)\n        {\n            return Err(self.error_here(\n                "dynamic(...) supports stripPrefix and maxMatches only; dynamic values stay strings",\n            ));\n        }\n        if mode != FieldBindingMode::Dynamic && max_matches != DEFAULT_DYNAMIC_FIELD_MATCHES {\n            return Err(self.error_here("maxMatches is only valid for dynamic(...)"));\n        }\n        Ok(FieldBinding {\n            name,\n            lookup,\n            source,\n            mode,\n            value_type,\n            default,\n            strip_prefix,\n            max_matches,\n        })''',
)

replace(
    FIELD,
    "    FieldBinding, FieldBindingMode, FieldFile, FieldValueType, ImportTarget, ModuleFile, RouteFile,\n    Value,\n};",
    "    FieldBinding, FieldBindingMode, FieldFile, FieldValueType, ImportTarget, ModuleFile, RouteFile,\n    Value, DEFAULT_DYNAMIC_FIELD_MATCHES,\n};",
)
replace(
    FIELD,
    '''                Ok(Value::Object(dynamic_values(\n                    &self.query,\n                    key,\n                    strip_prefix,\n                )))''',
    '''                dynamic_values(\n                    &self.query,\n                    key,\n                    strip_prefix,\n                    DEFAULT_DYNAMIC_FIELD_MATCHES,\n                )\n                .map(Value::Object)\n                .map_err(field_module_error)''',
)
replace(
    FIELD,
    '''    if binding.mode == FieldBindingMode::Dynamic {\n        return Ok(Value::Object(match values {\n            Some(values) => dynamic_values(values, &binding.lookup, binding.strip_prefix),\n            None => HashMap::new(),\n        }));\n    }''',
    '''    if binding.mode == FieldBindingMode::Dynamic {\n        return match values {\n            Some(values) => dynamic_values(\n                values,\n                &binding.lookup,\n                binding.strip_prefix,\n                binding.max_matches,\n            )\n            .map(Value::Object)\n            .map_err(|message| {\n                resolve_error(\n                    "FLD4004",\n                    &binding.name,\n                    format!("{message} for resolver {resolver_name:?}"),\n                )\n            }),\n            None => Ok(Value::Object(HashMap::new())),\n        };\n    }''',
)
replace(
    FIELD,
    '''fn dynamic_values(\n    values: &HashMap<String, Value>,\n    prefix: &str,\n    strip_prefix: bool,\n) -> HashMap<String, Value> {\n    values\n        .iter()\n        .filter_map(|(key, value)| {\n            key.strip_prefix(prefix).map(|suffix| {\n                let key = if strip_prefix {\n                    suffix.to_string()\n                } else {\n                    key.clone()\n                };\n                (key, value.clone())\n            })\n        })\n        .collect()\n}''',
    '''fn dynamic_values(\n    values: &HashMap<String, Value>,\n    prefix: &str,\n    strip_prefix: bool,\n    max_matches: usize,\n) -> Result<HashMap<String, Value>, String> {\n    let mut output = HashMap::new();\n    let mut matched = 0usize;\n    for (key, value) in values {\n        let Some(suffix) = key.strip_prefix(prefix) else {\n            continue;\n        };\n        matched = matched.saturating_add(1);\n        if matched > max_matches {\n            return Err(format!(\n                "dynamic FieldManager prefix {prefix:?} matched more than {max_matches} fields"\n            ));\n        }\n        let key = if strip_prefix {\n            suffix.to_string()\n        } else {\n            key.clone()\n        };\n        output.insert(key, value.clone());\n    }\n    Ok(output)\n}''',
)

# Add focused parser/runtime regressions before the existing final test in the module.
anchor = '''    #[test]\n    fn field_directive_source_is_inherited_by_declarative_bindings() {'''
insert = '''    #[test]\n    fn dynamic_bindings_are_bounded_and_configurable() {\n        let route = route(\n            r#":import[field]\n               fields { tracking = dynamic("utm_", stripPrefix = true, maxMatches = 2); }\n               class Route { get(req) { return req.fields; } }"#,\n        );\n        assert_eq!(route.field_bindings[0].max_matches, 2);\n\n        let request = Value::Object(HashMap::from([\n            (\n                "query".into(),\n                Value::Object(HashMap::from([\n                    ("utm_a".into(), Value::String("a".into())),\n                    ("utm_b".into(), Value::String("b".into())),\n                    ("utm_c".into(), Value::String("c".into())),\n                ])),\n            ),\n            ("params".into(), Value::Object(HashMap::new())),\n            ("headers".into(), Value::Object(HashMap::new())),\n            ("cookies".into(), Value::Object(HashMap::new())),\n            ("body".into(), Value::Null),\n        ]));\n        let plan = FieldRoutePlan {\n            direct_enabled: true,\n            inline_bindings: route.field_bindings.clone(),\n            resolvers: Vec::new(),\n        };\n        let program = empty_program("dynamic-bound");\n        let error = block_on_ready(plan.resolve(&request, &program)).unwrap_err();\n        assert_eq!(error.code, "FLD4004");\n        assert!(error.message.contains("more than 2 fields"));\n    }\n\n    #[test]\n    fn dynamic_binding_limits_are_validated_at_parse_time() {\n        for source in [\n            r#":import[field]\n               fields { tracking = dynamic("utm_", maxMatches = 0); }\n               class Route { get(req) { return req.fields; } }"#,\n            r#":import[field]\n               fields { tracking = dynamic("utm_", maxMatches = 257); }\n               class Route { get(req) { return req.fields; } }"#,\n            r#":import[field]\n               fields { page = optional("page", maxMatches = 2); }\n               class Route { get(req) { return req.fields; } }"#,\n        ] {\n            let tokens = Lexer::new(source).tokenize().unwrap();\n            let error = Parser::new(tokens).parse_file().unwrap_err();\n            assert!(error.message.contains("maxMatches"));\n        }\n    }\n\n    #[test]\n    fn direct_dynamic_helper_uses_the_default_bound() {\n        let query = (0..=DEFAULT_DYNAMIC_FIELD_MATCHES)\n            .map(|index| (format!("utm_{index}"), Value::String(index.to_string())))\n            .collect();\n        let context = FieldRuntimeContext {\n            query,\n            resolved: HashMap::new(),\n            allowed_resolvers: HashSet::new(),\n            direct_enabled: true,\n        };\n        let error = context\n            .call("dynamic", &[Value::String("utm_".into())])\n            .unwrap_err();\n        assert!(error.message.contains("more than 64 fields"));\n    }\n\n'''
text = Path(FIELD).read_text(encoding="utf-8")
if anchor not in text:
    raise SystemExit("missing field-manager test anchor")
Path(FIELD).write_text(text.replace(anchor, insert + anchor, 1), encoding="utf-8")

# Document the bound beside the public dynamic-field contract.
text = Path(DOC).read_text(encoding="utf-8")
anchor = "Dynamic prefix bindings resolve to an object. The resolved reusable values are also attached as `req.fields` for inspection."
replacement = anchor + " Dynamic families are fail-closed and bounded: they accept at most 64 matches by default, declarative `dynamic(...)` may set `maxMatches = N`, and `N` must be between 1 and 256. Exceeding the bound returns `FLD4004`; values are never partially truncated."
if anchor not in text:
    raise SystemExit("missing FieldManager documentation anchor")
Path(DOC).write_text(text.replace(anchor, replacement, 1), encoding="utf-8")
