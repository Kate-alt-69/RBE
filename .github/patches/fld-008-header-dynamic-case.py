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
    """                dynamic_values(\n                    &self.query,\n                    key,\n                    strip_prefix,\n                    DEFAULT_DYNAMIC_FIELD_MATCHES,\n                )\n""",
    """                dynamic_values(\n                    &self.query,\n                    \"query\",\n                    key,\n                    strip_prefix,\n                    DEFAULT_DYNAMIC_FIELD_MATCHES,\n                )\n""",
    "direct dynamic source",
)

text = replace_once(
    text,
    """            Some(values) => dynamic_values(\n                values,\n                &binding.lookup,\n                binding.strip_prefix,\n                binding.max_matches,\n            )\n""",
    """            Some(values) => dynamic_values(\n                values,\n                &binding.source,\n                &binding.lookup,\n                binding.strip_prefix,\n                binding.max_matches,\n            )\n""",
    "declarative dynamic source",
)

old = """fn dynamic_values(\n    values: &HashMap<String, Value>,\n    prefix: &str,\n    strip_prefix: bool,\n    max_matches: usize,\n) -> Result<HashMap<String, Value>, DynamicValuesError> {\n    // Count before inspecting value types so overflow always wins regardless of\n    // HashMap iteration order. Dynamic families are bounded before output work.\n    let matched = values.keys().filter(|key| key.starts_with(prefix)).count();\n    if matched > max_matches {\n        return Err(DynamicValuesError::TooMany {\n            prefix: prefix.to_string(),\n            max_matches,\n        });\n    }\n\n    // Pick the first invalid key lexicographically so a malformed body produces\n    // a stable diagnostic even though request objects are stored in a HashMap.\n    if let Some(key) = values\n        .iter()\n        .filter(|(key, _)| key.starts_with(prefix))\n        .filter(|(_, value)| !matches!(value, Value::String(_)))\n        .map(|(key, _)| key)\n        .min()\n    {\n        return Err(DynamicValuesError::NonString { key: key.clone() });\n    }\n\n    let mut output = HashMap::with_capacity(matched);\n    for (key, value) in values {\n        let Some(suffix) = key.strip_prefix(prefix) else {\n            continue;\n        };\n        let Value::String(value) = value else {\n            unreachable!(\"dynamic value types were validated before output construction\");\n        };\n        let key = if strip_prefix {\n            suffix.to_string()\n        } else {\n            key.clone()\n        };\n        output.insert(key, Value::String(value.clone()));\n    }\n    Ok(output)\n}\n"""

new = """fn dynamic_suffix<'a>(key: &'a str, source: &str, prefix: &str) -> Option<&'a str> {\n    if source == \"header\" {\n        let candidate = key.get(..prefix.len())?;\n        if candidate.eq_ignore_ascii_case(prefix) {\n            key.get(prefix.len()..)\n        } else {\n            None\n        }\n    } else {\n        key.strip_prefix(prefix)\n    }\n}\n\nfn dynamic_values(\n    values: &HashMap<String, Value>,\n    source: &str,\n    prefix: &str,\n    strip_prefix: bool,\n    max_matches: usize,\n) -> Result<HashMap<String, Value>, DynamicValuesError> {\n    // Count before inspecting value types so overflow always wins regardless of\n    // HashMap iteration order. Dynamic families are bounded before output work.\n    let matched = values\n        .keys()\n        .filter(|key| dynamic_suffix(key, source, prefix).is_some())\n        .count();\n    if matched > max_matches {\n        return Err(DynamicValuesError::TooMany {\n            prefix: prefix.to_string(),\n            max_matches,\n        });\n    }\n\n    // Pick the first invalid key lexicographically so a malformed body produces\n    // a stable diagnostic even though request objects are stored in a HashMap.\n    if let Some(key) = values\n        .iter()\n        .filter(|(key, _)| dynamic_suffix(key, source, prefix).is_some())\n        .filter(|(_, value)| !matches!(value, Value::String(_)))\n        .map(|(key, _)| key)\n        .min()\n    {\n        return Err(DynamicValuesError::NonString { key: key.clone() });\n    }\n\n    let mut output = HashMap::with_capacity(matched);\n    for (key, value) in values {\n        let Some(suffix) = dynamic_suffix(key, source, prefix) else {\n            continue;\n        };\n        let Value::String(value) = value else {\n            unreachable!(\"dynamic value types were validated before output construction\");\n        };\n        let key = if strip_prefix {\n            suffix.to_string()\n        } else {\n            key.clone()\n        };\n        output.insert(key, Value::String(value.clone()));\n    }\n    Ok(output)\n}\n"""
text = replace_once(text, old, new, "dynamic source matching")

insert = r'''

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
                    ("content-type".into(), Value::String("application/json".into())),
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
    "Header lookup is ASCII case-insensitive.",
    "Header lookup, including dynamic prefix matching, is ASCII case-insensitive.",
    "header case-insensitive docs",
)
doc_path.write_text(doc, encoding="utf-8")
