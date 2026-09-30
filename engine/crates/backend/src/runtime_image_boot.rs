use std::path::Path;

use config::Config;
use route_engine::{
    MiddlewarePlan, PhysicalRelSource, RelcError, RuntimeImage, ServerCompileError, ServerPolicy,
    ServerValue, SourceId,
};
use service_runtime::ServiceCatalog;

#[path = "package_links.rs"]
pub(crate) mod package_links;

const RUNTIME_IMAGE_COMPILE_HELP: &str =
    "https://kastrick.vercel.app/project/rbe/doc/error-codes/runtime#rbe5100";
const PACKAGE_GRAPH_SETTINGS_KEY: &str = "__rbePackageGraphSha256";

pub fn compile(config: &Config, catalog: Option<&ServiceCatalog>) -> anyhow::Result<RuntimeImage> {
    let root = runtime_paths::binary_dir();
    let server_path = root.join("server.server");
    let server_source = if server_path.is_file() {
        std::fs::read_to_string(&server_path).map_err(|error| {
            anyhow::anyhow!(
                "failed to read Server REL root {}: {error}",
                server_path.display()
            )
        })?
    } else {
        tracing::warn!(
            path = %server_path.display(),
            "server.server is absent; using compatibility Server REL root"
        );
        "server Main {}\n".to_string()
    };
    let physical = route_engine::discover_physical_rel_sources(
        &route_engine::default_api_dir(),
        &route_engine::default_module_dir(),
    )?;
    let mut settings = config.settings().clone();
    let package_state = package_links::load_verified_project_package_state(&root)?;
    if let Some(package_state) = &package_state {
        settings.insert(
            PACKAGE_GRAPH_SETTINGS_KEY.to_string(),
            package_state.graph_sha256.clone(),
        );
    }
    let compile_settings = serde_json::to_string(&settings)?;
    let package_context = package_state
        .as_ref()
        .map(|package_state| package_state.context.clone());
    let package_registry = package_state
        .as_ref()
        .map(|package_state| package_state.registry.clone());
    let context = route_engine::CompilationContext {
        physical: &physical,
        package_links: package_context.as_ref(),
        package_registry: package_registry.as_ref(),
    };
    let runtime = route_engine::compile_server_with_context(
        &server_source,
        &compile_settings,
        server_path,
        context,
    )?;
    validate_runtime_image(&runtime)?;
    Ok(runtime)
}

fn validate_runtime_image(runtime: &RuntimeImage) -> anyhow::Result<()> {
    for route in &runtime.routes {
        validate_route_plan(route)?;
    }
    Ok(())
}

fn validate_route_plan(route: &route_engine::CompiledRoute) -> anyhow::Result<()> {
    if route.plan.middlewares.len() > 64 {
        anyhow::bail!(
            "RBE runtime image route {:?} contains too many middleware bindings; see {RUNTIME_IMAGE_COMPILE_HELP}",
            route.path
        );
    }
    for middleware in &route.plan.middlewares {
        validate_middleware_plan(middleware)?;
    }
    Ok(())
}

fn validate_middleware_plan(middleware: &MiddlewarePlan) -> anyhow::Result<()> {
    if middleware.module.is_empty() || middleware.function.is_empty() {
        anyhow::bail!(
            "RBE runtime image contains an invalid middleware binding; see {RUNTIME_IMAGE_COMPILE_HELP}"
        );
    }
    Ok(())
}

pub fn render_compile_error(error: &ServerCompileError) -> String {
    match error {
        ServerCompileError::Relc(RelcError::Parse(parse)) => format!(
            "{}\nhelp: {}",
            parse.render_with_source(),
            RUNTIME_IMAGE_COMPILE_HELP
        ),
        ServerCompileError::Relc(RelcError::Link(message)) => {
            format!("{message}\nhelp: {RUNTIME_IMAGE_COMPILE_HELP}")
        }
        ServerCompileError::InvalidServerRoot { .. }
        | ServerCompileError::DuplicateRoute { .. }
        | ServerCompileError::DuplicateModule { .. }
        | ServerCompileError::DuplicateMiddleware { .. }
        | ServerCompileError::InvalidModule { .. }
        | ServerCompileError::InvalidRoute { .. }
        | ServerCompileError::InvalidMiddleware { .. }
        | ServerCompileError::InvalidMiddlewareOrder { .. }
        | ServerCompileError::Settings(_) => format!("{error}\nhelp: {RUNTIME_IMAGE_COMPILE_HELP}"),
    }
}

pub fn render_relc_error(error: &RelcError) -> String {
    match error {
        RelcError::Parse(parse) => {
            format!("{}\nhelp: {}", parse.render_with_source(), RUNTIME_IMAGE_COMPILE_HELP)
        }
        RelcError::Link(message) => format!("{message}\nhelp: {RUNTIME_IMAGE_COMPILE_HELP}"),
    }
}

pub fn resolve_server_value(value: &ServerValue) -> anyhow::Result<serde_json::Value> {
    match value {
        ServerValue::Null => Ok(serde_json::Value::Null),
        ServerValue::Bool(value) => Ok(serde_json::Value::Bool(*value)),
        ServerValue::Number(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| anyhow::anyhow!("server value contains a non-finite number")),
        ServerValue::String(value) => Ok(serde_json::Value::String(value.clone())),
        ServerValue::Array(values) => values
            .iter()
            .map(resolve_server_value)
            .collect::<anyhow::Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        ServerValue::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve_server_value(value)?)))
            .collect::<anyhow::Result<serde_json::Map<_, _>>>()
            .map(serde_json::Value::Object),
    }
}

pub fn source_id_label(source_id: &SourceId) -> String {
    source_id.to_string()
}
