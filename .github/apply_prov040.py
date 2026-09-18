from pathlib import Path

path = Path("engine/crates/cloud-node/src/provider.rs")
text = path.read_text()

old_create = '''async fn create_only_response(
    response: reqwest::Response,
    kind: ProviderKind,
) -> anyhow::Result<bool> {
    let status = response.status();
    let conflict = status == StatusCode::PRECONDITION_FAILED
        || status == StatusCode::CONFLICT
        || (kind == ProviderKind::Supabase && status == StatusCode::BAD_REQUEST);
    if conflict {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(provider_http_error(response, "immutable object create").await);
    }
    Ok(true)
}
'''
new_create = '''async fn create_only_response(
    response: reqwest::Response,
    kind: ProviderKind,
) -> anyhow::Result<bool> {
    let status = response.status();
    if kind == ProviderKind::Supabase
        && (status == StatusCode::BAD_REQUEST || status == StatusCode::CONFLICT)
    {
        let (status, detail) = provider_error_parts(response).await;
        if supabase_duplicate_error(&detail) {
            return Ok(false);
        }
        return Err(provider_http_error_from_parts(
            status,
            &detail,
            "immutable object create",
        ));
    }
    if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(provider_http_error(response, "immutable object create").await);
    }
    Ok(true)
}
'''
if "supabase_duplicate_error(&detail)" not in text:
    if old_create not in text:
        raise SystemExit("create_only_response anchor not found")
    text = text.replace(old_create, new_create, 1)

old_error = '''async fn provider_http_error(mut response: reqwest::Response, operation: &str) -> anyhow::Error {
    let status = response.status();
    let mut detail = Vec::new();
    while detail.len() < MAX_PROVIDER_ERROR_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = (MAX_PROVIDER_ERROR_BYTES - detail.len()).min(chunk.len());
                detail.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let detail = String::from_utf8_lossy(&detail);
    anyhow::anyhow!("Cloud Node provider {operation} failed with HTTP {status}: {detail}")
}
'''
new_error = r'''async fn provider_error_parts(mut response: reqwest::Response) -> (StatusCode, Vec<u8>) {
    let status = response.status();
    let mut detail = Vec::new();
    while detail.len() < MAX_PROVIDER_ERROR_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = (MAX_PROVIDER_ERROR_BYTES - detail.len()).min(chunk.len());
                detail.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    (status, detail)
}

fn provider_http_error_from_parts(
    status: StatusCode,
    detail: &[u8],
    operation: &str,
) -> anyhow::Error {
    let detail = String::from_utf8_lossy(detail);
    anyhow::anyhow!("Cloud Node provider {operation} failed with HTTP {status}: {detail}")
}

async fn provider_http_error(response: reqwest::Response, operation: &str) -> anyhow::Error {
    let (status, detail) = provider_error_parts(response).await;
    provider_http_error_from_parts(status, &detail, operation)
}

fn supabase_duplicate_error(detail: &[u8]) -> bool {
    let detail = String::from_utf8_lossy(detail).to_ascii_lowercase();
    detail.contains("asset already exists")
        || detail.contains("resourcealreadyexists")
        || detail.contains("keyalreadyexists")
        || detail.contains("already_exists")
        || detail.contains("\"duplicate\"")
}
'''
if "fn supabase_duplicate_error(" not in text:
    if old_error not in text:
        raise SystemExit("provider_http_error anchor not found")
    text = text.replace(old_error, new_error, 1)

test_anchor = '''    #[test]
    fn provider_probe_records_validate_identity() {
'''
tests = r'''    #[test]
    fn supabase_duplicate_errors_are_narrow() {
        assert!(supabase_duplicate_error(
            br#"{"error":"Duplicate","message":"Asset Already Exists"}"#
        ));
        assert!(supabase_duplicate_error(
            br#"{"code":"ResourceAlreadyExists","message":"exists"}"#
        ));
        assert!(supabase_duplicate_error(
            br#"{"code":"KeyAlreadyExists","message":"exists"}"#
        ));
        assert!(supabase_duplicate_error(br#"{"code":"already_exists"}"#));
        assert!(!supabase_duplicate_error(
            br#"{"code":"InvalidRequest","message":"bad request"}"#
        ));
        assert!(!supabase_duplicate_error(
            br#"{"code":"InvalidKey","message":"bad key"}"#
        ));
    }

'''
if "fn supabase_duplicate_errors_are_narrow()" not in text:
    if test_anchor not in text:
        raise SystemExit("provider test anchor not found")
    text = text.replace(test_anchor, tests + test_anchor, 1)

path.write_text(text)
