from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Config must reject path shapes that Axum's nesting parser is not intended to
# receive. The dashboard prefix is a static control-plane namespace, not a
# user-defined dynamic route pattern.
replace_once(
    "engine/crates/config/src/lib.rs",
    '''        if !self.dashboards.admin_path_prefix.starts_with('/') {
            return Err(ConfigError::Invalid(
                "dashboards.adminPathPrefix must start with '/'".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
''',
    '''        validate_dashboard_path_prefix(&self.dashboards.admin_path_prefix)?;
        Ok(())
    }
}

fn validate_dashboard_path_prefix(value: &str) -> Result<(), ConfigError> {
    let valid = (2..=128).contains(&value.len())
        && value.starts_with('/')
        && !value.ends_with('/')
        && value.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                })
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::Invalid(
            "dashboards.adminPathPrefix must be a static 1..=127 byte path such as '/admin', using only ASCII letters, digits, '.', '_' or '-' and no trailing slash"
                .into(),
        ))
    }
}

#[cfg(test)]
''',
    "dashboard static prefix validation",
)

replace_once(
    "engine/crates/config/src/lib.rs",
    '''    #[test]
    fn rejects_inconsistent_api_body_limits() {
''',
    '''    #[test]
    fn rejects_dashboard_prefixes_that_can_break_router_nesting() {
        for prefix in [
            "admin",
            "/",
            "/admin/",
            "/admin//control",
            "/admin/{id}",
            "/admin/*rest",
            "/admin control",
        ] {
            let source = serde_json::json!({
                "api": { "host": "0.0.0.0", "port": 8080 },
                "dashboards": { "adminPathPrefix": prefix }
            });
            let config: Config = serde_json::from_value(source).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("dashboards.adminPathPrefix"),
                "unexpected error for {prefix:?}: {error}"
            );
        }

        let source = serde_json::json!({
            "api": { "host": "0.0.0.0", "port": 8080 },
            "dashboards": { "adminPathPrefix": "/control-room_v2.1" }
        });
        let config: Config = serde_json::from_value(source).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_inconsistent_api_body_limits() {
''',
    "dashboard prefix validation regression",
)

# Runtime Image validation needs one additional, configured native namespace at
# API assembly time. The existing static native namespace checks remain intact.
replace_once(
    "engine/crates/route-engine/src/route_collision.rs",
    '''fn collision_error(
''',
    '''pub(crate) fn validate_image_reserved_namespace(
    image: &RuntimeImage,
    prefix: &str,
    label: &str,
) -> anyhow::Result<()> {
    let collisions = image_reserved_namespace_collisions(image, prefix, label)?;
    if collisions.is_empty() {
        return Ok(());
    }

    let report_path = write_collision_report(&collisions)?;
    Err(collision_error(
        &format!("Runtime Image {label} namespace validation failed"),
        &collisions,
        &report_path,
    ))
}

fn image_reserved_namespace_collisions(
    image: &RuntimeImage,
    prefix: &str,
    label: &str,
) -> anyhow::Result<Vec<RouteCollision>> {
    if let Some(native) = RESERVED_NATIVE_API_PREFIXES
        .iter()
        .find(|native| namespaces_overlap(prefix, native))
    {
        anyhow::bail!(
            "configured {label} namespace {prefix:?} overlaps native API namespace {native:?}"
        );
    }

    let mut collisions = Vec::new();
    for id in &image.routes {
        let manifest = image
            .source(id)
            .ok_or_else(|| anyhow::anyhow!("Runtime Image route {id} has no manifest"))?;
        let url_path = manifest
            .route_path
            .clone()
            .unwrap_or_else(|| url_path_for_logical(&manifest.logical_name));
        if is_in_native_namespace(&url_path, prefix) {
            collisions.push(RouteCollision {
                path: PathBuf::from(id.as_str()),
                message: format!(
                    "route URL `{url_path}` conflicts with configured {label} namespace `{prefix}`"
                ),
            });
        }
    }
    Ok(collisions)
}

fn namespaces_overlap(left: &str, right: &str) -> bool {
    is_in_native_namespace(left, right) || is_in_native_namespace(right, left)
}

fn collision_error(
''',
    "configured native namespace validation",
)

replace_once(
    "engine/crates/route-engine/src/route_collision.rs",
    '''    #[test]
    fn collision_error_surfaces_owner_reason_and_report_path() {
''',
    '''    #[test]
    fn configured_namespace_catches_valid_rel_before_router_assembly() {
        use crate::relc::{compile_runtime_image, PhysicalRelSource};
        use crate::source_registry::RelSourceKind;

        let image = compile_runtime_image(
            "server Main {}",
            vec![PhysicalRelSource::new(
                RelSourceKind::Route,
                "control/status",
                "api/control/status.route",
                "class Route { get(req) { return true; } }",
            )],
            &serde_json::json!({}),
        )
        .unwrap();

        let collisions = image_reserved_namespace_collisions(&image, "/api/control", "dashboard")
            .unwrap();
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].message.contains("configured dashboard namespace"));
        assert!(collisions[0].message.contains("/api/control/status"));

        let native_error =
            image_reserved_namespace_collisions(&image, "/api/auth/private", "dashboard")
                .unwrap_err()
                .to_string();
        assert!(native_error.contains("overlaps native API namespace"));
        assert!(native_error.contains("/api/auth"));
    }

    #[test]
    fn collision_error_surfaces_owner_reason_and_report_path() {
''',
    "configured namespace regression",
)

replace_once(
    "engine/crates/route-engine/src/lib.rs",
    '''pub fn validate_runtime_image_routes(image: &RuntimeImage) -> anyhow::Result<()> {
    route_collision::validate_image(image)
}

pub fn parse_field_source''',
    '''pub fn validate_runtime_image_routes(image: &RuntimeImage) -> anyhow::Result<()> {
    route_collision::validate_image(image)
}

/// Reserve one additional runtime-configured native namespace before the HTTP
/// router is assembled. This is used by optional control-plane surfaces whose
/// path is not known when RELC performs its static native-route validation.
pub fn validate_runtime_image_reserved_namespace(
    image: &RuntimeImage,
    prefix: &str,
    label: &str,
) -> anyhow::Result<()> {
    route_collision::validate_image_reserved_namespace(image, prefix, label)
}

pub fn parse_field_source''',
    "public configured namespace validation",
)

# Validate before building/mounting any REL or dashboard Router. This prevents
# valid REL + dashboard configuration from reaching Axum as an overlapping
# routing graph and turns it into a clear boot-time validation error instead.
replace_once(
    "engine/crates/api/src/lib.rs",
    '''    let cors = build_cors_layer(&state);
    let image = runtime_image.snapshot();
    let dot_route_routes =
        route_engine::build_routes_from_image(image.as_ref(), service_interfaces)?;
''',
    '''    let cors = build_cors_layer(&state);
    let image = runtime_image.snapshot();
    if state.config.dashboards.enabled {
        route_engine::validate_runtime_image_reserved_namespace(
            image.as_ref(),
            &state.config.dashboards.admin_path_prefix,
            "dashboard",
        )?;
    }
    let dot_route_routes =
        route_engine::build_routes_from_image(image.as_ref(), service_interfaces)?;
''',
    "API dashboard namespace preflight",
)
