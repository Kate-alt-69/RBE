//! Async language bridge from `.module` execution into core Video Manager.

use core_lib::{AppState, VideoLanguage};

use crate::ast::Value;
use crate::module_eval::{HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError};

pub struct VideoHostCapabilities {
    video: VideoLanguage,
}

impl VideoHostCapabilities {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
        }
    }
}

impl HostCapabilityCaller for VideoHostCapabilities {
    fn call<'a>(
        &'a self,
        scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            if module != "video" {
                return Ok(None);
            }
            let owner = scope.ok_or_else(|| ModuleEvalError {
                code: "VID3003",
                message: "video capability requires a resolved .module identity".into(),
            })?;
            let args = args.into_iter().map(value_to_json).collect::<Vec<_>>();
            let value =
                self.video
                    .call(&owner, function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: error.code,
                        message: error.message,
                    })?;
            Ok(Some(value_from_json(value)?))
        })
    }
}

fn value_to_json(value: Value) -> serde_json::Value {
    match value {
        Value::String(value) => serde_json::Value::String(value),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(value) => serde_json::Value::Bool(value),
        Value::Null => serde_json::Value::Null,
        Value::Object(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, value_to_json(value)))
                .collect(),
        ),
        Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
    }
}

fn value_from_json(value: serde_json::Value) -> Result<Value, ModuleEvalError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Bool(value)),
        serde_json::Value::Number(value) => {
            value
                .as_f64()
                .map(Value::Number)
                .ok_or_else(|| ModuleEvalError {
                    code: "VID3002",
                    message: "Video Manager returned a number outside the RBE numeric range".into(),
                })
        }
        serde_json::Value::String(value) => Ok(Value::String(value)),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(value_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_json::Value::Object(fields) => {
            let mut out = std::collections::HashMap::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key, value_from_json(value)?);
            }
            Ok(Value::Object(out))
        }
    }
}
