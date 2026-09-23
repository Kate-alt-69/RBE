use std::path::Path;

use config::Config;
use route_engine::{
    MiddlewarePlan, PhysicalRelSource, RelcError, RuntimeImage, ServerPolicy, ServerValue, SourceId,
};
use service_runtime::ServiceCatalog;

const RUNTIME_IMAGE_COMPILE_HELP: &str =
    "https://kastrick.vercel.app/project/rbe/doc/error-codes/runtime#rbe5100";

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
    let image = route_engine::compile_runtime_image(&server_source, physical.clone(), &settings)
        .map_err(|error| {
            anyhow::anyhow!("{}", render_runtime_image_compile_error(&error, &physical))
        })?;
    route_engine::validate_runtime_image_routes(&image)?;
    tracing::info!(
        image = %image.image_id,
        source_hash = %image.source_hash,
        routes = image.routes.len(),
        modules = image.modules.len(),
        services = image.services.len(),
        server = %image.server_policy.server_name,
        status = image.server_policy.status.as_str(),
        "linked immutable Runtime Image"
    );
    Ok(image)
}

fn render_runtime_image_compile_error(error: &RelcError, physical: &[PhysicalRelSource]) -> String {
    let compiler_error = match error {
        RelcError::Parse {
            source,
            code,
            error: parse_error,
        } => render_rel_parse_diagnostic(
            source,
            code,
            parse_error,
            error.help_url().as_str(),
            physical,
        ),
        _ => error.to_string(),
    };

    format!(
        "RBE5100 Backend could not compile the Runtime Image.\n\nError:\n{}\n\nNote:\n  Startup stopped before the API listener was bound. Fix the compiler error above and retry.\n\nHelp:\n  {RUNTIME_IMAGE_COMPILE_HELP}",
        indent_block(&compiler_error, 2)
    )
}

fn render_rel_parse_diagnostic(
    source_id: &SourceId,
    code: &str,
    parse_error: &route_engine::ParseError,
    help_url: &str,
    physical: &[PhysicalRelSource],
) -> String {
    let title = friendly_parse_message(&parse_error.message);
    let (note, hint) = parse_note_and_hint(&parse_error.message);
    let source = physical_source_for(source_id, physical);

    let Some(source) = source else {
        return format!(
            "error[{code}]: {title}\n  --> {source_id}:{}:{}\n   |\nnote: {note}\nhint: {hint}\nhelp: {help_url}",
            parse_error.line, parse_error.column
        );
    };

    let source_line = parse_error
        .line
        .checked_sub(1)
        .and_then(|index| source.source.lines().nth(index));
    let path = diagnostic_path(&source.path);
    let Some(source_line) = source_line else {
        return format!(
            "error[{code}]: {title}\n  --> {path}:{}:{}\n   |\nnote: {note}\nhint: {hint}\nhelp: {help_url}",
            parse_error.line, parse_error.column
        );
    };

    let marker_column =
        corrected_marker_column(source_line, parse_error.column, &parse_error.message);
    let display_line = expand_tabs(source_line);
    let marker_padding = marker_padding(source_line, marker_column);
    let line_no = parse_error.line.max(1);
    let width = line_no.to_string().len();
    let gutter = " ".repeat(width);

    format!(
        "error[{code}]: {title}\n  --> {path}:{line_no}:{marker_column}\n{gutter} |\n{line_no:>width$} | {display_line}\n{gutter} | {marker_padding}^ {title}\n{gutter} |\nnote: {note}\nhint: {hint}\nhelp: {help_url}",
        width = width,
    )
}

fn physical_source_for<'a>(
    source_id: &SourceId,
    physical: &'a [PhysicalRelSource],
) -> Option<&'a PhysicalRelSource> {
    physical.iter().find(|candidate| {
        SourceId::physical(candidate.kind, &candidate.logical_name)
            .ok()
            .as_ref()
            == Some(source_id)
    })
}

