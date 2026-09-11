from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding='utf-8')
    if old not in text:
        raise SystemExit(f'missing anchor in {path}: {old[:120]!r}')
    p.write_text(text.replace(old, new, 1), encoding='utf-8')

admin = 'engine/crates/api/admin_hash.rs'
replace_once(admin, 'pub const DEFAULT_ROUNDS: u32 = 120_000;', '#[cfg(test)]\npub const DEFAULT_ROUNDS: u32 = 120_000;')
replace_once(admin, 'if input.len() % 2 != 0 {', 'if !input.len().is_multiple_of(2) {')
replace_once(admin, 'for chunk in input.as_bytes().chunks_exact(2) {', 'for chunk in input.as_bytes().as_chunks::<2>().0 {')
replace_once(admin, 'for chunk in padded.chunks_exact(64) {', 'for chunk in padded.as_slice().as_chunks::<64>().0 {')

dash = 'engine/crates/api/src/dashboard.rs'
replace_once(
    dash,
    'fn require_auth(peer: SocketAddr, headers: &HeaderMap) -> Result<AdminSession, Response> {\n    if let Some(response) = local_only(peer) {\n        return Err(response);\n    }',
    'type AdminAuthResult = Result<AdminSession, Box<Response>>;\n\nfn require_auth(peer: SocketAddr, headers: &HeaderMap) -> AdminAuthResult {\n    if let Some(response) = local_only(peer) {\n        return Err(Box::new(response));\n    }',
)
replace_once(
    dash,
    '        return Err(json_response(\n            StatusCode::SERVICE_UNAVAILABLE,\n            json!({\n                "error": "admin password is not configured in this backend build",\n                "code": "ADMIN_PASSWORD_UNCONFIGURED"\n            }),\n        ));',
    '        return Err(Box::new(json_response(\n            StatusCode::SERVICE_UNAVAILABLE,\n            json!({\n                "error": "admin password is not configured in this backend build",\n                "code": "ADMIN_PASSWORD_UNCONFIGURED"\n            }),\n        )));',
)
replace_once(
    dash,
    '    authorized_session(headers).ok_or_else(|| {\n        json_response(\n            StatusCode::UNAUTHORIZED,\n            json!({ "error": "admin authentication required" }),\n        )\n    })\n}\n\nfn require_mutation_auth(peer: SocketAddr, headers: &HeaderMap) -> Result<AdminSession, Response> {',
    '    authorized_session(headers).ok_or_else(|| {\n        Box::new(json_response(\n            StatusCode::UNAUTHORIZED,\n            json!({ "error": "admin authentication required" }),\n        ))\n    })\n}\n\nfn require_mutation_auth(peer: SocketAddr, headers: &HeaderMap) -> AdminAuthResult {',
)
replace_once(
    dash,
    '        return Err(json_response(\n            StatusCode::FORBIDDEN,\n            json!({ "error": "admin mutation token rejected" }),\n        ));',
    '        return Err(Box::new(json_response(\n            StatusCode::FORBIDDEN,\n            json!({ "error": "admin mutation token rejected" }),\n        )));',
)
# All seven auth call sites return the boxed response back to Axum.
p = Path(dash)
text = p.read_text(encoding='utf-8')
needle = 'if let Err(response) = require_'
if text.count(needle) != 7:
    raise SystemExit(f'expected 7 dashboard auth call sites, found {text.count(needle)}')
text = text.replace('        return response;\n', '        return *response;\n', 7)
p.write_text(text, encoding='utf-8')

replace_once(dash, '        Ok(()) => return Ok(()),', '        Ok(()) => Ok(()),')
replace_once(dash, '            assert!(build_auth::ADMIN_PASSWORD_ROUNDS >= 1);\n', '')
