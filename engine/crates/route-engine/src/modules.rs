//! Curated capability/import registry for `.route` files.
//! Direct imports are resolved to a single callable binding; namespace
//! imports keep the `module.function()` form for compatibility.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::ast::{ImportTarget, Value};

#[derive(Debug)]
pub struct ModuleError {
    pub message: String,
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

enum ModuleKind {
    Builtin(BuiltinModule),
    CustomUnimplemented {
        source_path: String,
        resolved_path: std::path::PathBuf,
    },
}

#[derive(Clone, Copy)]
enum BuiltinModule {
    Net,
    Env,
    Private,
    Json,
    Time,
    Log,
    Crypto,
    Http,
    Request,
    Security,
    Response,
    VideoManager,
}

pub struct ModuleRegistry {
    modules: HashMap<String, ModuleKind>,
    direct_functions: HashMap<String, (String, String)>,
}

/// Capabilities that are permitted to be imported by a `.route` file.
///
/// `private` is deliberately restricted to server-owned, read-only runtime
/// health information. It is not a general host/runtime escape hatch.
pub fn route_capability_allowed(name: &str) -> bool {
    matches!(
        name,
        "net"
            | "json"
            | "crypto"
            | "time"
            | "http"
            | "request"
            | "log"
            | "security"
            | "response"
            | "private"
    )
}

/// Return whether a known built-in module exports a particular function.
/// This is used by semantic analysis so unknown built-in calls fail during
/// boot instead of becoming request-time 500s.
pub fn builtin_function_exists(module: &str, function: &str) -> bool {
    match module {
        "net" => matches!(function, "ping"),
        "private" => matches!(function, "health"),
        "json" => matches!(function, "parse" | "stringify"),
        "time" => matches!(function, "now"),
        "log" => matches!(function, "info" | "warn"),
        "crypto" => matches!(function, "hash"),
        "env" => matches!(function, "get"),
        "vm" | "video-manager" => matches!(
            function,
            "status"
                | "databaseHealth"
                | "database_health"
                | "get"
                | "create"
                | "queueDownload"
                | "queue_download"
        ),
        _ => false,
    }
}

pub fn binding_name(target: &ImportTarget) -> String {
    match target {
        ImportTarget::Aliased { alias, .. } => alias.clone(),
        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::BuiltinFunction { function, .. } => function.clone(),
        ImportTarget::Custom(path) => std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string(),
        ImportTarget::CustomFunction { function, .. } => function.clone(),
        ImportTarget::Service(name) => name.clone(),
        ImportTarget::ServiceFunction { function, .. } => function.clone(),
    }
}

fn base_target(target: &ImportTarget) -> &ImportTarget {
    match target {
        ImportTarget::Aliased { target, .. } => target.as_ref(),
        _ => target,
    }
}

impl ModuleRegistry {
    pub fn from_imports(imports: &[ImportTarget]) -> Self {
        let mut modules = HashMap::new();
        let mut direct_functions = HashMap::new();

        for target in imports {
            match base_target(target) {
                ImportTarget::Builtin(name) => {
                    let kind = match name.as_str() {
                        "net" => ModuleKind::Builtin(BuiltinModule::Net),
                        "env" => ModuleKind::Builtin(BuiltinModule::Env),
                        "private" => ModuleKind::Builtin(BuiltinModule::Private),
                        "json" => ModuleKind::Builtin(BuiltinModule::Json),
                        "time" => ModuleKind::Builtin(BuiltinModule::Time),
                        "log" => ModuleKind::Builtin(BuiltinModule::Log),
                        "crypto" => ModuleKind::Builtin(BuiltinModule::Crypto),
                        "http" => ModuleKind::Builtin(BuiltinModule::Http),
                        "request" => ModuleKind::Builtin(BuiltinModule::Request),
                        "security" => ModuleKind::Builtin(BuiltinModule::Security),
                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
                        _ => ModuleKind::CustomUnimplemented {
                            source_path: format!("builtin:{name}"),
                            resolved_path: std::path::PathBuf::new(),
                        },
                    };
                    modules.insert(binding_name(target), kind);
                }
                ImportTarget::BuiltinFunction { module, function } => {
                    let kind = match module.as_str() {
                        "net" => ModuleKind::Builtin(BuiltinModule::Net),
                        "env" => ModuleKind::Builtin(BuiltinModule::Env),
                        "private" => ModuleKind::Builtin(BuiltinModule::Private),
                        "json" => ModuleKind::Builtin(BuiltinModule::Json),
                        "time" => ModuleKind::Builtin(BuiltinModule::Time),
                        "log" => ModuleKind::Builtin(BuiltinModule::Log),
                        "crypto" => ModuleKind::Builtin(BuiltinModule::Crypto),
                        "http" => ModuleKind::Builtin(BuiltinModule::Http),
                        "request" => ModuleKind::Builtin(BuiltinModule::Request),
                        "security" => ModuleKind::Builtin(BuiltinModule::Security),
                        "response" => ModuleKind::Builtin(BuiltinModule::Response),
                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),
                        _ => ModuleKind::CustomUnimplemented {
                            source_path: format!("builtin:{module}"),
                            resolved_path: std::path::PathBuf::new(),
                        },
                    };
                    modules.entry(module.clone()).or_insert(kind);
                    direct_functions
                        .insert(binding_name(target), (module.clone(), function.clone()));
                }
                ImportTarget::Custom(path) => {
                    let resolved =
                        crate::paths::resolve_custom_import(&crate::paths::binary_dir(), path);
                    let name = binding_name(target);
                    modules.insert(
                        name,
                        ModuleKind::CustomUnimplemented {
                            source_path: path.clone(),
                            resolved_path: resolved,
                        },
                    );
                }
                ImportTarget::CustomFunction { path, function } => {
                    let resolved =
                        crate::paths::resolve_custom_import(&crate::paths::binary_dir(), path);
                    let module_name = std::path::Path::new(path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(path)
                        .to_string();
                    modules
                        .entry(module_name.clone())
                        .or_insert(ModuleKind::CustomUnimplemented {
                            source_path: path.clone(),
                            resolved_path: resolved,
                        });
                    direct_functions.insert(binding_name(target), (module_name, function.clone()));
                }
                ImportTarget::Service(_) | ImportTarget::ServiceFunction { .. } => {
                    // Service calls are async and are intentionally handled by ModuleExecutor,
                    // never by the synchronous route capability registry.
                }
                ImportTarget::Aliased { .. } => {
                    unreachable!("base_target removes aliased import wrappers")
                }
            }
        }

        Self {
            modules,
            direct_functions,
        }
    }

