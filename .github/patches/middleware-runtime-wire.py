from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


# Compression is a native tower-http stage when declared by Server REL.
path = "engine/Cargo.toml"
text = read(path)
text = text.replace(
    'tower-http = { version = "0.5", features = ["trace", "cors"] }',
    'tower-http = { version = "0.5", features = ["trace", "cors", "compression-full"] }',
    1,
)
write(path, text)


# Lower supported MiddlewarePlan options into the effective typed Config.
path = "engine/crates/backend/src/runtime_image_boot.rs"
text = read(path)
if "pub fn apply_middleware_plan(" not in text:
    text = text.replace(
        "use route_engine::{RuntimeImage, ServerPolicy, ServerValue};",
        "use route_engine::{MiddlewarePlan, RuntimeImage, ServerPolicy, ServerValue};",
        1,
    )
    anchor = '''pub fn apply_server_policy(config: &mut Config, policy: &ServerPolicy) -> anyhow::Result<()> {'''
    if anchor not in text:
        raise SystemExit("missing apply_server_policy anchor")
    middleware_fn = r'''pub fn apply_middleware_plan(config: &mut Config, plan: &MiddlewarePlan) -> anyhow::Result<()> {
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
                if let Some(value) = middleware_u64(step.options.get("windowSecs"), "rateLimit.windowSecs")? {
                    config.security.api_rate_limit.window_secs = value;
                }
                if let Some(value) = middleware_u64(step.options.get("maxRequests"), "rateLimit.maxRequests")? {
                    config.security.api_rate_limit.max_requests = u32::try_from(value)
                        .map_err(|_| anyhow::anyhow!("rateLimit.maxRequests exceeds u32"))?;
                }
            }
            "ipBan" => {
                if let Some(value) = middleware_u64(step.options.get("strikeThreshold"), "ipBan.strikeThreshold")? {
                    config.security.ip_ban.strike_threshold = u32::try_from(value)
                        .map_err(|_| anyhow::anyhow!("ipBan.strikeThreshold exceeds u32"))?;
                }
                if let Some(value) = middleware_u64(step.options.get("strikeWindowSecs"), "ipBan.strikeWindowSecs")? {
                    config.security.ip_ban.strike_window_secs = value;
                }
                if let Some(value) = middleware_u64(step.options.get("banDurationSecs"), "ipBan.banDurationSecs")? {
                    config.security.ip_ban.ban_duration_secs = value;
                }
            }
            "csp" => {
                if let Some(value) = step.options.get("value").or_else(|| step.options.get("policy")) {
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
    let Some(value) = value else { return Ok(None); };
    match value {
        ServerValue::Number(value)
            if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64 =>
        {
            Ok(Some(*value as u64))
        }
        _ => anyhow::bail!("{label} must be a non-negative integer"),
    }
}

fn middleware_bytes(value: &ServerValue, label: &str) -> anyhow::Result<u64> {
    match value {
        ServerValue::Number(value)
            if value.is_finite() && *value > 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64 =>
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
            if value.is_finite() && *value > 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64 =>
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

'''
    text = text.replace(anchor, middleware_fn + anchor, 1)
write(path, text)


# Apply MiddlewarePlan immediately after ServerPolicy, before config is Arc-owned.
path = "engine/crates/backend/src/main.rs"
text = read(path)
anchor = '''    runtime_image_boot::apply_server_policy(&mut config, &runtime_image.server_policy)?;
'''
if anchor not in text:
    raise SystemExit("missing ServerPolicy application anchor")
if "apply_middleware_plan" not in text[text.index(anchor):text.index(anchor) + 300]:
    text = text.replace(
        anchor,
        anchor + '''    runtime_image_boot::apply_middleware_plan(&mut config, &runtime_image.middleware_plan)?;
''',
        1,
    )
write(path, text)


# Enable response compression only when it exists in the native plan.
path = "engine/crates/api/src/lib.rs"
text = read(path)
if "CompressionLayer" not in text:
    text = text.replace(
        "use tower_http::cors::CorsLayer;",
        "use tower_http::compression::CompressionLayer;\nuse tower_http::cors::CorsLayer;",
        1,
    )
old = '''    Ok(router
        .layer(middleware)
        .layer(axum::Extension(runtime_image))
        .with_state(state))'''
if old in text:
    new = '''    let compression_enabled = runtime_image
        .snapshot()
        .middleware_plan
        .contains("compression");
    let router = router.layer(middleware);
    let router = if compression_enabled {
        router.layer(CompressionLayer::new())
    } else {
        router
    };
    Ok(router
        .layer(axum::Extension(runtime_image))
        .with_state(state))'''
    text = text.replace(old, new, 1)
elif "compression_enabled" not in text:
    raise SystemExit("missing Runtime Image router return anchor")
write(path, text)
