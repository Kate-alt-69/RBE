from pathlib import Path

path = Path('engine/crates/route-engine/src/discovery.rs')
text = path.read_text(encoding='utf-8')

old = '''async fn request_value(
    state: &AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Result<Value, Response> {'''
new = '''async fn request_value(
    state: &AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Result<Value, Box<Response>> {'''
if old not in text:
    raise SystemExit('missing request_value result anchor')
text = text.replace(old, new, 1)

old = '''            request_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured payload limit",
            )
        })?;'''
new = '''            Box::new(request_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured payload limit",
            ))
        })?;'''
if old not in text:
    raise SystemExit('missing payload error anchor')
text = text.replace(old, new, 1)

old = '''        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|error| {
            request_error(StatusCode::BAD_REQUEST, format!("invalid JSON request body: {error}"))
        })?;'''
new = '''        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|error| {
            Box::new(request_error(
                StatusCode::BAD_REQUEST,
                format!("invalid JSON request body: {error}"),
            ))
        })?;'''
if old not in text:
    raise SystemExit('missing JSON error anchor')
text = text.replace(old, new, 1)

old = '''            Err(response) => return response,
'''
new = '''            Err(response) => return *response,
'''
if old not in text:
    raise SystemExit('missing boxed response call-site anchor')
text = text.replace(old, new, 1)

# build_method_router still receives the URL pattern only because its callers
# log/register with it; the handler itself now reads the real URI from Request.
old = '''    url_path: String,
) -> MethodRouter<AppState> {'''
new = '''    _url_path: String,
) -> MethodRouter<AppState> {'''
if old not in text:
    raise SystemExit('missing build_method_router URL anchor')
text = text.replace(old, new, 1)

path.write_text(text, encoding='utf-8')
