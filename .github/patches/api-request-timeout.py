from pathlib import Path

api = Path("engine/crates/api/src/lib.rs")
source = api.read_text()
source = source.replace(
    "use std::time::Instant;",
    "use std::time::{Duration, Instant};",
    1,
)
source = source.replace(
    "use axum::middleware::Next;\nuse axum::response::Response;",
    "use axum::http::StatusCode;\nuse axum::middleware::Next;\nuse axum::response::{IntoResponse, Response};",
    1,
)
old_layers = '''        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            security::security_headers,
        ))
        .layer(cors);'''
new_layers = '''        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            security::security_headers,
        ))
        .layer(cors)
        .layer(axum::middleware::from_fn_with_state(
            Duration::from_millis(state.config.api.request_timeout_ms),
            request_timeout,
        ));'''
if old_layers not in source:
    raise SystemExit("API middleware layer anchor changed")
source = source.replace(old_layers, new_layers, 1)

anchor = '''async fn request_metrics(State(state): State<AppState>, request: Request, next: Next) -> Response {'''
timeout_fn = '''async fn request_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

'''
if anchor not in source:
    raise SystemExit("API request metrics anchor changed")
source = source.replace(anchor, timeout_fn + anchor, 1)
api.write_text(source)

config = Path("engine/crates/config/src/lib.rs")
source = config.read_text()
validation_anchor = '''        if self.api.port == 0 {
            return Err(ConfigError::Invalid(
                "api.port must be a nonzero port".into(),
            ));
        }'''
validation = '''        if self.api.port == 0 {
            return Err(ConfigError::Invalid(
                "api.port must be a nonzero port".into(),
            ));
        }
        if self.api.request_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "api.requestTimeoutMs must be greater than zero".into(),
            ));
        }'''
if validation_anchor not in source:
    raise SystemExit("API config validation anchor changed")
source = source.replace(validation_anchor, validation, 1)

test_anchor = '''    #[test]
    fn auto_count_accepts_auto_null_and_number() {'''
test = '''    #[test]
    fn rejects_zero_api_request_timeout() {
        let config: Config = serde_json::from_str(
            r#"{
                "api": {
                    "host": "0.0.0.0",
                    "port": 8080,
                    "requestTimeoutMs": 0
                }
            }"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("api.requestTimeoutMs"));
    }

'''
if test_anchor not in source:
    raise SystemExit("config test anchor changed")
source = source.replace(test_anchor, test + test_anchor, 1)
config.write_text(source)