    pub fn call_direct(&self, binding: &str, args: &[Value]) -> Result<Value, ModuleError> {
        let Some((module, function)) = self.direct_functions.get(binding) else {
            return Err(ModuleError {
                message: format!("{binding} was not imported as a direct function"),
            });
        };
        self.call(module, function, args)
    }

    pub fn is_direct_function(&self, binding: &str) -> bool {
        self.direct_functions.contains_key(binding)
    }

    pub fn call(
        &self,
        module_name: &str,
        function_name: &str,
        args: &[Value],
    ) -> Result<Value, ModuleError> {
        let Some(kind) = self.modules.get(module_name) else {
            return Err(ModuleError {
                message: format!("{module_name}.{function_name} does not exist — please remove it from the route"),
            });
        };

        match kind {
            ModuleKind::Builtin(BuiltinModule::Net) => call_net(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Env) => call_env(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Private) => call_private(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Json) => call_json(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Time) => call_time(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Log) => call_log(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Crypto) => call_crypto(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Http) => Err(ModuleError {
                message: format!("{module_name}.{function_name}() is not implemented yet"),
            }),
            ModuleKind::Builtin(BuiltinModule::Request) => Err(ModuleError {
                message: format!("{module_name}.{function_name}() is not implemented yet"),
            }),
            ModuleKind::Builtin(BuiltinModule::Security) => Err(ModuleError {
                message: format!("{module_name}.{function_name}() is not implemented yet"),
            }),
            ModuleKind::Builtin(BuiltinModule::Response) => Err(ModuleError {
                message: format!("{module_name}.{function_name}() is not implemented yet"),
            }),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {
                message: format!(
            "{module_name}.{function_name}() requires the privileged module Video Manager host capability"
        ),
            }),
            ModuleKind::CustomUnimplemented {
                source_path,
                resolved_path,
            } => {
                let note = if resolved_path.as_os_str().is_empty() {
                    String::new()
                } else {
                    format!(" (would resolve to {})", resolved_path.display())
                };
                Err(ModuleError {
                    message: format!(
                        "{module_name}.{function_name}(...) — \"{source_path}\"{note} is not implemented yet; this .route file parses, but the call cannot run until the module lands"
                    ),
                })
            }
        }
    }
}