fn diagnostic_path(path: &Path) -> String {
    let root = runtime_paths::binary_dir();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn friendly_parse_message(message: &str) -> String {
    if let Some(token) = message.strip_prefix("unexpected token in expression: ") {
        return format!("expected an expression, found {}", token_label(token));
    }
    humanize_token_names(message)
}

fn token_label(token: &str) -> String {
    let token = token.trim();
    let symbol = match token {
        "LParen" => "(",
        "RParen" => ")",
        "LBracket" => "[",
        "RBracket" => "]",
        "LBrace" => "{",
        "RBrace" => "}",
        "Semicolon" => ";",
        "Comma" => ",",
        "Colon" => ":",
        "Dot" => ".",
        "Eq" => "=",
        "EqEq" => "==",
        "EqEqEq" => "===",
        "NotEq" => "!=",
        "NotEqEq" => "!==",
        "AndAnd" => "&&",
        "OrOr" => "||",
        "Plus" => "+",
        "Minus" => "-",
        "Star" => "*",
        "Slash" => "/",
        "Percent" => "%",
        "Lt" => "<",
        "LtEq" => "<=",
        "Gt" => ">",
        "GtEq" => ">=",
        "Eof" => "end of file",
        _ => token,
    };
    format!("`{symbol}`")
}

fn humanize_token_names(message: &str) -> String {
    [
        ("EqEqEq", "`===`"),
        ("NotEqEq", "`!==`"),
        ("EqEq", "`==`"),
        ("NotEq", "`!=`"),
        ("AndAnd", "`&&`"),
        ("OrOr", "`||`"),
        ("LtEq", "`<=`"),
        ("GtEq", "`>=`"),
        ("LParen", "`(`"),
        ("RParen", "`)`"),
        ("LBracket", "`[`"),
        ("RBracket", "`]`"),
        ("LBrace", "`{`"),
        ("RBrace", "`}`"),
        ("Semicolon", "`;`"),
        ("Comma", "`,`"),
        ("Colon", "`:`"),
        ("Dot", "`.`"),
        ("Eof", "end of file"),
    ]
    .into_iter()
    .fold(message.to_string(), |rendered, (raw, friendly)| {
        rendered.replace(raw, friendly)
    })
}

fn parse_note_and_hint(message: &str) -> (&'static str, &'static str) {
    if message.contains("unexpected token in expression: RParen") {
        return (
            "`)` closes the current expression, but REL was still waiting for an operand.",
            "check for a dangling operator immediately before `)` (for example `&& )` or `|| )`) or add the missing expression.",
        );
    }
    if message.contains("unexpected token in expression") {
        return (
            "REL expected a value or expression at the highlighted location.",
            "check nearby operators, commas and delimiters; binary operators such as `&&` and `||` need an expression on both sides.",
        );
    }
    if message.contains("expected RParen") || message.contains("expected RBracket") {
        return (
            "a delimited expression or argument list was not closed where the grammar expected it.",
            "check the opening delimiter and nested expressions before the highlighted token.",
        );
    }
    if message.contains("expected Semicolon") {
        return (
            "REL statements currently require an explicit `;` terminator.",
            "add `;` before the highlighted token if the preceding statement is complete.",
        );
    }
    if message.contains("expected identifier") {
        return (
            "this grammar position requires an identifier name.",
            "replace the highlighted token with a valid REL identifier or check the delimiter immediately before it.",
        );
    }
    (
        "the REL parser reached a token that is not valid in the current grammar position.",
        "inspect the highlighted token and the operator or delimiter immediately before it; the compiler location is where parsing could no longer continue.",
    )
}

fn unexpected_symbol(message: &str) -> Option<&'static str> {
    let token = message
        .strip_prefix("unexpected token in expression: ")?
        .trim();
    match token {
        "LParen" => Some("("),
        "RParen" => Some(")"),
        "LBracket" => Some("["),
        "RBracket" => Some("]"),
        "LBrace" => Some("{"),
        "RBrace" => Some("}"),
        "Semicolon" => Some(";"),
        "Comma" => Some(","),
        _ => None,
    }
}

fn corrected_marker_column(line: &str, reported_column: usize, message: &str) -> usize {
    let chars = line.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return 1;
    }
    let reported = reported_column.max(1).min(chars.len() + 1);
    let Some(symbol) = unexpected_symbol(message).and_then(|value| value.chars().next()) else {
        return reported;
    };

    let center = reported
        .saturating_sub(1)
        .min(chars.len().saturating_sub(1));
    for distance in 0..=4 {
        if let Some(index) = center.checked_sub(distance) {
            if chars.get(index) == Some(&symbol) {
                return index + 1;
            }
        }
        let index = center.saturating_add(distance);
        if chars.get(index) == Some(&symbol) {
            return index + 1;
        }
    }
    reported
}

fn marker_padding(line: &str, column: usize) -> String {
    let mut padding = String::new();
    for character in line.chars().take(column.saturating_sub(1)) {
        if character == '\t' {
            padding.push_str("    ");
        } else {
            padding.push(' ');
        }
    }
    padding
}

fn expand_tabs(line: &str) -> String {
    line.replace('\t', "    ")
}

fn indent_block(value: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn rel_parse_failure_renders_source_frame_note_hint_and_both_help_links() {
        let module = r#"export function broken(value) {
    if (value && ) {
        return true;
    }
    return false;
}
"#;
        let sources = vec![PhysicalRelSource::new(
            route_engine::RelSourceKind::Module,
            "broken",
            "module/broken.module",
            module,
        )];
        let error = route_engine::compile_runtime_image(
            "server Main {}",
            sources.clone(),
            &serde_json::json!({}),
        )
        .expect_err("invalid REL must fail");

        let rendered = render_runtime_image_compile_error(&error, &sources);
        assert!(rendered.starts_with("RBE5100 "));
        assert!(rendered.contains("error[REL1100]:"));
        assert!(rendered.contains("module/broken.module:2:"));
        assert!(rendered.contains("if (value && )"));
        assert!(rendered.contains("^ expected an expression"));
        assert!(rendered.contains("note:"));
        assert!(rendered.contains("hint:"));
        assert!(rendered.contains("/rel#rel1100"));
        assert!(rendered.contains("/runtime#rbe5100"));
        assert!(!rendered.contains("RBE5099"));
    }
}
