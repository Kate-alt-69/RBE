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
    '''fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}
''',
    '''fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn singleton_header_string(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Option<String>, String> {
    let label = name.as_str().to_string();
    let values = headers.get_all(name);
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(format!(
            "duplicate header {label:?} is ambiguous; {label} may appear only once"
        ));
    }
    value
        .to_str()
        .map(|value| Some(value.to_owned()))
        .map_err(|_| format!("header {label:?} contains non-text bytes"))
}
''',
)

replace_once(
    DISCOVERY,
    '''    #[test]
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
    '''    #[test]
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

    #[test]
    fn singleton_content_type_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.append(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

        let error = singleton_header_string(&headers, header::CONTENT_TYPE).unwrap_err();
        assert!(error.contains("content-type"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

    #[test]
    fn singleton_host_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::HOST, HeaderValue::from_static("api.example.test"));
        headers.append(header::HOST, HeaderValue::from_static("admin.example.test"));

        let error = singleton_header_string(&headers, header::HOST).unwrap_err();
        assert!(error.contains("host"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }
}
''',
)

replace_once(
    DISCOVERY,
    '''    let content_type = header_string(&parts.headers, header::CONTENT_TYPE);
    let (body_value, raw_body) = request_body_values(&raw, content_type.as_deref())
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
''',
    '''    let content_type = singleton_header_string(&parts.headers, header::CONTENT_TYPE)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
    let (body_value, raw_body) = request_body_values(&raw, content_type.as_deref())
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
''',
)

replace_once(
    DISCOVERY,
    '''    let host = header_string(&parts.headers, header::HOST)
        .map(Value::String)
        .unwrap_or(Value::Null);
''',
    '''    let host = singleton_header_string(&parts.headers, header::HOST)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?
        .map(Value::String)
        .unwrap_or(Value::Null);
''',
)

replace_once(
    FIELD_DOC,
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Repeated textual header field-lines are joined with `, ` in their existing `HeaderMap` order; if any header field-line contains bytes that cannot be represented as text, request snapshot construction fails with HTTP 400 instead of silently dropping that value. Cookie source extraction consumes every `Cookie` header field-line; malformed cookie pairs and repeated cookie names are rejected with HTTP 400 instead of first/last-value-wins behavior.''',
    '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Repeated textual header field-lines are joined with `, ` in their existing `HeaderMap` order; if any header field-line contains bytes that cannot be represented as text, request snapshot construction fails with HTTP 400 instead of silently dropping that value. `Content-Type` and `Host` are semantic singleton headers: repeating either one is rejected with HTTP 400 so body decoding / `req.host` cannot use a first value while `req.headers` exposes a joined value. Cookie source extraction consumes every `Cookie` header field-line; malformed cookie pairs and repeated cookie names are rejected with HTTP 400 instead of first/last-value-wins behavior.''',
)

replace_once(
    ROUTE_DOC,
    '''Current transport limitations include collapsed duplicate query keys, joined duplicate request headers, and no first-class binary-body value type.
''',
    '''Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers such as `Content-Type` and `Host` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation. There is no first-class binary-body value type.
''',
)

print("FLD-024 patch applied")
