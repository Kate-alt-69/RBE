//! Typed Server REL policy lowering and FORCE precedence resolution.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as JsonValue;

use crate::server_rel::{ServerProgram, ServerSetting, ServerSettingBody, ServerValue};

pub const HARD_MAX_REQUEST_BODY_BYTES: u64 = 1024 * 1024 * 1024;
pub const HARD_MAX_RECURSION_DEPTH: u64 = 1024;
pub const HARD_MAX_REPEATED_SYMBOL_DEPTH: u64 = 512;
pub const HARD_MAX_OPERATION_BUDGET: u64 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOrigin {
    BuiltIn,
    ServerDefault,
    Settings,
    ForcedServer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPolicyValue {
    pub value: ServerValue,
    pub origin: PolicyOrigin,
    pub forced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Online,
    Maintenance,
    Draining,
    Readonly,
    Offline,
}

impl ServerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Maintenance => "maintenance",
            Self::Draining => "draining",
            Self::Readonly => "readonly",
            Self::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursionPolicy {
    pub max_depth: u64,
    pub repeated_symbol_depth: u64,
    pub operation_budget: u64,
}

impl Default for RecursionPolicy {
    fn default() -> Self {
        Self {
            max_depth: 128,
            repeated_symbol_depth: 64,
            operation_budget: 1_000_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerPolicy {
    pub server_name: String,
    pub status: ServerStatus,
    pub values: BTreeMap<String, ResolvedPolicyValue>,
    pub recursion: RecursionPolicy,
}

impl ServerPolicy {
    pub fn resolve(program: &ServerProgram, settings: &JsonValue) -> Result<Self, ServerPolicyError> {
        let mut values = BTreeMap::new();
        for (key, value) in builtin_policy() {
            values.insert(
                key,
                ResolvedPolicyValue {
                    value,
                    origin: PolicyOrigin::BuiltIn,
                    forced: false,
                },
            );
        }

        let mut normal = BTreeMap::new();
        let mut forced = BTreeMap::new();
        for setting in &program.settings {
            if matches!(setting.name.as_str(), "env" | "middleware") {
                continue;
            }
            flatten_server_setting("", setting, setting.forced, &mut normal, &mut forced)?;
        }

        merge_server_layer(&mut values, normal, PolicyOrigin::ServerDefault, false);
        merge_server_layer(
            &mut values,
            settings_overlay(settings),
            PolicyOrigin::Settings,
            false,
        );
        merge_server_layer(&mut values, forced, PolicyOrigin::ForcedServer, true);

        validate_policy(&values)?;
        let status = status_from_values(&values)?;
        let recursion = recursion_from_values(&values)?;

        Ok(Self {
            server_name: program.name.clone(),
            status,
            values,
            recursion,
        })
    }

    pub fn get(&self, key: &str) -> Option<&ResolvedPolicyValue> {
        self.values.get(&canonical_key(key))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPolicyError(pub String);

impl fmt::Display for ServerPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ServerPolicyError {}

fn builtin_policy() -> BTreeMap<String, ServerValue> {
    BTreeMap::from([
        ("status".into(), ServerValue::Ident("online".into())),
        ("listener.host".into(), ServerValue::String("127.0.0.1".into())),
        ("listener.port".into(), ServerValue::Number(8080.0)),
        ("requestTimeoutMs".into(), ServerValue::Number(30_000.0)),
        (
            "maxBodySizeBytes".into(),
            ServerValue::Number((10 * 1024 * 1024) as f64),
        ),
        (
            "recursion.maxDepth".into(),
            ServerValue::Number(RecursionPolicy::default().max_depth as f64),
        ),
        (
            "recursion.repeatedSymbolDepth".into(),
            ServerValue::Number(RecursionPolicy::default().repeated_symbol_depth as f64),
        ),
        (
            "recursion.operationBudget".into(),
            ServerValue::Number(RecursionPolicy::default().operation_budget as f64),
        ),
    ])
}

fn flatten_server_setting(
    prefix: &str,
    setting: &ServerSetting,
    inherited_force: bool,
    normal: &mut BTreeMap<String, ServerValue>,
    forced: &mut BTreeMap<String, ServerValue>,
) -> Result<(), ServerPolicyError> {
    let force = inherited_force || setting.forced;
    let raw_key = if prefix.is_empty() {
        setting.name.clone()
    } else {
        format!("{prefix}.{}", setting.name)
    };
    match &setting.body {
        ServerSettingBody::Block(children) => {
            for child in children {
                flatten_server_setting(&raw_key, child, force, normal, forced)?;
            }
        }
        ServerSettingBody::Flag => {
            let target = if force { forced } else { normal };
            insert_unique(target, canonical_key(&raw_key), ServerValue::Bool(true))?;
        }
        ServerSettingBody::Value(value) => {
            let target = if force { forced } else { normal };
            insert_unique(target, canonical_key(&raw_key), normalize_value(&raw_key, value)?)?;
        }
    }
    Ok(())
}

fn insert_unique(
    target: &mut BTreeMap<String, ServerValue>,
    key: String,
    value: ServerValue,
) -> Result<(), ServerPolicyError> {
    if target.insert(key.clone(), value).is_some() {
        return Err(ServerPolicyError(format!(
            "duplicate ServerPolicy value `{key}` after canonicalization"
        )));
    }
    Ok(())
}

fn merge_server_layer(
    values: &mut BTreeMap<String, ResolvedPolicyValue>,
    layer: BTreeMap<String, ServerValue>,
    origin: PolicyOrigin,
    forced: bool,
) {
    for (key, value) in layer {
        values.insert(
            key,
            ResolvedPolicyValue {
                value,
                origin,
                forced,
            },
        );
    }
}

fn settings_overlay(settings: &JsonValue) -> BTreeMap<String, ServerValue> {
    let mut out = BTreeMap::new();
    copy_json_path(settings, &["api", "host"], "listener.host", &mut out);
    copy_json_path(settings, &["api", "port"], "listener.port", &mut out);
    copy_json_path(
        settings,
        &["api", "requestTimeoutMs"],
        "requestTimeoutMs",
        &mut out,
    );
    copy_json_path(
        settings,
        &["api", "maxBodySizeBytes"],
        "maxBodySizeBytes",
        &mut out,
    );
    copy_json_path(
        settings,
        &["security", "trustedProxyHeaders"],
        "trustedProxyHeaders",
        &mut out,
    );
    copy_json_path(
        settings,
        &["security", "corsAllowedOrigins"],
        "corsAllowedOrigins",
        &mut out,
    );
    copy_json_path(
        settings,
        &["security", "maxJsonPayloadBytes"],
        "maxJsonPayloadBytes",
        &mut out,
    );
    copy_json_path(
        settings,
        &["security", "cspPolicy"],
        "cspPolicy",
        &mut out,
    );
    out
}

fn copy_json_path(
    root: &JsonValue,
    path: &[&str],
    key: &str,
    output: &mut BTreeMap<String, ServerValue>,
) {
    let mut current = root;
    for segment in path {
        let Some(next) = current.get(*segment) else {
            return;
        };
        current = next;
    }
    if let Some(value) = json_to_server_value(current) {
        output.insert(key.to_string(), value);
    }
}

fn json_to_server_value(value: &JsonValue) -> Option<ServerValue> {
    match value {
        JsonValue::Null => Some(ServerValue::Null),
        JsonValue::Bool(value) => Some(ServerValue::Bool(*value)),
        JsonValue::Number(value) => value.as_f64().map(ServerValue::Number),
        JsonValue::String(value) => Some(ServerValue::String(value.clone())),
        JsonValue::Array(items) => items
            .iter()
            .map(json_to_server_value)
            .collect::<Option<Vec<_>>>()
            .map(ServerValue::Array),
        JsonValue::Object(_) => None,
    }
}

fn normalize_value(key: &str, value: &ServerValue) -> Result<ServerValue, ServerPolicyError> {
    let canonical = canonical_key(key);
    match value {
        ServerValue::Quantity { value, unit }
            if canonical == "requestTimeoutMs" || canonical.ends_with("TimeoutMs") =>
        {
            Ok(ServerValue::Number(quantity_duration_ms(*value, unit)? as f64))
        }
        ServerValue::Quantity { value, unit }
            if canonical == "maxBodySizeBytes" || canonical.ends_with("Bytes") =>
        {
            Ok(ServerValue::Number(quantity_bytes(*value, unit)? as f64))
        }
        ServerValue::Quantity { .. } => Err(ServerPolicyError(format!(
            "quantity is not valid for ServerPolicy `{canonical}` without a typed unit contract"
        ))),
        _ => Ok(value.clone()),
    }
}

fn canonical_key(key: &str) -> String {
    match key {
        "timeout" | "requestTimeout" | "request.timeout" => "requestTimeoutMs".into(),
        "requestLimit" | "bodyLimit" | "request.maxBodySize" => "maxBodySizeBytes".into(),
        "proxy.trustedForwarding" => "trustedProxyHeaders".into(),
        other => other.to_string(),
    }
}

fn quantity_duration_ms(value: f64, unit: &str) -> Result<u64, ServerPolicyError> {
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "ms" => 1.0,
        "s" | "sec" | "secs" => 1_000.0,
        "m" | "min" | "mins" => 60_000.0,
        "h" | "hr" | "hrs" => 3_600_000.0,
        other => {
            return Err(ServerPolicyError(format!(
                "unsupported duration unit `{other}`"
            )));
        }
    };
    finite_positive_integer(value * multiplier, "duration")
}

fn quantity_bytes(value: f64, unit: &str) -> Result<u64, ServerPolicyError> {
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1024.0,
        "mb" => 1024.0 * 1024.0,
        "gb" => 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(ServerPolicyError(format!(
                "unsupported byte-size unit `{other}`"
            )));
        }
    };
    finite_positive_integer(value * multiplier, "byte size")
}

fn finite_positive_integer(value: f64, label: &str) -> Result<u64, ServerPolicyError> {
    if !value.is_finite() || value <= 0.0 || value.fract() != 0.0 || value > u64::MAX as f64 {
        return Err(ServerPolicyError(format!(
            "{label} must resolve to a positive integer"
        )));
    }
    Ok(value as u64)
}

fn validate_policy(values: &BTreeMap<String, ResolvedPolicyValue>) -> Result<(), ServerPolicyError> {
    let port = numeric(values, "listener.port")?;
    if port == 0 || port > u16::MAX as u64 {
        return Err(ServerPolicyError(
            "listener.port must be in the range 1..=65535".into(),
        ));
    }
    let body = numeric(values, "maxBodySizeBytes")?;
    if body == 0 || body > HARD_MAX_REQUEST_BODY_BYTES {
        return Err(ServerPolicyError(format!(
            "maxBodySizeBytes must be in 1..={HARD_MAX_REQUEST_BODY_BYTES}; the hard engine ceiling cannot be forced away"
        )));
    }
    let timeout = numeric(values, "requestTimeoutMs")?;
    if timeout == 0 {
        return Err(ServerPolicyError("requestTimeoutMs must be positive".into()));
    }
    Ok(())
}

fn status_from_values(
    values: &BTreeMap<String, ResolvedPolicyValue>,
) -> Result<ServerStatus, ServerPolicyError> {
    let Some(value) = values.get("status") else {
        return Ok(ServerStatus::Online);
    };
    let raw = match &value.value {
        ServerValue::Ident(value) | ServerValue::String(value) => value.as_str(),
        _ => return Err(ServerPolicyError("status must be an identifier/string".into())),
    };
    match raw.to_ascii_lowercase().as_str() {
        "online" => Ok(ServerStatus::Online),
        "maintenance" => Ok(ServerStatus::Maintenance),
        "draining" => Ok(ServerStatus::Draining),
        "readonly" => Ok(ServerStatus::Readonly),
        "offline" => Ok(ServerStatus::Offline),
        _ => Err(ServerPolicyError(format!("unsupported server status `{raw}`"))),
    }
}

fn recursion_from_values(
    values: &BTreeMap<String, ResolvedPolicyValue>,
) -> Result<RecursionPolicy, ServerPolicyError> {
    let max_depth = numeric(values, "recursion.maxDepth")?;
    let repeated_symbol_depth = numeric(values, "recursion.repeatedSymbolDepth")?;
    let operation_budget = numeric(values, "recursion.operationBudget")?;
    if max_depth == 0 || max_depth > HARD_MAX_RECURSION_DEPTH {
        return Err(ServerPolicyError(format!(
            "recursion.maxDepth must be in 1..={HARD_MAX_RECURSION_DEPTH}"
        )));
    }
    if repeated_symbol_depth == 0 || repeated_symbol_depth > HARD_MAX_REPEATED_SYMBOL_DEPTH {
        return Err(ServerPolicyError(format!(
            "recursion.repeatedSymbolDepth must be in 1..={HARD_MAX_REPEATED_SYMBOL_DEPTH}"
        )));
    }
    if operation_budget == 0 || operation_budget > HARD_MAX_OPERATION_BUDGET {
        return Err(ServerPolicyError(format!(
            "recursion.operationBudget must be in 1..={HARD_MAX_OPERATION_BUDGET}"
        )));
    }
    Ok(RecursionPolicy {
        max_depth,
        repeated_symbol_depth,
        operation_budget,
    })
}

fn numeric(
    values: &BTreeMap<String, ResolvedPolicyValue>,
    key: &str,
) -> Result<u64, ServerPolicyError> {
    let value = values
        .get(key)
        .ok_or_else(|| ServerPolicyError(format!("missing resolved ServerPolicy `{key}`")))?;
    match value.value {
        ServerValue::Number(value) => finite_positive_integer(value, key),
        _ => Err(ServerPolicyError(format!("ServerPolicy `{key}` must be numeric"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_rel::compile_server_source;

    #[test]
    fn settings_override_defaults_but_force_overrides_settings() {
        let program = compile_server_source(
            r#"server Main {
                listener { host "server"; force port 7044; }
                timeout 5s;
            }"#,
        )
        .unwrap();
        let settings = serde_json::json!({
            "api": { "host": "operator", "port": 9000, "requestTimeoutMs": 12000, "maxBodySizeBytes": 1048576 }
        });
        let policy = ServerPolicy::resolve(&program, &settings).unwrap();
        assert!(matches!(
            &policy.get("listener.host").unwrap().value,
            ServerValue::String(value) if value == "operator"
        ));
        assert_eq!(policy.get("listener.port").unwrap().origin, PolicyOrigin::ForcedServer);
        assert!(matches!(
            policy.get("listener.port").unwrap().value,
            ServerValue::Number(7044.0)
        ));
        assert!(matches!(
            policy.get("requestTimeoutMs").unwrap().value,
            ServerValue::Number(12000.0)
        ));
    }

    #[test]
    fn hard_ceiling_cannot_be_forced_away() {
        let program = compile_server_source(&format!(
            "server Main {{ force maxBodySizeBytes {}; }}",
            HARD_MAX_REQUEST_BODY_BYTES + 1
        ))
        .unwrap();
        assert!(ServerPolicy::resolve(&program, &serde_json::json!({})).is_err());
    }
}