from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DISCOVERY = ROOT / "engine/crates/route-engine/src/discovery.rs"
FIELD_DOC = ROOT / "doc/field-manager.md"
ROUTE_DOC = ROOT / "doc/x.route/README.md"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    DISCOVERY,
    '''async fn request_value(
    state: &AppState,
''',
    '''fn request_body_values(raw: &[u8], content_type: Option<&str>) -> Result<(Value, String), String> {
    let raw_body = std::str::from_utf8(raw)
        .map_err(|error| format!("request body is not valid UTF-8: {error}"))?
        .to_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else if is_json_content_type(content_type) {
        let parsed = serde_json::from_str::<serde_json::Value>(&raw_body)
            .map_err(|error| format!("invalid JSON request body: {error}"))?;
        json_to_value(parsed)
    } else {
        Value::String(raw_body.clone())
    };
    Ok((body, raw_body))
}

#[cfg(test)]
mod request_body_snapshot_tests {
    use super::*;

    #[test]
    fn text_body_and_raw_body_share_exact_utf8() {
        let (body, raw_body) = request_body_values("héllo".as_bytes(), Some("text/plain")).unwrap();
        assert!(matches!(body, Value::String(value) if value == "héllo"));
        assert_eq!(raw_body, "héllo");
    }

    #[test]
    fn request_body_rejects_invalid_utf8() {
        let error = request_body_values(&[0x66, 0x80, 0x6f], Some("text/plain")).unwrap_err();
        assert!(error.contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn json_body_keeps_semantic_and_raw_views() {
        let raw = br#"{"enabled":true,"count":7}"#;
        let (body, raw_body) = request_body_values(raw, Some("application/json; charset=utf-8")).unwrap();
        let Value::Object(values) = body else {
            panic!("expected JSON object");
        };
        assert!(matches!(values.get("enabled"), Some(Value::Bool(true))));
        assert!(matches!(values.get("count"), Some(Value::Number(value)) if *value == 7.0));
        assert_eq!(raw_body, r#"{"enabled":true,"count":7}"#);
    }
}

async fn request_value(
    state: &AppState,
''',
)

replace_once(
    DISCOVERY,
    '''    let content_type = header_string(&parts.headers, header::CONTENT_TYPE);
    let body_value = if raw.is_empty() {
        Value::Null
    } else if is_json_content_type(content_type.as_deref()) {
        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|error| {
            Box::new(request_error(
                StatusCode::BAD_REQUEST,
                format!("invalid JSON request body: {error}"),
            ))
        })?;
        json_to_value(parsed)
    } else {
        Value::String(String::from_utf8_lossy(&raw).into_owned())
    };
''',
    '''    let content_type = header_string(&parts.headers, header::CONTENT_TYPE);
    let (body_value, raw_body) = request_body_values(&raw, content_type.as_deref())
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
''',
)

replace_once(
    DISCOVERY,
    '''        ("body".into(), body_value),
        (
            "rawBody".into(),
            Value::String(String::from_utf8_lossy(&raw).into_owned()),
        ),
''',
    '''        ("body".into(), body_value),
        ("rawBody".into(), Value::String(raw_body)),
''',
)

replace_once(
    FIELD_DOC,
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Repeated textual header field-lines are joined with `, ` in their existing `HeaderMap` order; if any header field-line contains bytes that cannot be represented as text, request snapshot construction fails with HTTP 400 instead of silently dropping that value. Cookie source extraction consumes every `Cookie` header field-line; malformed cookie pairs and repeated cookie names are rejected with HTTP 400 instead of first/last-value-wins behavior. Named `body` bindings require a JSON object; a truly empty body behaves like a missing object so optional/default bindings still work. An explicit JSON `null` body is not treated as empty: named body bindings reject it with `FLD4002`. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
''',
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Repeated textual header field-lines are joined with `, ` in their existing `HeaderMap` order; if any header field-line contains bytes that cannot be represented as text, request snapshot construction fails with HTTP 400 instead of silently dropping that value. Cookie source extraction consumes every `Cookie` header field-line; malformed cookie pairs and repeated cookie names are rejected with HTTP 400 instead of first/last-value-wins behavior. Named `body` bindings require a JSON object; a truly empty body behaves like a missing object so optional/default bindings still work. An explicit JSON `null` body is not treated as empty: named body bindings reject it with `FLD4002`. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies. Request-body bytes are decoded strictly as UTF-8 before entering the REL snapshot; invalid UTF-8 returns HTTP 400, and `body` / `rawBody` are never replacement-decoded into different or lossy values.
''',
)

replace_once(
    ROUTE_DOC,
    '''JSON bodies are decoded to REL values. Other current v1 body types are exposed as strings. Invalid JSON returns HTTP 400 and configured body-limit violations return HTTP 413 before REL execution.
''',
    '''JSON bodies are decoded to REL values. Other current v1 body types are exposed as UTF-8 strings; invalid UTF-8 returns HTTP 400 rather than being replacement-decoded. Invalid JSON returns HTTP 400 and configured body-limit violations return HTTP 413 before REL execution. Route REL v1 has no first-class binary request-body value type.
''',
)

print("FLD-023 patch applied")
