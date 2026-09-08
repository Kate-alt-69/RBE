from pathlib import Path

workspace = Path("engine/Cargo.toml")
source = workspace.read_text()
old_dep = 'tower-http = { version = "0.5", features = ["trace", "cors"] }'
new_dep = 'tower-http = { version = "0.5", features = ["trace", "cors", "limit"] }'
if old_dep not in source:
    raise SystemExit("tower-http workspace dependency anchor changed")
workspace.write_text(source.replace(old_dep, new_dep, 1))

api = Path("engine/crates/api/src/lib.rs")
source = api.read_text()
source = source.replace(
    "use axum::extract::{Request, State};",
    "use axum::body::{to_bytes, Body};\nuse axum::extract::{Request, State};",
    1,
)
source = source.replace(
    "use axum::http::StatusCode;",
    "use axum::http::{header::CONTENT_TYPE, StatusCode};",
    1,
)
source = source.replace(
    "use tower_http::cors::CorsLayer;\nuse tower_http::trace::TraceLayer;",
    "use tower_http::cors::CorsLayer;\nuse tower_http::limit::RequestBodyLimitLayer;\nuse tower_http::trace::TraceLayer;",
    1,
)
old_body = '''        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.security.max_json_payload_bytes,
        ))'''
new_body = '''        // The API ceiling applies to every HTTP body, including streaming
        // consumers. JSON receives a second, stricter security ceiling below.
        .layer(RequestBodyLimitLayer::new(state.config.api.max_body_size_bytes))
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.api.max_body_size_bytes,
        ))'''
if old_body not in source:
    raise SystemExit("API body limit layer anchor changed")
source = source.replace(old_body, new_body, 1)
old_tail = '''        .layer(axum::middleware::from_fn_with_state(
            Duration::from_millis(state.config.api.request_timeout_ms),
            request_timeout,
        ));'''
new_tail = '''        .layer(axum::middleware::from_fn_with_state(
            Duration::from_millis(state.config.api.request_timeout_ms),
            request_timeout,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.config.security.max_json_payload_bytes,
            json_payload_limit,
        ));'''
if old_tail not in source:
    raise SystemExit("API timeout layer anchor changed")
source = source.replace(old_tail, new_tail, 1)

anchor = '''async fn request_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {'''
json_middleware = '''async fn json_payload_limit(
    State(max_bytes): State<usize>,
    request: Request,
    next: Next,
) -> Response {
    if !is_json_content_type(&request) {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, max_bytes).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                error = %error,
                max_bytes,
                "rejected JSON request body above configured security limit"
            );
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    };
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

fn is_json_content_type(request: &Request) -> bool {
    let Some(value) = request.headers().get(CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type.to_ascii_lowercase().ends_with("+json")
}

'''
if anchor not in source:
    raise SystemExit("API timeout function anchor changed")
source = source.replace(anchor, json_middleware + anchor, 1)
api.write_text(source)

config = Path("engine/crates/config/src/lib.rs")
source = config.read_text()
validation_anchor = '''        if self.api.request_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "api.requestTimeoutMs must be greater than zero".into(),
            ));
        }'''
validation = '''        if self.api.request_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "api.requestTimeoutMs must be greater than zero".into(),
            ));
        }
        if self.api.max_body_size_bytes == 0 {
            return Err(ConfigError::Invalid(
                "api.maxBodySizeBytes must be greater than zero".into(),
            ));
        }
        if self.security.max_json_payload_bytes == 0 {
            return Err(ConfigError::Invalid(
                "security.maxJsonPayloadBytes must be greater than zero".into(),
            ));
        }
        if self.security.max_json_payload_bytes > self.api.max_body_size_bytes {
            return Err(ConfigError::Invalid(
                "security.maxJsonPayloadBytes must not exceed api.maxBodySizeBytes".into(),
            ));
        }'''
if validation_anchor not in source:
    raise SystemExit("API timeout validation anchor changed")
source = source.replace(validation_anchor, validation, 1)

test_anchor = '''    #[test]
    fn rejects_zero_api_request_timeout() {'''
test = '''    #[test]
    fn rejects_inconsistent_api_body_limits() {
        let zero_api: Config = serde_json::from_str(
            r#"{
                "api": { "host": "0.0.0.0", "port": 8080, "maxBodySizeBytes": 0 }
            }"#,
        )
        .unwrap();
        assert!(zero_api
            .validate()
            .unwrap_err()
            .to_string()
            .contains("api.maxBodySizeBytes"));

        let zero_json: Config = serde_json::from_str(
            r#"{
                "api": { "host": "0.0.0.0", "port": 8080 },
                "security": { "maxJsonPayloadBytes": 0 }
            }"#,
        )
        .unwrap();
        assert!(zero_json
            .validate()
            .unwrap_err()
            .to_string()
            .contains("security.maxJsonPayloadBytes"));

        let inverted: Config = serde_json::from_str(
            r#"{
                "api": { "host": "0.0.0.0", "port": 8080, "maxBodySizeBytes": 1024 },
                "security": { "maxJsonPayloadBytes": 2048 }
            }"#,
        )
        .unwrap();
        assert!(inverted
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must not exceed"));
    }

'''
if test_anchor not in source:
    raise SystemExit("config API test anchor changed")
source = source.replace(test_anchor, test + test_anchor, 1)
config.write_text(source)
