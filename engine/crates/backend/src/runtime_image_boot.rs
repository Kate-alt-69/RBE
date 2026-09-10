use std::path::Path;

use config::Config;
use route_engine::{MiddlewarePlan, RuntimeImage, ServerPolicy, ServerValue};
use service_runtime::ServiceCatalog;

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
        catalog,
    )?;
    let settings = effective_settings_json(config);
    let image = route_engine::compile_runtime_image(&server_source, physical, &settings)
        .map_err(|error| anyhow::anyhow!("Runtime Image compile failed: {error}"))?;
    tracing::info!(
        image = %image.image_id,
        source_hash = format_args!("{:016x}", image.source_hash),
        routes = image.routes.len(),
        modules = image.modules.len(),
        services = image.services.len(),
        server = %image.server_policy.server_name,
        status = image.server_policy.status.as_str(),
        "linked immutable Runtime Image"
    );
    Ok(image)
}

pub fn apply_middleware_plan(config: &mut Config, plan: &MiddlewarePlan) -> anyhow::Result<()> {
    for step in &plan.steps {
        match step.name.as_str() {
            "json" => {
                if let Some(value) = step.options.get("limit") {
                    let bytes = middleware_bytes(value, "json.limit")?;
                    config.security.max_json_payload_bytes = usize::try_from(bytes)
                        .map_err(|_| anyhow::anyhow!("json.limit does not fit usize"))?;
                }
            }
            "timeout" => {
                if let Some(value) = step
                    .options
                    .get("value")
                    .or_else(|| step.options.get("duration"))
                {
                    config.api.request_timeout_ms = middleware_duration_ms(value, "timeout")?;
                }
            }
            "cors" => {
                if matches!(step.options.get("enabled"), Some(ServerValue::Bool(false))) {
                    config.security.cors_allowed_origins.clear();
                    config.security.debug_cors_origins.clear();
                }
            }
            "rateLimit" => {
                if let Some(value) =
                    middleware_u64(step.options.get("windowSecs"), "rateLimit.windowSecs")?
                {
                    config.security.api_rate_limit.window_secs = value;
                }
                if let Some(value) =
                    middleware_u64(step.options.get("maxRequests"), "rateLimit.maxRequests")?
                {
                    config.security.api_rate_limit.max_requests = u32::try_from(value)
                        .map_err(|_| anyhow::anyhow!("rateLimit.maxRequests exceeds u32"))?;
                }
            }
            "ipBan" => {
                if let Some(value) =
                    middleware_u64(step.options.get("strikeThreshold"), "ipBan.strikeThreshold")?
                {
                    config.security.ip_ban.strike_threshold = u32::try_from(value)
                        .map_err(|_| anyhow::anyhow!("ipBan.strikeThreshold exceeds u32"))?;
                }
                if let Some(value) = middleware_u64(
                    step.options.get("strikeWindowSecs"),
                    "ipBan.strikeWindowSecs",
                )? {
                    config.security.ip_ban.strike_window_secs = value;
                }
                if let Some(value) =
                    middleware_u64(step.options.get("banDurationSecs"), "ipBan.banDurationSecs")?
                {
                    config.security.ip_ban.ban_duration_secs = value;
                }
            }
            "csp" => {
                if let Some(value) = step
                    .options
                    .get("value")
                    .or_else(|| step.options.get("policy"))
                {
                    match value {
                        ServerValue::String(value) => config.security.csp_policy = value.clone(),
                        _ => anyhow::bail!("csp policy must be a string"),
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn middleware_u64(value: Option<&ServerValue>, label: &str) -> anyhow::Result<Option<u64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        ServerValue::Number(value)
            if value.is_finite()
                && *value >= 0.0
                && value.fract() == 0.0
                && *value <= u64::MAX as f64 =>
        {
            Ok(Some(*value as u64))
        }
        _ => anyhow::bail!("{label} must be a non-negative integer"),
    }
}

fn middleware_bytes(value: &ServerValue, label: &str) -> anyhow::Result<u64> {
    match value {
        ServerValue::Number(value)
            if value.is_finite()
                && *value > 0.0
                && value.fract() == 0.0
                && *value <= u64::MAX as f64 =>
        {
            Ok(*value as u64)
        }
        ServerValue::Quantity { value, unit } if value.is_finite() && *value > 0.0 => {
            let multiplier = match unit.to_ascii_lowercase().as_str() {
                "b" => 1.0,
                "kb" => 1024.0,
                "mb" => 1024.0 * 1024.0,
                "gb" => 1024.0 * 1024.0 * 1024.0,
                _ => anyhow::bail!("{label} has unsupported size unit {unit:?}"),
            };
            let bytes = *value * multiplier;
            if !bytes.is_finite() || bytes <= 0.0 || bytes > u64::MAX as f64 {
                anyhow::bail!("{label} is outside the supported byte range");
            }
            Ok(bytes as u64)
        }
        _ => anyhow::bail!("{label} must be a positive byte size"),
    }
}

fn middleware_duration_ms(value: &ServerValue, label: &str) -> anyhow::Result<u64> {
    match value {
        ServerValue::Number(value)
            if value.is_finite()
                && *value > 0.0
                && value.fract() == 0.0
                && *value <= u64::MAX as f64 =>
        {
            Ok(*value as u64)
        }
        ServerValue::Quantity { value, unit } if value.is_finite() && *value > 0.0 => {
            let multiplier = match unit.to_ascii_lowercase().as_str() {
                "ms" => 1.0,
                "s" => 1000.0,
                "m" => 60_000.0,
                _ => anyhow::bail!("{label} has unsupported duration unit {unit:?}"),
            };
            let millis = *value * multiplier;
            if !millis.is_finite() || millis <= 0.0 || millis > u64::MAX as f64 {
                anyhow::bail!("{label} is outside the supported duration range");
            }
            Ok(millis as u64)
        }
        _ => anyhow::bail!("{label} must be a positive duration"),
    }
}

pub fn apply_server_policy(config: &mut Config, policy: &ServerPolicy) -> anyhow::Result<()> {
    if let Some(value) = string(policy, "listener.host")? {
        config.api.host = value;
    }
    if let Some(value) = integer(policy, "listener.port")? {
        config.api.port = u16::try_from(value)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| anyhow::anyhow!("ServerPolicy listener.port must fit 1..=65535"))?;
    }
    if let Some(value) = integer(policy, "requestTimeoutMs")? {
        config.api.request_timeout_ms = value;
    }
    if let Some(value) = integer(policy, "maxBodySizeBytes")? {
        config.api.max_body_size_bytes = usize::try_from(value)
            .map_err(|_| anyhow::anyhow!("ServerPolicy maxBodySizeBytes exceeds usize"))?;
    }
    if let Some(value) = boolean(policy, "trustedProxyHeaders")? {
        config.security.trusted_proxy_headers = value;
    }
    if let Some(value) = string_array(policy, "corsAllowedOrigins")? {
        config.security.cors_allowed_origins = value;
    }
    if let Some(value) = integer(policy, "maxJsonPayloadBytes")? {
        config.security.max_json_payload_bytes = usize::try_from(value)
            .map_err(|_| anyhow::anyhow!("ServerPolicy maxJsonPayloadBytes exceeds usize"))?;
    }
    if let Some(value) = string(policy, "cspPolicy")? {
        config.security.csp_policy = value;
    }
    Ok(())
}

fn effective_settings_json(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "api": {
            "host": config.api.host,
            "port": config.api.port,
            "requestTimeoutMs": config.api.request_timeout_ms,
            "maxBodySizeBytes": config.api.max_body_size_bytes,
        },
        "security": {
            "trustedProxyHeaders": config.security.trusted_proxy_headers,
            "corsAllowedOrigins": config.security.cors_allowed_origins,
            "maxJsonPayloadBytes": config.security.max_json_payload_bytes,
            "cspPolicy": config.security.csp_policy,
        },
        "runtimeEnv": config.runtime_env,
    })
}

fn policy_value<'a>(policy: &'a ServerPolicy, key: &str) -> Option<&'a ServerValue> {
    policy.get(key).map(|value| &value.value)
}

