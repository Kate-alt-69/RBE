from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DISCOVERY = ROOT / "engine/crates/route-engine/src/discovery.rs"
ROUTE_DOC = ROOT / "doc/x.route/README.md"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    DISCOVERY,
    '''fn singleton_header_string(
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
    '''fn singleton_header_string(
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

fn comma_header_values(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Vec<String>, String> {
    let label = name.as_str().to_string();
    let mut output = Vec::new();
    for value in headers.get_all(name).iter() {
        let value = value
            .to_str()
            .map_err(|_| format!("header {label:?} contains non-text bytes"))?;
        output.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        );
    }
    Ok(output)
}
''',
)

replace_once(
    DISCOVERY,
    '''    #[test]
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
    '''    #[test]
    fn singleton_host_rejects_duplicate_field_lines() {
        let mut headers = HeaderMap::new();
        headers.append(header::HOST, HeaderValue::from_static("api.example.test"));
        headers.append(header::HOST, HeaderValue::from_static("admin.example.test"));

        let error = singleton_header_string(&headers, header::HOST).unwrap_err();
        assert!(error.contains("host"), "{error}");
        assert!(error.contains("may appear only once"), "{error}");
    }

    #[test]
    fn comma_header_values_consume_all_field_lines_in_order() {
        let mut headers = HeaderMap::new();
        let name = HeaderName::from_static("x-forwarded-for");
        headers.append(
            name.clone(),
            HeaderValue::from_static("203.0.113.10, 10.0.0.1"),
        );
        headers.append(name.clone(), HeaderValue::from_static("10.0.0.2"));

        assert_eq!(
            comma_header_values(&headers, name).unwrap(),
            vec!["203.0.113.10", "10.0.0.1", "10.0.0.2"]
        );
    }

    #[test]
    fn comma_header_values_reject_non_text_field_lines() {
        let mut headers = HeaderMap::new();
        let name = HeaderName::from_static("x-forwarded-for");
        headers.append(
            name.clone(),
            HeaderValue::from_bytes(&[0x80]).expect("obs-text header should parse"),
        );

        let error = comma_header_values(&headers, name).unwrap_err();
        assert!(error.contains("x-forwarded-for"), "{error}");
        assert!(error.contains("non-text bytes"), "{error}");
    }
}
''',
)

replace_once(
    DISCOVERY,
    '''    let trust_proxy = state.config.security.trusted_proxy_headers;
    let forwarded_for = if trust_proxy {
        parts
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| Value::String(value.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
''',
    '''    let trust_proxy = state.config.security.trusted_proxy_headers;
    let forwarded_for = if trust_proxy {
        comma_header_values(
            &parts.headers,
            HeaderName::from_static("x-forwarded-for"),
        )
        .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
''',
)

replace_once(
    ROUTE_DOC,
    '''Forwarded client/protocol data is trusted only when `trustedProxyHeaders` policy is enabled. Otherwise `request.ip` is derived from the socket peer.
''',
    '''Forwarded client/protocol data is trusted only when `trustedProxyHeaders` policy is enabled. Otherwise `request.ip` is derived from the socket peer. When proxy trust is enabled, every textual `X-Forwarded-For` field-line is consumed in `HeaderMap` order, comma-delimited entries are flattened in order, and `request.ip` uses the first resulting entry; non-text forwarding field-lines fail with HTTP 400 instead of being ignored.
''',
)

print("FLD-025 patch applied")
