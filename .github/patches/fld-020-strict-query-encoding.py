from pathlib import Path

DISCOVERY = Path("engine/crates/route-engine/src/discovery.rs")
DOC = Path("doc/field-manager.md")

source = DISCOVERY.read_text(encoding="utf-8")

start = source.index("fn query_fields(raw_query: Option<&str>) -> Result<HashMap<String, String>, String> {")
end = source.index("\nfn request_error", start)
replacement = r'''fn query_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_query_component(component: &str, label: &str) -> Result<(), String> {
    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(format!(
                        "query field {label} has truncated percent encoding at byte {index}"
                    ));
                }
                let Some(high) = query_hex_nibble(bytes[index + 1]) else {
                    return Err(format!(
                        "query field {label} has invalid percent encoding at byte {index}"
                    ));
                };
                let Some(low) = query_hex_nibble(bytes[index + 2]) else {
                    return Err(format!(
                        "query field {label} has invalid percent encoding at byte {index}"
                    ));
                };
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    std::str::from_utf8(&decoded).map(|_| ()).map_err(|_| {
        format!("query field {label} is not valid UTF-8 after percent decoding")
    })
}

fn query_fields(raw_query: Option<&str>) -> Result<HashMap<String, String>, String> {
    let raw_query = raw_query.unwrap_or_default();
    for pair in raw_query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        validate_query_component(name, "name")?;
        validate_query_component(value, "value")?;
    }

    let mut query = HashMap::new();
    for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
        let key = key.into_owned();
        let value = value.into_owned();
        if query.insert(key.clone(), value).is_some() {
            return Err(format!(
                "duplicate query field {key:?} is ambiguous; each decoded query name may appear only once"
            ));
        }
    }
    Ok(query)
}
'''
source = source[:start] + replacement + source[end:]

marker = '''    #[test]\n    fn json_content_type_detection_matches_structured_media_types() {'''
if "query_fields_reject_lossy_or_malformed_encoding" not in source:
    tests = r'''    #[test]
    fn query_fields_reject_lossy_or_malformed_encoding() {
        for raw in ["%FF=value", "key=%C3%28"] {
            let error = query_fields(Some(raw)).unwrap_err();
            assert!(error.contains("not valid UTF-8"), "{error}");
        }

        for raw in ["bad%=value", "bad%2=value", "bad%GG=value"] {
            let error = query_fields(Some(raw)).unwrap_err();
            assert!(error.contains("percent encoding"), "{error}");
        }
    }

    #[test]
    fn query_fields_keep_valid_utf8_percent_decoding() {
        let query = query_fields(Some("check=%E2%9C%93&city=S%C3%A3o+Paulo")).unwrap();
        assert_eq!(query.get("check").map(String::as_str), Some("✓"));
        assert_eq!(query.get("city").map(String::as_str), Some("São Paulo"));
    }

'''
    if marker not in source:
        raise SystemExit("http edge test marker not found")
    source = source.replace(marker, tests + marker, 1)

DISCOVERY.write_text(source, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
old = "Query names are decoded at the HTTP boundary and remain scalar identities: the same decoded query name may appear only once. Repeated names (including percent-encoded aliases that decode to the same name) are rejected with HTTP 400 instead of being silently collapsed by first/last-value-wins behavior."
new = old + " Percent escapes must be syntactically complete hexadecimal byte escapes, and decoded query names/values must be valid UTF-8; malformed or lossy identities are rejected with HTTP 400 instead of being replacement-decoded."
if old not in doc and new not in doc:
    raise SystemExit("FieldManager query identity documentation marker not found")
if new not in doc:
    doc = doc.replace(old, new, 1)
DOC.write_text(doc, encoding="utf-8")
