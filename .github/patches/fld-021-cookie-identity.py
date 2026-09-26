from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/field-manager.md")

source = DISCOVERY.read_text(encoding="utf-8")
old = '''fn cookies_value(headers: &HeaderMap) -> Value {
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
'''
new = '''fn cookies_value(headers: &HeaderMap) -> Result<Value, String> {
    let mut cookies = HashMap::new();
    for header_value in headers.get_all(header::COOKIE).iter() {
        let raw = header_value
            .to_str()
            .map_err(|_| "Cookie header contains non-text bytes".to_string())?;
        for part in raw.split(';').map(str::trim).filter(|part| !part.is_empty()) {
            let Some((name, value)) = part.split_once('=') else {
                return Err(format!("malformed Cookie pair {part:?}; expected name=value"));
            };
            if name.is_empty() || name.trim() != name {
                return Err(format!("malformed Cookie name {name:?}"));
            }
            if cookies
                .insert(name.to_string(), Value::String(value.to_string()))
                .is_some()
            {
                return Err(format!(
                    "duplicate cookie field {name:?} is ambiguous; each cookie name may appear only once"
                ));
            }
        }
    }
    Ok(Value::Object(cookies))
}
'''
if old not in source:
    raise SystemExit("cookies_value implementation marker not found")
source = source.replace(old, new, 1)

old_test = '''        let Value::Object(cookies) = cookies_value(&headers) else {
            panic!("expected cookie object");
        };'''
new_test = '''        let Value::Object(cookies) = cookies_value(&headers).unwrap() else {
            panic!("expected cookie object");
        };'''
if old_test not in source:
    raise SystemExit("cookie snapshot test marker not found")
source = source.replace(old_test, new_test, 1)

close_marker = '''    }
}

fn query_hex_nibble'''
extra_tests = '''    }

    #[test]
    fn cookie_snapshot_rejects_duplicate_names_in_one_field_line() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("session=old; session=new"),
        );

        let error = cookies_value(&headers).unwrap_err();
        assert!(error.contains("duplicate cookie field \"session\""), "{error}");
    }

    #[test]
    fn cookie_snapshot_rejects_duplicate_names_across_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("session=old"));
        headers.append(header::COOKIE, HeaderValue::from_static("session=new"));

        let error = cookies_value(&headers).unwrap_err();
        assert!(error.contains("duplicate cookie field \"session\""), "{error}");
    }

    #[test]
    fn cookie_snapshot_rejects_malformed_pairs() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("session=ok; malformed"),
        );
        let error = cookies_value(&headers).unwrap_err();
        assert!(error.contains("malformed Cookie pair"), "{error}");

        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("=missing-name"));
        let error = cookies_value(&headers).unwrap_err();
        assert!(error.contains("malformed Cookie name"), "{error}");
    }
}

fn query_hex_nibble'''
if close_marker not in source:
    raise SystemExit("cookie snapshot module close marker not found")
source = source.replace(close_marker, extra_tests, 1)

content_length = '''    let content_length = header_string(&parts.headers, header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| Value::Number(value as f64))
        .unwrap_or(Value::Null);

    let fields = HashMap::from(['''
content_length_new = '''    let content_length = header_string(&parts.headers, header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| Value::Number(value as f64))
        .unwrap_or(Value::Null);
    let cookies = cookies_value(&parts.headers).map_err(|error| {
        Box::new(request_error(StatusCode::BAD_REQUEST, error))
    })?;

    let fields = HashMap::from(['''
if content_length not in source:
    raise SystemExit("request snapshot cookie insertion marker not found")
source = source.replace(content_length, content_length_new, 1)

old_field = '''        ("headers".into(), headers_value(&parts.headers)),
        ("cookies".into(), cookies_value(&parts.headers)),
        ("body".into(), body_value),'''
new_field = '''        ("headers".into(), headers_value(&parts.headers)),
        ("cookies".into(), cookies),
        ("body".into(), body_value),'''
if old_field not in source:
    raise SystemExit("request snapshot cookies field marker not found")
source = source.replace(old_field, new_field, 1)

DISCOVERY.write_text(source, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
needle = "malformed or lossy identities are rejected with HTTP 400 instead of being replacement-decoded."
addition = needle + " Cookie names are scalar identities too: malformed cookie pairs and repeated cookie names across one or more `Cookie` header field-lines are rejected with HTTP 400 instead of first/last-value-wins behavior."
if needle not in doc and addition not in doc:
    raise SystemExit("FieldManager request identity documentation marker not found")
if addition not in doc:
    doc = doc.replace(needle, addition, 1)
DOC.write_text(doc, encoding="utf-8")
