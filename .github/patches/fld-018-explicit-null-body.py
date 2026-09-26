from pathlib import Path

field_path = Path("engine/crates/route-engine/src/field_manager.rs")
doc_path = Path("doc/field-manager.md")

source = field_path.read_text(encoding="utf-8")
old = '''fn request_source_map<'a>(
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
'''
new = '''fn request_source_map<'a>(
    request: &'a Value,
    source: &str,
    field: &str,
) -> Result<Option<&'a HashMap<String, Value>>, FieldResolveError> {
    let value = request_source_value(request, source, field)?;
    match value {
        Value::Object(values) => Ok(Some(values)),
        Value::Null if source == "body" => {
            let request = request_object(request)?;
            let raw_body = match request.get("rawBody") {
                Some(Value::String(raw_body)) => raw_body,
                Some(_) => {
                    return Err(resolve_error(
                        "FLD5000",
                        field,
                        "FieldManager request snapshot rawBody is not a string",
                    ));
                }
                None => {
                    return Err(resolve_error(
                        "FLD5000",
                        field,
                        "FieldManager request snapshot has no rawBody string",
                    ));
                }
            };
            if raw_body.is_empty() {
                Ok(None)
            } else {
                Err(resolve_error(
                    "FLD4002",
                    field,
                    "body source must be a JSON object for named FieldManager bindings",
                ))
            }
        }
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
'''
if old not in source:
    raise SystemExit("request_source_map anchor not found")
source = source.replace(old, new, 1)

anchor = '''    #[test]
    fn field_directive_source_is_inherited_by_declarative_bindings() {
'''
tests = '''    #[test]
    fn named_body_bindings_distinguish_empty_body_from_explicit_json_null() {
        let route = route(
            r#":import[field]
               fields { enabled = optional("enabled", source = body, type = bool, default = false); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("body-null-vs-empty");

        let empty_request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
            ("rawBody".into(), Value::String(String::new())),
        ]));
        let empty_context = block_on_ready(plan.resolve(&empty_request, &program)).unwrap();
        assert!(matches!(
            empty_context.call("enabled", &[]).unwrap(),
            Value::Bool(false)
        ));

        let null_request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
            ("rawBody".into(), Value::String("null".into())),
        ]));
        let error = block_on_ready(plan.resolve(&null_request, &program)).unwrap_err();
        assert_eq!(error.code, "FLD4002");
        assert_eq!(error.field, "enabled");
        assert!(error.message.contains("JSON object"));
    }

    #[test]
    fn null_body_snapshot_requires_raw_body_identity() {
        let route = route(
            r#":import[field]
               fields { enabled = optional("enabled", source = body, type = bool, default = false); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let request = Value::Object(HashMap::from([
            ("query".into(), Value::Object(HashMap::new())),
            ("body".into(), Value::Null),
        ]));
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("body-null-invariant");
        let error = block_on_ready(plan.resolve(&request, &program)).unwrap_err();
        assert_eq!(error.code, "FLD5000");
        assert!(error.message.contains("rawBody"));
    }

'''
if anchor not in source:
    raise SystemExit("test insertion anchor not found")
source = source.replace(anchor, tests + anchor, 1)
field_path.write_text(source, encoding="utf-8")

doc = doc_path.read_text(encoding="utf-8")
old_doc = "Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms."
new_doc = "Named `body` bindings require a JSON object; a truly empty body behaves like a missing object so optional/default bindings still work. An explicit JSON `null` body is not treated as empty: named body bindings reject it with `FLD4002`. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms."
if old_doc not in doc:
    raise SystemExit("documentation anchor not found")
doc = doc.replace(old_doc, new_doc, 1)
doc_path.write_text(doc, encoding="utf-8")
