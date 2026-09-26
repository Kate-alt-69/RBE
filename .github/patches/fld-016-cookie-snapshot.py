from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one patch anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


discovery = "engine/crates/route-engine/src/discovery.rs"
old_cookies = '''fn cookies_value(headers: &HeaderMap) -> Value {
    let mut cookies = HashMap::new();
    if let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if !name.is_empty() {
                cookies.insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }
    Value::Object(cookies)
}
'''
new_cookies = '''fn cookies_value(headers: &HeaderMap) -> Value {
    let mut cookies = HashMap::new();
    for raw in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
    {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if !name.is_empty() {
                // Cookie header field-lines are processed in HeaderMap order.
                // Preserve the existing last-value-wins behavior when a client
                // repeats the same cookie name across one or more field-lines.
                cookies.insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }
    Value::Object(cookies)
}

#[cfg(test)]
mod cookie_snapshot_tests {
    use super::*;

    #[test]
    fn cookie_snapshot_consumes_all_header_field_lines_in_order() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("first=one; shared=old"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("second=two; shared=new"),
        );

        let Value::Object(cookies) = cookies_value(&headers) else {
            panic!("expected cookie object");
        };
        assert!(matches!(cookies.get("first"), Some(Value::String(value)) if value == "one"));
        assert!(matches!(cookies.get("second"), Some(Value::String(value)) if value == "two"));
        assert!(matches!(cookies.get("shared"), Some(Value::String(value)) if value == "new"));
    }
}
'''
replace_once(discovery, old_cookies, new_cookies)

docs = "doc/field-manager.md"
old_docs = '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
'''
new_docs = '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Cookie source extraction consumes every `Cookie` header field-line in request order instead of only the first one; if the same cookie name appears more than once, the later value wins consistently with the existing single-line parser behavior. Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
'''
replace_once(docs, old_docs, new_docs)
