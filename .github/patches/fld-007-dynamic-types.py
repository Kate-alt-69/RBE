from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


field_path = Path("engine/crates/route-engine/src/field_manager.rs")
text = field_path.read_text(encoding="utf-8")

text = replace_once(
    text,
    """                .map(Value::Object)\n                .map_err(field_module_error)\n""",
    """                .map(Value::Object)\n                .map_err(|error| field_module_error(error.to_string()))\n""",
    "direct dynamic error conversion",
)

text = replace_once(
    text,
    """            .map(Value::Object)\n            .map_err(|message| {\n                resolve_error(\n                    \"FLD4004\",\n                    &binding.name,\n                    format!(\"{message} for resolver {resolver_name:?}\"),\n                )\n            }),\n""",
    """            .map(Value::Object)\n            .map_err(|error| match error {\n                error @ DynamicValuesError::TooMany { .. } => resolve_error(\n                    \"FLD4004\",\n                    &binding.name,\n                    format!(\"{error} for resolver {resolver_name:?}\"),\n                ),\n                DynamicValuesError::NonString { key } => resolve_error(\n                    \"FLD4002\",\n                    &binding.name,\n                    format!(\n                        \"{} dynamic field {key:?} for resolver {resolver_name:?} must be a string\",\n                        binding.source\n                    ),\n                ),\n            }),\n""",
    "declarative dynamic error mapping",
)

old_dynamic = """fn dynamic_values(\n    values: &HashMap<String, Value>,\n    prefix: &str,\n    strip_prefix: bool,\n    max_matches: usize,\n) -> Result<HashMap<String, Value>, String> {\n    let mut output = HashMap::new();\n    let mut matched = 0usize;\n    for (key, value) in values {\n        let Some(suffix) = key.strip_prefix(prefix) else {\n            continue;\n        };\n        matched = matched.saturating_add(1);\n        if matched > max_matches {\n            return Err(format!(\n                \"dynamic FieldManager prefix {prefix:?} matched more than {max_matches} fields\"\n            ));\n        }\n        let key = if strip_prefix {\n            suffix.to_string()\n        } else {\n            key.clone()\n        };\n        output.insert(key, value.clone());\n    }\n    Ok(output)\n}\n"""
new_dynamic = """#[derive(Debug)]\nenum DynamicValuesError {\n    TooMany {\n        prefix: String,\n        max_matches: usize,\n    },\n    NonString {\n        key: String,\n    },\n}\n\nimpl std::fmt::Display for DynamicValuesError {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        match self {\n            Self::TooMany {\n                prefix,\n                max_matches,\n            } => write!(\n                f,\n                \"dynamic FieldManager prefix {prefix:?} matched more than {max_matches} fields\"\n            ),\n            Self::NonString { key } => {\n                write!(f, \"dynamic FieldManager field {key:?} is not a string\")\n            }\n        }\n    }\n}\n\nfn dynamic_values(\n    values: &HashMap<String, Value>,\n    prefix: &str,\n    strip_prefix: bool,\n    max_matches: usize,\n) -> Result<HashMap<String, Value>, DynamicValuesError> {\n    // Count before inspecting value types so overflow always wins regardless of\n    // HashMap iteration order. Dynamic families are bounded before output work.\n    let matched = values.keys().filter(|key| key.starts_with(prefix)).count();\n    if matched > max_matches {\n        return Err(DynamicValuesError::TooMany {\n            prefix: prefix.to_string(),\n            max_matches,\n        });\n    }\n\n    // Pick the first invalid key lexicographically so a malformed body produces\n    // a stable diagnostic even though request objects are stored in a HashMap.\n    if let Some(key) = values\n        .iter()\n        .filter(|(key, _)| key.starts_with(prefix))\n        .filter(|(_, value)| !matches!(value, Value::String(_)))\n        .map(|(key, _)| key)\n        .min()\n    {\n        return Err(DynamicValuesError::NonString { key: key.clone() });\n    }\n\n    let mut output = HashMap::with_capacity(matched);\n    for (key, value) in values {\n        let Some(suffix) = key.strip_prefix(prefix) else {\n            continue;\n        };\n        let Value::String(value) = value else {\n            unreachable!(\"dynamic value types were validated before output construction\");\n        };\n        let key = if strip_prefix {\n            suffix.to_string()\n        } else {\n            key.clone()\n        };\n        output.insert(key, Value::String(value.clone()));\n    }\n    Ok(output)\n}\n"""
text = replace_once(text, old_dynamic, new_dynamic, "dynamic values implementation")

insert = r'''

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
'''

last_close = text.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs: could not find final module close")
text = text[:last_close] + insert + text[last_close:]
field_path.write_text(text, encoding="utf-8")

doc_path = Path("doc/field-manager.md")
doc = doc_path.read_text(encoding="utf-8")
doc = replace_once(
    doc,
    """Dynamic families are fail-closed and bounded: they accept at most 64 matches by default, declarative `dynamic(...)` may set `maxMatches = N`, and `N` must be between 1 and 256. Exceeding the bound returns `FLD4004`; values are never partially truncated.\n""",
    """Dynamic families are fail-closed and bounded: they accept at most 64 matches by default, declarative `dynamic(...)` may set `maxMatches = N`, and `N` must be between 1 and 256. Exceeding the bound returns `FLD4004`; values are never partially truncated. Dynamic families are string maps: a matched non-string body value fails with `FLD4002` instead of leaking an untyped JSON value through the field namespace.\n""",
    "FieldManager dynamic docs",
)
doc_path.write_text(doc, encoding="utf-8")
