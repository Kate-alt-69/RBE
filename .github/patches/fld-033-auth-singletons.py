from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/x.route/README.md")

text = DISCOVERY.read_text(encoding="utf-8")
old_headers = '''fn headers_value(headers: &HeaderMap) -> Result<Value, String> {
    let mut out = HashMap::new();
'''
new_headers = '''fn headers_value(headers: &HeaderMap) -> Result<Value, String> {
    for name in [header::AUTHORIZATION, header::PROXY_AUTHORIZATION] {
        singleton_header_string(headers, name)?;
    }

    let mut out = HashMap::new();
'''
if old_headers not in text:
    raise SystemExit("headers_value anchor not found")
text = text.replace(old_headers, new_headers, 1)

anchor = '''    #[test]
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

'''
if anchor not in text:
    raise SystemExit("header snapshot test anchor not found")
extra = anchor + '''    #[test]
    fn header_snapshot_rejects_duplicate_authorization_credentials() {
        let mut headers = HeaderMap::new();
        headers.append(header::AUTHORIZATION, HeaderValue::from_static("Bearer one"));
        headers.append(header::AUTHORIZATION, HeaderValue::from_static("Bearer two"));

        let error = headers_value(&headers).unwrap_err();
        assert!(error.contains("authorization"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

    #[test]
    fn header_snapshot_rejects_duplicate_proxy_authorization_credentials() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic b25l"),
        );
        headers.append(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic dHdv"),
        );

        let error = headers_value(&headers).unwrap_err();
        assert!(error.contains("proxy-authorization"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

    #[test]
    fn header_snapshot_preserves_single_authorization_value() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer one"));

        let Value::Object(values) = headers_value(&headers).unwrap() else {
            panic!("expected header object");
        };
        assert!(matches!(
            values.get("authorization"),
            Some(Value::String(value)) if value == "Bearer one"
        ));
    }

'''
text = text.replace(anchor, extra, 1)
DISCOVERY.write_text(text, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
needle = "When `Host` is present it must also parse as an HTTP authority (`host` or `host:port`, including bracketed IPv6); malformed authority text returns HTTP 400 instead of being exposed through `request.host`."
replacement = needle + " Credential-bearing `Authorization` and `Proxy-Authorization` headers are also fail-closed singletons: repeated field-lines return HTTP 400 rather than being comma-joined into a synthetic credential value."
if needle not in doc:
    raise SystemExit("host documentation anchor not found")
DOC.write_text(doc.replace(needle, replacement, 1), encoding="utf-8")