fn runtime_start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

fn call_net(function_name: &str, _args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "ping" => {
            let mut fields = HashMap::new();
            fields.insert("ok".to_string(), Value::Bool(true));
            Ok(Value::Object(fields))
        }
        other => Err(ModuleError {
            message: format!("net.{other}() does not exist"),
        }),
    }
}

fn call_private(function_name: &str, _args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "health" => {
            let mut fields = HashMap::new();
            fields.insert("status".to_string(), Value::String("healthy".to_string()));
            fields.insert(
                "uptime".to_string(),
                Value::Number(runtime_start().elapsed().as_secs_f64()),
            );
            fields.insert("container".to_string(), Value::Null);
            fields.insert("vault".to_string(), Value::Bool(true));
            fields.insert("errorReporter".to_string(), Value::Null);
            Ok(Value::Object(fields))
        }
        other => Err(ModuleError {
            message: format!("private.{other}() does not exist"),
        }),
    }
}

fn call_json(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "parse" => {
            let Some(Value::String(input)) = args.first() else {
                return Err(ModuleError {
                    message: "json.parse(value) requires a string argument".into(),
                });
            };
            let parsed: serde_json::Value =
                serde_json::from_str(input).map_err(|e| ModuleError {
                    message: format!("json.parse() failed: {e}"),
                })?;
            Ok(json_to_value(parsed))
        }
        "stringify" => {
            let Some(value) = args.first() else {
                return Err(ModuleError {
                    message: "json.stringify(value) requires an argument".into(),
                });
            };
            let json = value_to_json(value);
            let text = serde_json::to_string(&json).map_err(|e| ModuleError {
                message: format!("json.stringify() failed: {e}"),
            })?;
            Ok(Value::String(text))
        }
        other => Err(ModuleError {
            message: format!("json.{other}() does not exist"),
        }),
    }
}

fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(v) => Value::Bool(v),
        serde_json::Value::Number(v) => Value::Number(v.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(v) => Value::String(v),
        serde_json::Value::Array(items) => {
            Value::Array(items.into_iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
    }
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(v) => serde_json::Value::String(v.clone()),
        Value::Number(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(v) => serde_json::Value::Bool(*v),
        Value::Null => serde_json::Value::Null,
        Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
    }
}

fn call_time(function_name: &str, _args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "now" => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            Ok(Value::Number(seconds))
        }
        other => Err(ModuleError {
            message: format!("time.{other}() does not exist"),
        }),
    }
}

fn call_log(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let message = args
        .first()
        .map(|value| format!("{value:?}"))
        .unwrap_or_default();
    match function_name {
        "info" => {
            tracing::info!(message = %message, "route log");
            Ok(Value::Null)
        }
        "warn" => {
            tracing::warn!(message = %message, "route log");
            Ok(Value::Null)
        }
        other => Err(ModuleError {
            message: format!("log.{other}() does not exist"),
        }),
    }
}

