use std::path::Path;

use config::Config;
use route_engine::{
    MiddlewarePlan, PhysicalRelSource, RelcError, RuntimeImage, ServerCompileError, ServerPolicy,
    ServerValue, SourceId,
};
use service_runtime::ServiceCatalog;

#[path = "package_links.rs"]
mod package_links;

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
    let package_links = package_links::load(&root).map_err(|error| {
        anyhow::anyhow!(
            "RBE5100 Backend could not load verified package exports for Runtime Image linking.\n\nError:\n  {error:#}\n\nNote:\n  Runtime Image startup refuses unverified or stale package-link metadata. Rehydrate the active package graph and retry.\n\nHelp:\n  {RUNTIME_IMAGE_COMPILE_HELP}"
        )
    })?;
    let image = route_engine::relc::compile_runtime_image_with_packages(
        &server_source,
        physical.clone(),
        &settings,
        &package_links,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "{}",
            render_runtime_image_compile_error(&error, &server_source, &physical)
        )
    })?;
    route_engine::validate_runtime_image_routes(&image)?;
    tracing::info!(
        image = %image.image_id,
        source_hash = %image.source_hash,
        routes = image.routes.len(),
        modules = image.modules.len(),
        services = image.services.len(),
        package_roots = package_links.roots.len(),
        server = %image.server_policy.server_name,
        status = image.server_policy.status.as_str(),
        "linked immutable Runtime Image"
    );
    Ok(image)
}

fn render_runtime_image_compile_error(
    error: &RelcError,
    server_source: &str,
    physical: &[PhysicalRelSource],
) -> String {
    let compiler_error = match error {
        RelcError::Parse {
            source,
            code,
            error: parse_error,
        } => {
            let code = classify_rel_diagnostic(code, &parse_error.message);
            let help_url = rel_help_url(code);
            render_rel_parse_diagnostic(source, code, parse_error, &help_url, physical)
        }
        RelcError::Server(ServerCompileError::Parse(parse_error)) => {
            let code = classify_rel_diagnostic("REL1100", &parse_error.message);
            let help_url = rel_help_url(code);
            let title = friendly_parse_message(&parse_error.message);
            let (note, hint) = parse_note_and_hint(code, &parse_error.message);
            render_text_source_diagnostic(
                "server.server",
                code,
                &title,
                parse_error.line,
                parse_error.column,
                server_source,
                note,
                hint,
                &help_url,
                &parse_error.message,
            )
        }
        RelcError::Server(ServerCompileError::Semantic {
            message,
            line,
            column,
        }) => {
            let code = "REL2004";
            let help_url = rel_help_url(code);
            let (note, hint) = server_semantic_note_and_hint(message);
            render_text_source_diagnostic(
                "server.server",
                code,
                message,
                *line,
                *column,
                server_source,
                note,
                hint,
                &help_url,
                message,
            )
        }
        _ => error.to_string(),
    };

    format!(
        "RBE5100 Backend could not compile the Runtime Image.\n\nError:\n{}\n\nNote:\n  Startup stopped before the API listener was bound. Fix the compiler error above and retry.\n\nHelp:\n  {RUNTIME_IMAGE_COMPILE_HELP}",
        indent_block(&compiler_error, 2)
    )
}

fn rel_help_url(code: &str) -> String {
    format!(
        "https://kastrick.vercel.app/project/rbe/doc/error-codes/rel#{}",
        code.to_ascii_lowercase()
    )
}

fn classify_rel_diagnostic<'a>(fallback: &'a str, message: &str) -> &'a str {
    if message.contains("malformed numeric literal") {
        return "REL1004";
    }
    if message.contains("project-root path") {
        return "REL1003";
    }
    if message.contains("unterminated string") || message.contains("unterminated escape") {
        return "REL1002";
    }
    if message.starts_with("unexpected character")
        || message.starts_with("single '&'")
        || message.starts_with("single '|'")
    {
        return "REL1001";
    }
    if message.contains("duplicate ") {
        return "REL1204";
    }
    if message.contains("not an HTTP verb") || message.contains("lifecycle method") {
        return "REL1205";
    }
    if message.contains(":import")
        || message.contains("import entry")
        || message.contains("import inside")
        || message.contains("module path, or service import")
    {
        return "REL1203";
    }
    if message.contains(":field[")
        || message.contains(":service[")
        || message.contains("REL directive")
    {
        return "REL1201";
    }
    if message.contains(".field source requires")
        || message.contains("source requires")
        || message.contains("must declare a key")
    {
        return "REL1206";
    }
    if message.contains("expected `function`")
        || message.contains("expected `function`, `export function`, or `class`")
        || message.contains("lifecycle class must be named")
    {
        return "REL1202";
    }
    if message.contains("class bound constants") {
        return "REL1302";
    }
    if message.contains("accepts at most")
        || message.contains("zero or one parameter")
        || message.contains("currently accept zero or one")
    {
        return "REL1303";
    }
    if message.contains("FieldManager")
        && (message.contains("must")
            || message.contains("unknown")
            || message.contains("cannot")
            || message.contains("supports"))
    {
        return "REL1304";
    }
    if message.contains("must be true or false")
        || message.contains("must be a non-empty string")
        || message.contains("unknown FieldManager type")
    {
        return "REL1301";
    }
    if message.contains("unexpected token in expression: RParen")
        || message.contains("unexpected token in expression: RBracket")
        || message.contains("unexpected token in expression: Comma")
        || message.contains("unexpected token in expression: Semicolon")
        || message.contains("unexpected token in expression: RBrace")
        || message.contains("unexpected token in expression: Eof")
    {
        return "REL1104";
    }
    if message.contains("unexpected token in expression") {
        return "REL1101";
    }
    if message.contains("expected identifier") {
        return "REL1103";
    }
    if message.contains("expected RParen")
        || message.contains("expected RBracket")
        || message.contains("expected RBrace")
        || message.contains("expected Semicolon")
        || message.contains("unterminated :service")
    {
        return "REL1102";
    }
    if message.contains("parameter list") || message.contains("argument list") {
        return "REL1106";
    }
    if message.contains("unterminated") && message.contains("function") {
        return "REL1107";
    }
    if message.contains("unexpected content") || message.contains("unexpected tokens after") {
        return "REL1105";
    }
    fallback
}

