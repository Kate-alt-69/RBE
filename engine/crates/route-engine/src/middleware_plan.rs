//! Deterministic native middleware lowering from Server REL.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::server_rel::{ServerProgram, ServerSetting, ServerSettingBody, ServerValue};

const KNOWN_MIDDLEWARE: &[&str] = &[
    "correlationId",
    "realIp",
    "forwarded",
    "requestTiming",
    "requestLog",
    "json",
    "text",
    "form",
    "multipart",
    "rawBody",
    "cookies",
    "cors",
    "compression",
    "securityHeaders",
    "csp",
    "hsts",
    "rateLimit",
    "ipBan",
    "timeout",
    "cache",
    "etag",
    "auth",
    "errorHandler",
];

#[derive(Debug, Clone, PartialEq)]
pub struct MiddlewareStep {
    pub name: String,
    pub options: BTreeMap<String, ServerValue>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MiddlewarePlan {
    pub steps: Vec<MiddlewareStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewarePlanError(pub String);

impl fmt::Display for MiddlewarePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MiddlewarePlanError {}

impl MiddlewarePlan {
    pub fn lower(program: &ServerProgram) -> Result<Self, MiddlewarePlanError> {
        let Some(middleware) = program.setting("middleware") else {
            return Ok(Self::default());
        };
        let ServerSettingBody::Block(entries) = &middleware.body else {
            return Err(MiddlewarePlanError(
                "Server REL middleware must be a block".into(),
            ));
        };

        let mut seen = BTreeSet::new();
        let mut steps = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let canonical = canonical_name(&entry.name).ok_or_else(|| {
                MiddlewarePlanError(format!(
                    "unknown native middleware `{}`; known middleware: {}",
                    entry.name,
                    KNOWN_MIDDLEWARE.join(", ")
                ))
            })?;
            if !seen.insert(canonical.to_string()) {
                return Err(MiddlewarePlanError(format!(
                    "duplicate native middleware `{canonical}`"
                )));
            }
            if canonical == "errorHandler" && index + 1 != entries.len() {
                return Err(MiddlewarePlanError(
                    "errorHandler must be the final native middleware stage".into(),
                ));
            }

            let mut options = BTreeMap::new();
            match &entry.body {
                ServerSettingBody::Flag => {}
                ServerSettingBody::Value(value) => {
                    options.insert("value".into(), value.clone());
                }
                ServerSettingBody::Block(children) => {
                    flatten_options("", children, &mut options)?;
                }
            }
            validate_step(canonical, &options)?;
            steps.push(MiddlewareStep {
                name: canonical.to_string(),
                options,
            });
        }

        Ok(Self { steps })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.steps.iter().any(|step| step.name == name)
    }
}

fn flatten_options(
    prefix: &str,
    entries: &[ServerSetting],
    output: &mut BTreeMap<String, ServerValue>,
) -> Result<(), MiddlewarePlanError> {
    for entry in entries {
        let key = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}.{}", entry.name)
        };
        match &entry.body {
            ServerSettingBody::Flag => insert_option(output, key, ServerValue::Bool(true))?,
            ServerSettingBody::Value(value) => insert_option(output, key, value.clone())?,
            ServerSettingBody::Block(children) => flatten_options(&key, children, output)?,
        }
    }
    Ok(())
}

fn insert_option(
    output: &mut BTreeMap<String, ServerValue>,
    key: String,
    value: ServerValue,
) -> Result<(), MiddlewarePlanError> {
    if output.insert(key.clone(), value).is_some() {
        return Err(MiddlewarePlanError(format!(
            "duplicate middleware option `{key}`"
        )));
    }
    Ok(())
}

fn canonical_name(name: &str) -> Option<&'static str> {
    KNOWN_MIDDLEWARE
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(name))
}

fn validate_step(
    name: &str,
    options: &BTreeMap<String, ServerValue>,
) -> Result<(), MiddlewarePlanError> {
    match name {
        "json" | "text" | "form" | "multipart" | "rawBody" => {
            if let Some(limit) = options.get("limit") {
                require_size_quantity(name, "limit", limit)?;
            }
        }
        "compression" => {
            if let Some(threshold) = options.get("threshold") {
                require_size_quantity(name, "threshold", threshold)?;
            }
            if let Some(ServerValue::Array(algorithms)) = options.get("algorithms") {
                for algorithm in algorithms {
                    let valid = matches!(
                        algorithm,
                        ServerValue::Ident(value) | ServerValue::String(value)
                            if matches!(value.as_str(), "br" | "gzip" | "zstd" | "deflate")
                    );
                    if !valid {
                        return Err(MiddlewarePlanError(
                            "compression.algorithms supports br, gzip, zstd, or deflate".into(),
                        ));
                    }
                }
            }
        }
        "cors" => {
            if let Some(value) = options.get("credentials") {
                require_bool(name, "credentials", value)?;
            }
            if let Some(value) = options.get("enabled") {
                require_bool(name, "enabled", value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn require_bool(
    middleware: &str,
    option: &str,
    value: &ServerValue,
) -> Result<(), MiddlewarePlanError> {
    if matches!(value, ServerValue::Bool(_)) {
        Ok(())
    } else {
        Err(MiddlewarePlanError(format!(
            "{middleware}.{option} must be boolean"
        )))
    }
}

fn require_size_quantity(
    middleware: &str,
    option: &str,
    value: &ServerValue,
) -> Result<(), MiddlewarePlanError> {
    match value {
        ServerValue::Quantity { value, unit }
            if value.is_finite()
                && *value > 0.0
                && matches!(unit.to_ascii_lowercase().as_str(), "b" | "kb" | "mb" | "gb") =>
        {
            Ok(())
        }
        ServerValue::Number(value) if value.is_finite() && *value > 0.0 => Ok(()),
        _ => Err(MiddlewarePlanError(format!(
            "{middleware}.{option} must be a positive byte size such as 1mb"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_rel::compile_server_source;

    #[test]
    fn preserves_declared_order_and_lowers_options() {
        let program = compile_server_source(
            r#"server Main {
                middleware {
                    correlationId;
                    json { limit 2mb; strict true; }
                    compression { algorithms [br, gzip]; threshold 1kb; }
                    errorHandler;
                }
            }"#,
        )
        .unwrap();
        let plan = MiddlewarePlan::lower(&program).unwrap();
        assert_eq!(
            plan.steps
                .iter()
                .map(|step| step.name.as_str())
                .collect::<Vec<_>>(),
            vec!["correlationId", "json", "compression", "errorHandler"]
        );
        assert!(plan.steps[1].options.contains_key("limit"));
    }

    #[test]
    fn rejects_unknown_or_misordered_error_handler() {
        let unknown =
            compile_server_source("server Main { middleware { totallyNotMiddleware; } }").unwrap();
        assert!(MiddlewarePlan::lower(&unknown).is_err());

        let order =
            compile_server_source("server Main { middleware { errorHandler; correlationId; } }")
                .unwrap();
        assert!(MiddlewarePlan::lower(&order).is_err());
    }
}
