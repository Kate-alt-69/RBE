from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/x.route/README.md")

text = DISCOVERY.read_text(encoding="utf-8")

old_helper = '''fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

'''
if old_helper not in text:
    raise SystemExit("legacy header_string helper not found")
text = text.replace(old_helper, "", 1)

old_usage = '''    let user_agent = header_string(&parts.headers, header::USER_AGENT)
        .map(Value::String)
        .unwrap_or(Value::Null);
'''
new_usage = '''    let user_agent = singleton_header_string(&parts.headers, header::USER_AGENT)
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?
        .map(Value::String)
        .unwrap_or(Value::Null);
'''
if old_usage not in text:
    raise SystemExit("user-agent snapshot block not found")
text = text.replace(old_usage, new_usage, 1)

anchor = '''    #[test]
    fn singleton_host_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::HOST, HeaderValue::from_static("api.example.test"));
        headers.append(header::HOST, HeaderValue::from_static("admin.example.test"));

        let error = singleton_header_string(&headers, header::HOST).unwrap_err();
        assert!(error.contains("host"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

'''
if anchor not in text:
    raise SystemExit("header singleton test anchor not found")
extra = anchor + '''    #[test]
    fn singleton_user_agent_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::USER_AGENT, HeaderValue::from_static("client-one"));
        headers.append(header::USER_AGENT, HeaderValue::from_static("client-two"));

        let error = singleton_header_string(&headers, header::USER_AGENT).unwrap_err();
        assert!(error.contains("user-agent"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

    #[test]
    fn singleton_user_agent_rejects_non_text_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_bytes(&[0x80]).expect("obs-text header should parse"),
        );

        let error = singleton_header_string(&headers, header::USER_AGENT).unwrap_err();
        assert!(error.contains("user-agent"), "{error}");
        assert!(error.contains("non-text bytes"), "{error}");
    }

    #[test]
    fn singleton_user_agent_is_optional() {
        assert_eq!(
            singleton_header_string(&HeaderMap::new(), header::USER_AGENT).unwrap(),
            None
        );
    }

'''
text = text.replace(anchor, extra, 1)
DISCOVERY.write_text(text, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
old_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers `Content-Type`, `Host`, and `Content-Length` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation."
new_doc = "Current transport behavior joins repeatable duplicate request headers, while semantic singleton headers `Content-Type`, `Host`, `User-Agent`, and `Content-Length` are rejected when repeated so the structured snapshot cannot disagree with HTTP interpretation."
if old_doc not in doc:
    raise SystemExit("route singleton documentation anchor not found")
DOC.write_text(doc.replace(old_doc, new_doc, 1), encoding="utf-8")
