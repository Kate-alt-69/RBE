//! Typed public Runtime ENV for REL.
//!
//! Runtime ENV is backend application configuration, not the operating-system
//! process environment. Values stay JSON typed and are resolved once while a
//! Runtime Image is being linked.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::server_rel::{ServerProgram, ServerSettingBody, ServerValue};
use crate::source_registry::RelSourceKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEnvOrigin {
    BuiltIn,
    ServerDefault,
    Settings,
    ForcedServer,
}

#[derive(Debug, Clone)]
pub struct RuntimeEnv {
    values: Arc<BTreeMap<String, JsonValue>>,
    origins: Arc<BTreeMap<String, RuntimeEnvOrigin>>,
}

impl Default for RuntimeEnv {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEnvError {
    Missing(String),
    WrongType {
        name: String,
        expected: &'static str,
        actual: &'static str,
    },
    InvalidServerEntry(String),
}

impl fmt::Display for RuntimeEnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(formatter, "Runtime ENV value `{name}` is not defined"),
            Self::WrongType {
                name,
                expected,
                actual,
            } => write!(
                formatter,
                "Runtime ENV value `{name}` is {actual}, expected {expected}"
            ),
            Self::InvalidServerEntry(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RuntimeEnvError {}

impl RuntimeEnv {
    pub fn empty() -> Self {
        Self {
            values: Arc::new(BTreeMap::new()),
            origins: Arc::new(BTreeMap::new()),
        }
    }

    /// Resolves Runtime ENV with the contract:
    ///
    /// built-ins < normal Server REL defaults < settings.json runtimeEnv
    /// < forced Server REL values.
    pub fn resolve(
        built_ins: &BTreeMap<String, JsonValue>,
        server: &ServerProgram,
        settings: &BTreeMap<String, JsonValue>,
    ) -> Result<Self, RuntimeEnvError> {
        let mut values = BTreeMap::new();
        let mut origins = BTreeMap::new();

        merge_layer(
            &mut values,
            &mut origins,
            built_ins,
            RuntimeEnvOrigin::BuiltIn,
        );

        let mut defaults = BTreeMap::new();
        let mut forced = BTreeMap::new();
        if let Some(env) = server.setting("env") {
            let ServerSettingBody::Block(entries) = &env.body else {
                return Err(RuntimeEnvError::InvalidServerEntry(
                    "Server REL `env` must be a block before Runtime ENV resolution".into(),
                ));
            };
            for entry in entries {
                let ServerSettingBody::Value(value) = &entry.body else {
                    return Err(RuntimeEnvError::InvalidServerEntry(format!(
                        "Server REL ENV `{}` must have a scalar/array value",
                        entry.name
                    )));
                };
                let value = server_value_to_json(value)?;
                if entry.forced || env.forced {
                    forced.insert(entry.name.clone(), value);
                } else {
                    defaults.insert(entry.name.clone(), value);
                }
            }
        }

        merge_layer(
            &mut values,
            &mut origins,
            &defaults,
            RuntimeEnvOrigin::ServerDefault,
        );
        merge_layer(
            &mut values,
            &mut origins,
            settings,
            RuntimeEnvOrigin::Settings,
        );
        merge_layer(
            &mut values,
            &mut origins,
            &forced,
            RuntimeEnvOrigin::ForcedServer,
        );

        Ok(Self {
            values: Arc::new(values),
            origins: Arc::new(origins),
        })
    }

    pub fn can_read(kind: RelSourceKind) -> bool {
        matches!(
            kind,
            RelSourceKind::Module | RelSourceKind::Service | RelSourceKind::Server
        )
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&JsonValue> {
        self.values.get(name)
    }

    pub fn require(&self, name: &str) -> Result<&JsonValue, RuntimeEnvError> {
        self.get(name)
            .ok_or_else(|| RuntimeEnvError::Missing(name.to_string()))
    }

    pub fn string(&self, name: &str) -> Result<&str, RuntimeEnvError> {
        let value = self.require(name)?;
        value.as_str().ok_or_else(|| wrong_type(name, "string", value))
    }

    pub fn number(&self, name: &str) -> Result<f64, RuntimeEnvError> {
        let value = self.require(name)?;
        value
            .as_f64()
            .ok_or_else(|| wrong_type(name, "number", value))
    }

    pub fn bool(&self, name: &str) -> Result<bool, RuntimeEnvError> {
        let value = self.require(name)?;
        value
            .as_bool()
            .ok_or_else(|| wrong_type(name, "boolean", value))
    }

    pub fn object(&self, name: &str) -> Result<&JsonMap<String, JsonValue>, RuntimeEnvError> {
        let value = self.require(name)?;
        value
            .as_object()
            .ok_or_else(|| wrong_type(name, "object", value))
    }

    pub fn array(&self, name: &str) -> Result<&[JsonValue], RuntimeEnvError> {
        let value = self.require(name)?;
        value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| wrong_type(name, "array", value))
    }

    pub fn origin(&self, name: &str) -> Option<RuntimeEnvOrigin> {
        self.origins.get(name).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &JsonValue)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    pub fn to_json(&self) -> JsonValue {
        JsonValue::Object(
            self.values
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        )
    }
}

fn merge_layer(
    values: &mut BTreeMap<String, JsonValue>,
    origins: &mut BTreeMap<String, RuntimeEnvOrigin>,
    layer: &BTreeMap<String, JsonValue>,
    origin: RuntimeEnvOrigin,
) {
    for (name, value) in layer {
        values.insert(name.clone(), value.clone());
        origins.insert(name.clone(), origin);
    }
}

pub(crate) fn server_value_to_json(value: &ServerValue) -> Result<JsonValue, RuntimeEnvError> {
    match value {
        ServerValue::String(value) | ServerValue::Ident(value) => {
            Ok(JsonValue::String(value.clone()))
        }
        ServerValue::Number(value) => serde_json::Number::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                RuntimeEnvError::InvalidServerEntry(
                    "Server REL Runtime ENV numbers must be finite".into(),
                )
            }),
        ServerValue::Quantity { value, unit } => Ok(JsonValue::String(format!("{value}{unit}"))),
        ServerValue::Bool(value) => Ok(JsonValue::Bool(*value)),
        ServerValue::Null => Ok(JsonValue::Null),
        ServerValue::Array(values) => values
            .iter()
            .map(server_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
    }
}

fn wrong_type(name: &str, expected: &'static str, value: &JsonValue) -> RuntimeEnvError {
    RuntimeEnvError::WrongType {
        name: name.to_string(),
        expected,
        actual: json_type(value),
    }
}

fn json_type(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_rel::compile_server_source;

    #[test]
    fn resolves_documented_precedence_without_stringifying_types() {
        let server = compile_server_source(
            r#"server Main {
                env {
                    APP_NAME "server";
                    COUNT 2;
                    force LOCKED true;
                }
            }"#,
        )
        .unwrap();
        let built_ins = BTreeMap::from([
            ("APP_NAME".to_string(), JsonValue::String("builtin".into())),
            ("BUILTIN".to_string(), JsonValue::Bool(true)),
        ]);
        let settings = BTreeMap::from([
            ("APP_NAME".to_string(), JsonValue::String("settings".into())),
            ("COUNT".to_string(), JsonValue::from(7)),
            ("LOCKED".to_string(), JsonValue::Bool(false)),
        ]);

        let env = RuntimeEnv::resolve(&built_ins, &server, &settings).unwrap();
        assert_eq!(env.string("APP_NAME").unwrap(), "settings");
        assert_eq!(env.number("COUNT").unwrap(), 7.0);
        assert!(env.bool("LOCKED").unwrap());
        assert_eq!(env.origin("LOCKED"), Some(RuntimeEnvOrigin::ForcedServer));
        assert_eq!(env.origin("APP_NAME"), Some(RuntimeEnvOrigin::Settings));
    }

    #[test]
    fn routes_do_not_receive_runtime_env_by_default() {
        assert!(!RuntimeEnv::can_read(RelSourceKind::Route));
        assert!(RuntimeEnv::can_read(RelSourceKind::Module));
        assert!(RuntimeEnv::can_read(RelSourceKind::Service));
        assert!(RuntimeEnv::can_read(RelSourceKind::Server));
    }
}