fn render_rel_parse_diagnostic(
    source_id: &SourceId,
    code: &str,
    parse_error: &route_engine::ParseError,
    help_url: &str,
    physical: &[PhysicalRelSource],
) -> String {
    let title = friendly_parse_message(&parse_error.message);
    let (note, hint) = parse_note_and_hint(code, &parse_error.message);
    let source = physical_source_for(source_id, physical);

    let Some(source) = source else {
        return format!(
            "error[{code}]: {title}\n  --> {source_id}:{}:{}\n   |\nnote: {note}\nhint: {hint}\nhelp: {help_url}",
            parse_error.line, parse_error.column
        );
    };

    render_text_source_diagnostic(
        &diagnostic_path(&source.path),
        code,
        &title,
        parse_error.line,
        parse_error.column,
        &source.source,
        note,
        hint,
        help_url,
        &parse_error.message,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_text_source_diagnostic(
    path: &str,
    code: &str,
    title: &str,
    line: usize,
    column: usize,
    source: &str,
    note: &str,
    hint: &str,
    help_url: &str,
    raw_message: &str,
) -> String {
    let source_line = line
        .checked_sub(1)
        .and_then(|index| source.lines().nth(index));
    let Some(source_line) = source_line else {
        return format!(
            "error[{code}]: {title}\n  --> {path}:{line}:{column}\n   |\nnote: {note}\nhint: {hint}\nhelp: {help_url}"
        );
    };

    let marker_column = corrected_marker_column(source_line, column, raw_message);
    let display_line = expand_tabs(source_line);
    let marker_padding = marker_padding(source_line, marker_column);
    let line_no = line.max(1);
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
        ("LBracket", "`[`") ,
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

fn parse_note_and_hint(code: &str, message: &str) -> (&'static str, &'static str) {
    match code {
        "REL1001" => (
            "the lexer found punctuation or an operator spelling that REL does not accept.",
            "replace the highlighted token with supported REL syntax; use `&&`/`||` rather than a single `&`/`|`.",
        ),
        "REL1002" => (
            "a string or escape sequence reached the end of its valid source range before closing.",
            "close the quoted string and verify the final escape sequence.",
        ),
        "REL1003" => (
            "project-root paths must use the `$$/` prefix and name a relative target.",
            "use a non-empty path such as `$$/generated/data.json`.",
        ),
        "REL1004" => (
            "REL refuses to silently coerce malformed numeric text into another value.",
            "rewrite the highlighted value as one valid number; for example use `1.23` instead of `1.2.3`.",
        ),
        "REL1102" => (
            "a delimiter, block terminator, or explicit statement `;` is missing.",
            "check matching `()`, `[]`, `{}` and the statement immediately before the highlighted token.",
        ),
        "REL1103" => (
            "this grammar position requires an identifier name.",
            "replace the highlighted token with a valid REL identifier or fix the delimiter immediately before it.",
        ),
        "REL1104" => (
            "an operator is missing an operand or the expression ended before REL could complete it.",
            "check for a dangling operator such as `&&`, `||`, a comparison, or arithmetic operator before the highlighted token.",
        ),
        "REL1105" => (
            "REL completed the preceding construct but found trailing source that cannot start another valid statement/declaration.",
            "remove the stray source or fix the preceding statement/block so parsing resumes at the intended boundary.",
        ),
        "REL1106" => (
            "a parameter or argument list has invalid item/separator syntax.",
            "verify commas, each parameter/expression, and the closing `)`.",
        ),
        "REL1107" => (
            "REL could not reconstruct a complete function/class/control-flow body.",
            "check the opening and closing braces around the highlighted declaration.",
        ),
        "REL1201" => (
            "the REL directive is malformed or uses syntax not accepted by that directive.",
            "check the directive name, brackets, fields, separators, and supported options.",
        ),
        "REL1202" => (
            "this file does not match the top-level declaration shape required by its REL source role.",
            "use the required Route/Module/Service/Field/Server declaration structure for this file.",
        ),
        "REL1203" => (
            "the import declaration is syntactically malformed before RELC can resolve its target.",
            "fix the `:import[...]` target, alias, commas, or closing bracket.",
        ),
        "REL1204" => (
            "this declaration identity must be unique in its current scope.",
            "remove the duplicate or rename one declaration/export/member/binding.",
        ),
        "REL1205" => (
            "the class member name is not supported by the active REL source role.",
            "use a supported HTTP verb for Route or a supported lifecycle member for Service.",
        ),
        "REL1206" => (
            "the active REL source role requires metadata or a structural declaration that is missing.",
            "add the required declaration shown by the compiler message.",
        ),
        "REL1301" => (
            "the value exists syntactically but has the wrong literal/type shape for this construct.",
            "replace it with one of the value types accepted by the compiler message.",
        ),
        "REL1302" => (
            "this location is restricted to compile-time constants.",
            "use a literal/array/object constant here or move dynamic work into executable REL.",
        ),
        "REL1303" => (
            "this language-owned declaration/call has a fixed parameter count.",
            "match the parameter/argument count shown by the compiler message.",
        ),
        "REL1304" => (
            "the FieldManager binding combines options or a resolver mode that is not valid together.",
            "use a supported `required`, `optional`, or `dynamic` binding shape and only its permitted options.",
        ),
        _ if message.contains("unexpected token in expression: RParen") => (
            "`)` closes the current expression, but REL was still waiting for an operand.",
            "check for a dangling operator immediately before `)` or add the missing expression.",
        ),
        _ if message.contains("unexpected token in expression") => (
            "REL expected a value or expression at the highlighted location.",
            "check nearby operators, commas and delimiters; binary operators need an expression on both sides.",
        ),
        _ => (
            "the REL parser reached a token that is not valid in the current grammar position.",
            "inspect the highlighted token and the operator or delimiter immediately before it.",
        ),
    }
}

fn server_semantic_note_and_hint(message: &str) -> (&'static str, &'static str) {
    if message.contains("duplicate") {
        return (
            "this Server REL setting/section is unique and was declared more than once.",
            "merge the configuration into one section or remove the duplicate declaration.",
        );
    }
    if message.contains("status") {
        return (
            "Server REL status is a validated runtime policy value, not an arbitrary string.",
            "use one of `online`, `maintenance`, `draining`, `readonly`, or `offline`.",
        );
    }
    if message.contains("configuration block") {
        return (
            "this Server REL setting owns nested configuration and therefore requires `{ ... }`.",
            "wrap the setting entries in a configuration block.",
        );
    }
    if message.contains("Runtime ENV") {
        return (
            "Runtime ENV defaults must be deterministic values accepted during Runtime Image compilation.",
            "use a literal or identifier value and keep each ENV default name unique.",
        );
    }
    (
        "server.server parsed, but the highlighted setting violates a Server REL semantic rule.",
        "adjust the highlighted setting according to the compiler message; do not bypass ServerPolicy validation.",
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
    fn rel_parse_failure_renders_source_frame_note_hint_and_narrow_help_link() {
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

        let rendered = render_runtime_image_compile_error(&error, "server Main {}", &sources);
        assert!(rendered.starts_with("RBE5100 "));
        assert!(rendered.contains("error[REL1104]:"));
        assert!(rendered.contains("module/broken.module:2:"));
        assert!(rendered.contains("if (value && )"));
        assert!(rendered.contains("^ expected an expression"));
        assert!(rendered.contains("note:"));
        assert!(rendered.contains("hint:"));
        assert!(rendered.contains("/rel#rel1104"));
        assert!(rendered.contains("/runtime#rbe5100"));
        assert!(!rendered.contains("RBE5099"));
    }

    #[test]
    fn malformed_number_gets_specific_rel1004_diagnostic() {
        let module = "export function broken() { return 1.2.3; }";
        let sources = vec![PhysicalRelSource::new(
            route_engine::RelSourceKind::Module,
            "broken-number",
            "module/broken-number.module",
            module,
        )];
        let error = route_engine::compile_runtime_image(
            "server Main {}",
            sources.clone(),
            &serde_json::json!({}),
        )
        .expect_err("malformed numeric literal must fail");
        let rendered = render_runtime_image_compile_error(&error, "server Main {}", &sources);
        assert!(rendered.contains("error[REL1004]:"));
        assert!(rendered.contains("malformed numeric literal"));
        assert!(rendered.contains("/rel#rel1004"));
    }

    #[test]
    fn server_semantic_failure_gets_source_frame() {
        let server = "server Main {\n    status bananas;\n}\n";
        let error = route_engine::compile_runtime_image(server, Vec::new(), &serde_json::json!({}))
            .expect_err("invalid Server REL status must fail");
        let rendered = render_runtime_image_compile_error(&error, server, &[]);
        assert!(rendered.contains("error[REL2004]:"));
        assert!(rendered.contains("server.server:2:"));
        assert!(rendered.contains("status bananas;"));
        assert!(rendered.contains("online"));
        assert!(rendered.contains("/rel#rel2004"));
    }
}
