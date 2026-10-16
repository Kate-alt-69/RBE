//! Async language bridge from `.module` execution into runtime-owned capabilities.

use std::sync::Arc;

use core_lib::{call_public_http, AppState, VideoLanguage};

use crate::ast::Value;
use crate::field_manager::FieldRuntimeContext;
use crate::module_eval::{HostCapabilityCaller, HostCapabilityFuture, ModuleEvalError};
use crate::rel_host_builtins::is_host_builtin;
use crate::rel_host_descriptor::{deferred_call, temp_request};
use crate::rel_host_runtime::{installed_rel_host_executor, RelHostExecutor};
use crate::runtime_image::RuntimeImage;

pub struct RuntimeHostCapabilities {
    video: VideoLanguage,
    image: Arc<RuntimeImage>,
    fields: Option<Arc<FieldRuntimeContext>>,
    rel_host_executor: Option<Arc<dyn RelHostExecutor>>,
}

impl RuntimeHostCapabilities {
    pub fn from_state_and_image(state: &AppState, image: Arc<RuntimeImage>) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
            fields: None,
            rel_host_executor: installed_rel_host_executor(),
        }
    }

    pub fn from_state_image_and_fields(
        state: &AppState,
        image: Arc<RuntimeImage>,
        fields: Arc<FieldRuntimeContext>,
    ) -> Self {
        Self {
            video: VideoLanguage::new(state.video_manager.clone()),
            image,
            fields: Some(fields),
            rel_host_executor: installed_rel_host_executor(),
        }
    }

    /// Attach a trusted Backend/Container executor explicitly. The normal
    /// Backend path installs one process-wide executor once at boot; this
    /// override remains useful for isolated tests and embedded Route Engine use.
    pub fn with_rel_host_executor(mut self, executor: Arc<dyn RelHostExecutor>) -> Self {
        self.rel_host_executor = Some(executor);
        self
    }
}

impl HostCapabilityCaller for RuntimeHostCapabilities {
    fn call<'a>(
        &'a self,
        scope: Option<String>,
        module: &'a str,
        function: &'a str,
        args: Vec<Value>,
    ) -> HostCapabilityFuture<'a> {
        Box::pin(async move {
            if is_host_builtin(module) {
                if module == "workspace" && function == "temp" {
                    let request = temp_request(&args)?;
                    let executor = self.rel_host_executor.as_ref().ok_or_else(|| ModuleEvalError {
                        code: "REL2201",
                        message: "workspace.temp() requires the trusted Backend/Container host executor; no unsafe PATH/process fallback is permitted".into(),
                    })?;
                    return executor.execute(request).await.map(Some);
                }
                if let Some(value) = deferred_call(module, function, args)? {
                    return Ok(Some(value));
                }
                return Err(ModuleEvalError {
                    code: "REL2200",
                    message: format!(
                        "{module}.{function}() is not yet materializable through the host capability bridge"
                    ),
                });
            }
            if module == "field" {
                let fields = self.fields.as_ref().ok_or_else(|| ModuleEvalError {
                    code: "FLD4000",
                    message: "FieldManager context is unavailable for this request".into(),
                })?;
                return fields.call(function, &args).map(Some);
            }
            if module == "ENV" {
                let value = self
                    .image
                    .environment
                    .call_rel(function, &args)
                    .map_err(|error| ModuleEvalError {
                        code: "ENV3000",
                        message: error.to_string(),
                    })?;
                return Ok(Some(value));
            }
            if module == "http" {
                let args = args.into_iter().map(value_to_json).collect::<Vec<_>>();
                let value =
                    call_public_http(function, &args)
                        .await
                        .map_err(|error| ModuleEvalError {
                            code: error.code,
                            message: error.message,
                        })?;
                return value_from_json(value).map(Some);
            }
            if !matches!(module, "vm" | "video-manager") {
                return Ok(None);
            }
            let owner = scope.ok_or_else(|| ModuleEvalError {
                code: "VID3003",
                message: "Video Manager capability requires a resolved .module identity".into(),
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