fn call_crypto(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "hash" => {
            let Some(Value::String(input)) = args.first() else {
                return Err(ModuleError {
                    message: "crypto.hash(value) requires a string argument".into(),
                });
            };
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            input.hash(&mut hasher);
            Ok(Value::String(format!("{:016x}", hasher.finish())))
        }
        other => Err(ModuleError {
            message: format!("crypto.{other}() does not exist"),
        }),
    }
}

fn call_env(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "get" => {
            let Some(Value::String(key)) = args.first() else {
                return Err(ModuleError {
                    message: "env.get(key) requires a string argument".into(),
                });
            };
            Ok(match std::env::var(key) {
                Ok(v) => Value::String(v),
                Err(_) => Value::Null,
            })
        }
        other => Err(ModuleError {
            message: format!("env.{other}() does not exist"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_capability_whitelist_matches_the_language_boundary() {
        assert!(route_capability_allowed("net"));
        assert!(route_capability_allowed("json"));
        assert!(route_capability_allowed("crypto"));
        assert!(route_capability_allowed("time"));
        assert!(route_capability_allowed("http"));
        assert!(route_capability_allowed("request"));
        assert!(route_capability_allowed("log"));
        assert!(route_capability_allowed("security"));
        assert!(route_capability_allowed("response"));
        assert!(route_capability_allowed("private"));
    }

    #[test]
    fn privileged_capabilities_are_not_route_capabilities() {
        assert!(!route_capability_allowed("env"));
        assert!(!route_capability_allowed("encoding"));
        assert!(!route_capability_allowed("auth"));
        assert!(!route_capability_allowed("vault"));
        assert!(!route_capability_allowed("storage"));
        assert!(!route_capability_allowed("cache"));
    }

    #[test]
    fn video_manager_uses_only_explicit_supported_import_names() {
        assert!(builtin_function_exists("vm", "status"));
        assert!(builtin_function_exists("video-manager", "queueDownload"));
        assert!(!builtin_function_exists("video", "status"));
        assert!(!route_capability_allowed("vm"));
        assert!(!route_capability_allowed("video-manager"));
        assert!(!route_capability_allowed("video"));
    }

    #[test]
    fn private_health_reports_runtime_status() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("private".into())]);
        let Value::Object(fields) = registry
            .call("private", "health", &[])
            .expect("health call")
        else {
            panic!("expected health object");
        };
        assert!(matches!(fields.get("status"), Some(Value::String(status)) if status == "healthy"));
        assert!(matches!(fields.get("uptime"), Some(Value::Number(uptime)) if *uptime >= 0.0));
        assert!(matches!(fields.get("container"), Some(Value::Null)));
        assert!(matches!(fields.get("vault"), Some(Value::Bool(true))));
    }

    #[test]
    fn json_parse_and_stringify_round_trip() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("json".into())]);
        let parsed = registry
            .call(
                "json",
                "parse",
                &[Value::String(r#"{"ok":true,"count":2}"#.into())],
            )
            .expect("parse");
        let Value::Object(fields) = &parsed else {
            panic!("expected object")
        };
        assert!(matches!(fields.get("ok"), Some(Value::Bool(true))));
        let Value::String(text) = registry
            .call("json", "stringify", &[parsed])
            .expect("stringify")
        else {
            panic!("expected string")
        };
        assert!(text.contains("\"ok\":true"));
    }

    #[test]
    fn time_now_returns_epoch_seconds() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("time".into())]);
        let Value::Number(now) = registry.call("time", "now", &[]).expect("time.now") else {
            panic!("expected number")
        };
        assert!(now > 0.0);
    }

    #[test]
    fn unknown_private_function_is_reported() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("private".into())]);
        let error = registry
            .call("private", "missing", &[])
            .expect_err("missing function should fail");
        assert!(error.message.contains("private.missing() does not exist"));
    }
}