fn string(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<String>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::String(value) | ServerValue::Ident(value)) => Ok(Some(value.clone())),
        Some(value) => {
            anyhow::bail!("ServerPolicy {key} must be a string/identifier, got {value:?}")
        }
    }
}

fn boolean(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<bool>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Bool(value)) => Ok(Some(*value)),
        Some(value) => anyhow::bail!("ServerPolicy {key} must be boolean, got {value:?}"),
    }
}

fn integer(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<u64>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Number(value))
            if value.is_finite()
                && *value >= 0.0
                && value.fract() == 0.0
                && *value <= u64::MAX as f64 =>
        {
            Ok(Some(*value as u64))
        }
        Some(value) => {
            anyhow::bail!("ServerPolicy {key} must be a non-negative integer, got {value:?}")
        }
    }
}

fn string_array(policy: &ServerPolicy, key: &str) -> anyhow::Result<Option<Vec<String>>> {
    match policy_value(policy, key) {
        None => Ok(None),
        Some(ServerValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                ServerValue::String(value) | ServerValue::Ident(value) => Ok(value.clone()),
                other => Err(anyhow::anyhow!(
                    "ServerPolicy {key} array entries must be strings, got {other:?}"
                )),
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some),
        Some(value) => anyhow::bail!("ServerPolicy {key} must be an array, got {value:?}"),
    }
}

#[allow(dead_code)]
fn _path_marker(_: &Path) {}
