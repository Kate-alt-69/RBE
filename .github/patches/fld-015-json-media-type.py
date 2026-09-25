from pathlib import Path

source_path = Path("engine/crates/route-engine/src/discovery.rs")
doc_path = Path("doc/field-manager.md")

source = source_path.read_text(encoding="utf-8")
doc = doc_path.read_text(encoding="utf-8")

anchor = '''fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

'''
replacement = anchor + '''fn is_json_content_type(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type.to_ascii_lowercase().ends_with("+json")
}

'''
assert anchor in source, "header helper anchor changed"
source = source.replace(anchor, replacement, 1)

old_condition = '''    } else if content_type
        .as_deref()
        .is_some_and(|value| value.contains("application/json") || value.contains("+json"))
    {
'''
new_condition = '''    } else if is_json_content_type(content_type.as_deref()) {
'''
assert old_condition in source, "request JSON content-type condition changed"
source = source.replace(old_condition, new_condition, 1)

test_anchor = '''    #[test]
    fn field_resolution_errors_keep_client_and_internal_boundaries_separate() {
'''
assert test_anchor in source, "HTTP edge test anchor changed"
new_tests = '''    #[test]
    fn json_content_type_detection_matches_structured_media_types() {
        for content_type in [
            "application/json",
            "Application/JSON; Charset=UTF-8",
            "application/problem+json",
            "APPLICATION/VND.API+JSON; charset=utf-8",
        ] {
            assert!(is_json_content_type(Some(content_type)), "{content_type}");
        }

        for content_type in [
            "text/plain",
            "text/application/jsonish",
            "application/problem+jsonx",
            "application/jsonish",
        ] {
            assert!(!is_json_content_type(Some(content_type)), "{content_type}");
        }
        assert!(!is_json_content_type(None));
    }

'''
source = source.replace(test_anchor, new_tests + test_anchor, 1)

doc_anchor = '''Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.
'''
doc_replacement = doc_anchor + '''
JSON body detection follows the HTTP media type rather than substring matching: `application/json` and structured `+json` media types are recognized case-insensitively, parameters such as `charset` are ignored, and lookalikes such as `application/jsonish` remain text. This keeps `source = body` deterministic with the same JSON media-type rule used by RBE's API layer.
'''
assert doc_anchor in doc, "FieldManager multi-source documentation anchor changed"
doc = doc.replace(doc_anchor, doc_replacement, 1)

source_path.write_text(source, encoding="utf-8")
doc_path.write_text(doc, encoding="utf-8")
