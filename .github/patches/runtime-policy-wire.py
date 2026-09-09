from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


path = "engine/crates/api/src/lib.rs"
text = read(path)

if "async fn server_status_gate(" not in text:
    import_anchor = "use axum::http::StatusCode;"
    if import_anchor not in text:
        raise SystemExit("missing API StatusCode import anchor")
    text = text.replace(
        import_anchor,
        "use axum::http::{HeaderValue, Method, StatusCode};",
        1,
    )

    middleware_anchor = '''        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_metrics,
        ))
'''
    if middleware_anchor not in text:
        raise SystemExit("missing request_metrics middleware anchor")
    text = text.replace(
        middleware_anchor,
        middleware_anchor
        + '''        .layer(axum::middleware::from_fn_with_state(
            runtime_image.clone(),
            server_status_gate,
        ))
''',
        1,
    )

    helper_anchor = '''async fn request_timeout(
'''
    if helper_anchor not in text:
        raise SystemExit("missing request_timeout anchor")
    helper = r'''async fn server_status_gate(
    State(runtime_image): State<std::sync::Arc<route_engine::RuntimeImageSlot>>,
    request: Request,
    next: Next,
) -> Response {
    let image = runtime_image.snapshot();
    let status = image.server_policy.status;
    let Some(rejection) = status_rejection(status, request.method(), request.uri().path()) else {
        return next.run(request).await;
    };

    let mut response = (
        rejection,
        axum::Json(serde_json::json!({
            "error": "server state rejects this request",
            "serverStatus": status.as_str(),
            "runtimeImage": image.image_id,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        "x-rbe-server-status",
        HeaderValue::from_static(status.as_str()),
    );
    if rejection == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("5"));
    }
    response
}

fn status_rejection(
    status: route_engine::ServerStatus,
    method: &Method,
    path: &str,
) -> Option<StatusCode> {
    if is_control_plane_path(path) {
        return None;
    }
    match status {
        route_engine::ServerStatus::Online => None,
        route_engine::ServerStatus::Readonly
            if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) =>
        {
            None
        }
        route_engine::ServerStatus::Readonly => Some(StatusCode::LOCKED),
        route_engine::ServerStatus::Maintenance
        | route_engine::ServerStatus::Draining
        | route_engine::ServerStatus::Offline => Some(StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn is_control_plane_path(path: &str) -> bool {
    path == "/health"
        || path == "/healthz"
        || path == "/api/admin"
        || path.starts_with("/api/admin/")
        || path == "/api/maintenance"
        || path.starts_with("/api/maintenance/")
}

'''
    text = text.replace(helper_anchor, helper + helper_anchor, 1)

    test_code = r'''

#[cfg(test)]
mod runtime_status_tests {
    use super::*;

    #[test]
    fn online_allows_normal_requests() {
        assert_eq!(
            status_rejection(route_engine::ServerStatus::Online, &Method::POST, "/api/foo"),
            None
        );
    }

    #[test]
    fn readonly_allows_reads_and_blocks_mutations() {
        assert_eq!(
            status_rejection(route_engine::ServerStatus::Readonly, &Method::GET, "/api/foo"),
            None
        );
        assert_eq!(
            status_rejection(route_engine::ServerStatus::Readonly, &Method::POST, "/api/foo"),
            Some(StatusCode::LOCKED)
        );
    }

    #[test]
    fn maintenance_and_offline_keep_control_plane_reachable() {
        for status in [
            route_engine::ServerStatus::Maintenance,
            route_engine::ServerStatus::Draining,
            route_engine::ServerStatus::Offline,
        ] {
            assert_eq!(
                status_rejection(status, &Method::GET, "/api/foo"),
                Some(StatusCode::SERVICE_UNAVAILABLE)
            );
            assert_eq!(
                status_rejection(status, &Method::POST, "/api/admin/runtime"),
                None
            );
            assert_eq!(status_rejection(status, &Method::GET, "/health"), None);
        }
    }
}
'''
    text += test_code

write(path, text)
