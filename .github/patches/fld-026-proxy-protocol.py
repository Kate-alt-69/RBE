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
    '''fn comma_header_values(
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
    '''fn comma_header_values(
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

fn trusted_forwarded_protocol(headers: &HeaderMap) -> Result<String, String> {
    let name = HeaderName::from_static("x-forwarded-proto");
    let present = headers.contains_key(&name);
    let protocols = comma_header_values(headers, name)?;
    if protocols.is_empty() {
        return if present {
            Err("trusted x-forwarded-proto header contains no protocol value".into())
        } else {
            Ok("http".into())
        };
    }
    for protocol in &protocols {
        if !protocol.eq_ignore_ascii_case("http") && !protocol.eq_ignore_ascii_case("https") {
            return Err(format!(
                "trusted x-forwarded-proto contains unsupported protocol {protocol:?}"
            ));
        }
    }
    Ok(protocols[0].to_ascii_lowercase())
}
''',
)

replace_once(
    DISCOVERY,
    '''    #[test]
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
    '''    #[test]
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

    #[test]
    fn trusted_forwarded_protocol_defaults_only_when_header_is_absent() {
        assert_eq!(trusted_forwarded_protocol(&HeaderMap::new()).unwrap(), "http");

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-proto"),
            HeaderValue::from_static("   "),
        );
        let error = trusted_forwarded_protocol(&headers).unwrap_err();
        assert!(error.contains("no protocol value"), "{error}");
    }

    #[test]
    fn trusted_forwarded_protocol_consumes_all_lines_and_canonicalizes() {
        let mut headers = HeaderMap::new();
        let name = HeaderName::from_static("x-forwarded-proto");
        headers.append(name.clone(), HeaderValue::from_static("HTTPS"));
        headers.append(name, HeaderValue::from_static("http"));

        assert_eq!(trusted_forwarded_protocol(&headers).unwrap(), "https");
    }

    #[test]
    fn trusted_forwarded_protocol_rejects_unknown_tokens_anywhere() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-proto"),
            HeaderValue::from_static("https, ftp"),
        );

        let error = trusted_forwarded_protocol(&headers).unwrap_err();
        assert!(error.contains("unsupported protocol"), "{error}");
        assert!(error.contains("ftp"), "{error}");
    }
}
''',
)

replace_once(
    DISCOVERY,
    '''    let protocol = if trust_proxy {
        parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| matches!(*value, "http" | "https"))
            .unwrap_or("http")
    } else {
        "http"
    };
''',
    '''    let protocol = if trust_proxy {
        trusted_forwarded_protocol(&parts.headers)
            .map_err(|error| Box::new(request_error(StatusCode::BAD_REQUEST, error)))?
    } else {
        "http".to_string()
    };
''',
)

replace_once(
    DISCOVERY,
    '''        ("protocol".into(), Value::String(protocol.to_string())),
''',
    '''        ("protocol".into(), Value::String(protocol)),
''',
)

replace_once(
    ROUTE_DOC,
    '''Forwarded client/protocol data is trusted only when `trustedProxyHeaders` policy is enabled. Otherwise `request.ip` is derived from the socket peer. When proxy trust is enabled, every textual `X-Forwarded-For` field-line is consumed in `HeaderMap` order, comma-delimited entries are flattened in order, and `request.ip` uses the first resulting entry; non-text forwarding field-lines fail with HTTP 400 instead of being ignored.
''',
    '''Forwarded client/protocol data is trusted only when `trustedProxyHeaders` policy is enabled. Otherwise `request.ip` is derived from the socket peer. When proxy trust is enabled, every textual `X-Forwarded-For` field-line is consumed in `HeaderMap` order, comma-delimited entries are flattened in order, and `request.ip` uses the first resulting entry; non-text forwarding field-lines fail with HTTP 400 instead of being ignored. `X-Forwarded-Proto` is handled across all field-lines with the same ordering rules: every supplied token must be `http` or `https` (ASCII case-insensitive), the first token becomes the canonical lowercase `request.protocol`, and malformed/empty present proxy metadata returns HTTP 400 instead of silently becoming `http`.
''',
)

print("FLD-026 patch applied")
