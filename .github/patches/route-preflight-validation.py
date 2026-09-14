from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one anchor, found {count}")
    target.write_text(text.replace(old, new, 1))


lib = "engine/crates/route-engine/src/lib.rs"
replace_once(
    lib,
    '''pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    discovery::build_routes_from_image(image, service_interfaces)
}

pub fn parse_service_source''',
    '''pub fn build_routes_from_image(
    image: &RuntimeImage,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<axum::Router<core_lib::AppState>> {
    discovery::build_routes_from_image(image, service_interfaces)
}

/// Validate the immutable Runtime Image against native API namespaces and
/// route/method collisions before backend boot launches any child processes.
pub fn validate_runtime_image_routes(image: &RuntimeImage) -> anyhow::Result<()> {
    route_collision::validate_image(image)
}

pub fn parse_service_source''',
)

boot = "engine/crates/backend/src/runtime_image_boot.rs"
replace_once(
    boot,
    '''    let image = route_engine::compile_runtime_image(&server_source, physical, &settings)
        .map_err(|error| anyhow::anyhow!("Runtime Image compile failed: {error}"))?;
    tracing::info!(''',
    '''    let image = route_engine::compile_runtime_image(&server_source, physical, &settings)
        .map_err(|error| anyhow::anyhow!("Runtime Image compile failed: {error}"))?;
    route_engine::validate_runtime_image_routes(&image)?;
    tracing::info!(''',
)
