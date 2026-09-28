from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/x.route/README.md")

text = DISCOVERY.read_text(encoding="utf-8")
old_import = "use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};"
new_import = "use axum::http::{header, uri::Authority, HeaderMap, HeaderName, HeaderValue, StatusCode};"
if old_import not in text:
    raise SystemExit("axum http import anchor not found")
text = text.replace(old_import, new_import, 1)

anchor = '''fn content_length_value(headers: &HeaderMap) -> Result<Value, String> {
'''
if anchor not in text:
    raise SystemExit("content_length_value anchor not found")
helper = '''fn host_value(headers: &HeaderMap) -> Result<Value, String> {
    let Some(raw) = singleton_header_string(headers, header::HOST)? else {
        return Ok(Value::Null);
    };
    raw.parse::<Authority>()
        .map_err(|error| format!("Host header is not a valid HTTP authority: {error}"))?;
    Ok(Value::String(raw))
}

'''
text = text.replace(anchor, helper + anchor, 1)

old_host = '''    let host = singleton_header_string(&parts.headers, header::HOST)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?
        .map(Value::String)
        .unwrap_or(Value::Null);
'''
new_host = '''    let host = host_value(&parts.headers)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?;
'''
if old_host not in text:
    raise SystemExit("request host snapshot block not found")
text = text.replace(old_host, new_host, 1)

anchor_test = '''    #[test]
    fn singleton_host_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::HOST, HeaderValue::from_static("api.example.test"));
        headers.append(header::HOST, HeaderValue::from_static("admin.example.test"));

        let error = singleton_header_string(&headers, header::HOST).unwrap_err();
        assert!(error.contains("host"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

'''
if anchor_test not in text:
    raise SystemExit("host singleton test anchor not found")
extra = anchor_test + '''    #[test]
    fn host_snapshot_accepts_valid_http_authorities() {
        for raw in ["example.test", "example.test:8443", "[2001:db8::1]:443"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::HOST,
                HeaderValue::from_str(raw).expect("valid host test header"),
            );
            assert!(matches!(
                host_value(&headers).unwrap(),
                Value::String(value) if value == raw
            ));
        }
    }

    #[test]
    fn host_snapshot_rejects_invalid_http_authorities() {
        for raw in ["", "bad host", "good.test, evil.test", "http://example.test"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::HOST,
                HeaderValue::from_str(raw).expect("invalid authority should still be representable as a header"),
            );
            let error = host_value(&headers).unwrap_err();
            assert!(error.contains("valid HTTP authority"), "{raw:?}: {error}");
        }
    }

    #[test]
    fn missing_host_is_null() {
        assert!(matches!(host_value(&HeaderMap::new()).unwrap(), Value::Null));
    }

'''
text = text.replace(anchor_test, extra, 1)
DISCOVERY.write_text(text, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
old_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers `Content-Type`, `Host`, `User-Agent`, and `Content-Length` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation."
new_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers `Content-Type`, `Host`, `User-Agent`, and `Content-Length` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation. When `Host` is present it must also parse as an HTTP authority (`host` or `host:port`, including bracketed IPv6); malformed authority text returns HTTP 400 instead of being exposed through `request.host`."
if old_doc not in doc:
    raise SystemExit("singleton header documentation anchor not found")
DOC.write_text(doc.replace(old_doc, new_doc, 1), encoding="utf-8")
