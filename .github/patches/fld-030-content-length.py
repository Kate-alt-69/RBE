from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/x.route/README.md")

text = DISCOVERY.read_text(encoding="utf-8")

anchor = '''fn comma_header_values(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Vec<String>, String> {
'''
if anchor not in text:
    raise SystemExit("comma_header_values anchor not found")

helper = '''fn content_length_value(headers: &HeaderMap) -> Result<Value, String> {
    let Some(raw) = singleton_header_string(headers, header::CONTENT_LENGTH)? else {
        return Ok(Value::Null);
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Content-Length must contain ASCII decimal digits only".into());
    }
    let parsed = raw
        .parse::<u64>()
        .map_err(|_| "Content-Length is outside the supported unsigned integer range".to_string())?;
    if parsed > MAX_EXACT_JSON_INTEGER as u64 {
        return Err(format!(
            "Content-Length {parsed} cannot be represented exactly by REL; maximum exact value is {MAX_EXACT_JSON_INTEGER}"
        ));
    }
    Ok(Value::Number(parsed as f64))
}

#[cfg(test)]
mod request_content_length_tests {
    use super::*;

    #[test]
    fn content_length_preserves_exact_decimal_value() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        assert!(matches!(
            content_length_value(&headers).unwrap(),
            Value::Number(value) if value == 42.0
        ));
    }

    #[test]
    fn content_length_rejects_duplicate_values() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        headers.append(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        let error = content_length_value(&headers).unwrap_err();
        assert!(error.contains("duplicate header"), "{error}");
    }

    #[test]
    fn content_length_rejects_non_decimal_syntax() {
        for raw in ["", "+1", "1.0", "1, 1", " 1"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(raw).expect("test header should be representable"),
            );
            let error = content_length_value(&headers).unwrap_err();
            assert!(error.contains("ASCII decimal digits"), "{raw:?}: {error}");
        }
    }

    #[test]
    fn content_length_rejects_lossy_rel_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_static("9007199254740992"),
        );
        let error = content_length_value(&headers).unwrap_err();
        assert!(error.contains("cannot be represented exactly by REL"), "{error}");
    }

    #[test]
    fn missing_content_length_is_null() {
        assert!(matches!(
            content_length_value(&HeaderMap::new()).unwrap(),
            Value::Null
        ));
    }
}

'''
text = text.replace(anchor, helper + anchor, 1)

old = '''    let content_length = header_string(&parts.headers, header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| Value::Number(value as f64))
        .unwrap_or(Value::Null);
'''
new = '''    let content_length = content_length_value(&parts.headers)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
'''
if old not in text:
    raise SystemExit("content_length snapshot block not found")
text = text.replace(old, new, 1)
DISCOVERY.write_text(text, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
old_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers such as `Content-Type` and `Host` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation. There is no first-class binary-body value type."
new_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers `Content-Type`, `Host`, and `Content-Length` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation. `Content-Length`, when present, must be non-empty ASCII decimal digits and no greater than `9007199254740991`, the largest integer REL can represent exactly; malformed or lossy values return HTTP 400 instead of becoming `null` or a rounded number. There is no first-class binary-body value type."
if old_doc not in doc:
    raise SystemExit("route transport documentation anchor not found")
DOC.write_text(doc.replace(old_doc, new_doc, 1), encoding="utf-8")
