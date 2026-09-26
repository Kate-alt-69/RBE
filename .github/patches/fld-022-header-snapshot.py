from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DISCOVERY = ROOT / "engine/crates/route-engine/src/discovery.rs"
DOC = ROOT / "doc/field-manager.md"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    DISCOVERY,
    '''fn headers_value(headers: &HeaderMap) -> Value {
    let mut out = HashMap::new();
    for name in headers.keys() {
        let values = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        out.insert(name.as_str().to_string(), Value::String(values));
    }
    Value::Object(out)
}
''',
    '''fn headers_value(headers: &HeaderMap) -> Result<Value, String> {
    let mut out = HashMap::new();
    for name in headers.keys() {
        let mut values = Vec::new();
        for value in headers.get_all(name).iter() {
            let value = value.to_str().map_err(|_| {
                format!(
                    "header {:?} contains non-text bytes",
                    name.as_str()
                )
            })?;
            values.push(value);
        }
        out.insert(
            name.as_str().to_string(),
            Value::String(values.join(", ")),
        );
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod header_snapshot_tests {
    use super::*;

    #[test]
    fn header_snapshot_joins_repeated_text_values() {
        let mut headers = HeaderMap::new();
        let name = HeaderName::from_static("x-rbe-test");
        headers.append(name.clone(), HeaderValue::from_static("one"));
        headers.append(name, HeaderValue::from_static("two"));

        let Value::Object(values) = headers_value(&headers).unwrap() else {
            panic!("expected header object");
        };
        assert!(matches!(
            values.get("x-rbe-test"),
            Some(Value::String(value)) if value == "one, two"
        ));
    }

    #[test]
    fn header_snapshot_rejects_non_text_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-rbe-binary"),
            HeaderValue::from_bytes(&[0x80]).expect("obs-text header should parse"),
        );

        let error = headers_value(&headers).unwrap_err();
        assert!(error.contains("x-rbe-binary"), "{error}");
        assert!(error.contains("non-text bytes"), "{error}");
    }
}
''',
)

replace_once(
    DISCOVERY,
    '''    let cookies = cookies_value(&parts.headers)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;

    let fields = HashMap::from([
''',
    '''    let headers = headers_value(&parts.headers)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
    let cookies = cookies_value(&parts.headers)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;

    let fields = HashMap::from([
''',
)

replace_once(
    DISCOVERY,
    '''        ("headers".into(), headers_value(&parts.headers)),
        ("cookies".into(), cookies),
''',
    '''        ("headers".into(), headers),
        ("cookies".into(), cookies),
''',
)

replace_once(
    DOC,
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Cookie source extraction consumes every `Cookie` header field-line in request order instead of only the first one; if the same cookie name appears more than once, the later value wins consistently with the existing single-line parser behavior. Named `body` bindings require a JSON object; a truly empty body behaves like a missing object so optional/default bindings still work. An explicit JSON `null` body is not treated as empty: named body bindings reject it with `FLD4002`. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
''',
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Repeated textual header field-lines are joined with `, ` in their existing `HeaderMap` order; if any header field-line contains bytes that cannot be represented as text, request snapshot construction fails with HTTP 400 instead of silently dropping that value. Cookie source extraction consumes every `Cookie` header field-line; malformed cookie pairs and repeated cookie names are rejected with HTTP 400 instead of first/last-value-wins behavior. Named `body` bindings require a JSON object; a truly empty body behaves like a missing object so optional/default bindings still work. An explicit JSON `null` body is not treated as empty: named body bindings reject it with `FLD4002`. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
''',
)

print("FLD-022 patch applied")
