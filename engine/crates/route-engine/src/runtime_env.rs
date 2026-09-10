//! Typed public Runtime ENV for REL.
//!
//! Runtime ENV is backend application configuration, not the operating-system
//! process environment. Values stay JSON typed and are resolved once while a
//! Runtime Image is being linked.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::ast::Value;
use crate::server_rel::{ServerProgram, ServerSettingBody, ServerValue};
use crate::source_registry::RelSourceKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEnvOrigin {
    BuiltIn,
    ServerDefault,
    Settings,
    ForcedServer,
    ImageSnapshot,
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
    InvalidCall(String),
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
            Self::InvalidServerEntry(message) | Self::InvalidCall(message) => {
                formatter.write_str(message)
            }
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

    pub fn from_snapshot(snapshot: JsonValue) -> Result<Self, RuntimeEnvError> {
        let JsonValue::Object(values) = snapshot else {
            return Err(RuntimeEnvError::InvalidServerEntry(
                "Runtime ENV image snapshot must be a JSON object".into(),
            ));
        };
        let values = values.into_iter().collect::<BTreeMap<_, _>>();
        let origins = values
            .keys()
            .map(|name| (name.clone(), RuntimeEnvOrigin::ImageSnapshot))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            values: Arc::new(values),
            origins: Arc::new(origins),
        })
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

    pub(crate) fn call_rel(
        &self,
        function: &str,
        args: &[Value],
    ) -> Result<Value, RuntimeEnvError> {
        let name = match args {
            [Value::String(name)] => name.as_str(),
            _ => {
                return Err(RuntimeEnvError::InvalidCall(format!(
                    "ENV.{function}() requires exactly one string key"
                )))
            }
        };
        match function {
            "has" => Ok(Value::Bool(self.has(name))),
            "get" => Ok(self.get(name).map(json_to_rel).unwrap_or(Value::Null)),
            "require" => self.require(name).map(json_to_rel),
            "string" => self
                .string(name)
                .map(|value| Value::String(value.to_string())),
            "number" => self.number(name).map(Value::Number),
            "bool" => self.bool(name).map(Value::Bool),
            "object" => self.object(name).map(|value| {
                Value::Object(
                    value
                        .iter()
                        .map(|(key, value)| (key.clone(), json_to_rel(value)))
                        .collect(),
                )
            }),
            "array" => self
                .array(name)
                .map(|value| Value::Array(value.iter().map(json_to_rel).collect())),
            other => Err(RuntimeEnvError::InvalidCall(format!(
                "ENV.{other}() does not exist"
            ))),
        }
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
        value
            .as_str()
            .ok_or_else(|| wrong_type(name, "string", value))
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

fn json_to_rel(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Bool(*value),
        JsonValue::Number(value) => Value::Number(value.as_f64().unwrap_or(0.0)),
        JsonValue::String(value) => Value::String(value.clone()),
        JsonValue::Array(values) => Value::Array(values.iter().map(json_to_rel).collect()),
        JsonValue::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json_to_rel(value)))
                .collect(),
        ),
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
    fn rel_call_surface_preserves_json_types() {
        let server = compile_server_source("server Main {}").unwrap();
        let settings = BTreeMap::from([
            ("NAME".to_string(), JsonValue::String("rbe".into())),
            ("COUNT".to_string(), JsonValue::from(7)),
            ("FLAGS".to_string(), serde_json::json!([true, false])),
        ]);
        let env = RuntimeEnv::resolve(&BTreeMap::new(), &server, &settings).unwrap();
        assert!(matches!(
            env.call_rel("string", &[Value::String("NAME".into())]).unwrap(),
            Value::String(value) if value == "rbe"
        ));
        assert!(matches!(
            env.call_rel("number", &[Value::String("COUNT".into())]).unwrap(),
            Value::Number(value) if value == 7.0
        ));
        assert!(matches!(
            env.call_rel("array", &[Value::String("FLAGS".into())]).unwrap(),
            Value::Array(values) if values.len() == 2
        ));
        assert!(matches!(
            env.call_rel("get", &[Value::String("MISSING".into())])
                .unwrap(),
            Value::Null
        ));
        assert!(env
            .call_rel("require", &[Value::String("MISSING".into())])
            .is_err());
    }

    #[test]
    fn image_snapshot_round_trip_keeps_runtime_env_types() {
        let env = RuntimeEnv::from_snapshot(serde_json::json!({
            "NAME": "rbe",
            "COUNT": 7,
            "FLAGS": [true, false]
        }))
        .unwrap();
        assert_eq!(env.string("NAME").unwrap(), "rbe");
        assert_eq!(env.number("COUNT").unwrap(), 7.0);
        assert_eq!(env.array("FLAGS").unwrap().len(), 2);
        assert_eq!(env.origin("NAME"), Some(RuntimeEnvOrigin::ImageSnapshot));
    }

    #[test]
    fn routes_do_not_receive_runtime_env_by_default() {
        assert!(!RuntimeEnv::can_read(RelSourceKind::Route));
        assert!(RuntimeEnv::can_read(RelSourceKind::Module));
        assert!(RuntimeEnv::can_read(RelSourceKind::Service));
        assert!(RuntimeEnv::can_read(RelSourceKind::Server));
    }
}
