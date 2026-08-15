//! Curated capability/import registry for `.route` files.
//! Direct imports are resolved to a single callable binding; namespace
//! imports keep the older `net.ping()` form for compatibility.

use std::collections::HashMap;
use std::fmt;

use crate::ast::{ImportTarget, Value};

#[derive(Debug)]
pub struct ModuleError { pub message: String }

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.message) }
}

enum ModuleKind {
    Builtin(BuiltinModule),
    CustomUnimplemented { source_path: String, resolved_path: std::path::PathBuf },
}

#[derive(Clone, Copy)]
enum BuiltinModule { Net, Env }

pub struct ModuleRegistry {
    modules: HashMap<String, ModuleKind>,
    direct_functions: HashMap<String, (String, String)>,
}

pub fn binding_name(target: &ImportTarget) -> String {
    match target {
        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::BuiltinFunction { function, .. } => function.clone(),
        ImportTarget::Custom(path) => std::path::Path::new(path)
            .file_stem().and_then(|s| s.to_str()).unwrap_or(path).to_string(),
        ImportTarget::CustomFunction { function, .. } => function.clone(),
    }
}

impl ModuleRegistry {
    pub fn from_imports(imports: &[ImportTarget]) -> Self {
        let mut modules = HashMap::new();
        let mut direct_functions = HashMap::new();

        for target in imports {
            match target {
                ImportTarget::Builtin(name) => {
                    let kind = match name.as_str() {
                        "net" => ModuleKind::Builtin(BuiltinModule::Net),
                        "env" => ModuleKind::Builtin(BuiltinModule::Env),
                        _ => ModuleKind::CustomUnimplemented {
                            source_path: format!("builtin:{name}"),
                            resolved_path: std::path::PathBuf::new(),
                        },
                    };
                    modules.insert(name.clone(), kind);
                }
                ImportTarget::BuiltinFunction { module, function } => {
                    let kind = match module.as_str() {
                        "net" => ModuleKind::Builtin(BuiltinModule::Net),
                        "env" => ModuleKind::Builtin(BuiltinModule::Env),
                        _ => ModuleKind::CustomUnimplemented {
                            source_path: format!("builtin:{module}"),
                            resolved_path: std::path::PathBuf::new(),
                        },
                    };
                    modules.entry(module.clone()).or_insert(kind);
                    direct_functions.insert(function.clone(), (module.clone(), function.clone()));
                }
                ImportTarget::Custom(path) => {
                    let resolved = crate::paths::resolve_custom_import(&crate::paths::binary_dir(), path);
                    let name = binding_name(target);
                    modules.insert(name, ModuleKind::CustomUnimplemented {
                        source_path: path.clone(), resolved_path: resolved,
                    });
                }
                ImportTarget::CustomFunction { path, function } => {
                    let resolved = crate::paths::resolve_custom_import(&crate::paths::binary_dir(), path);
                    let module_name = std::path::Path::new(path)
                        .file_stem().and_then(|s| s.to_str()).unwrap_or(path).to_string();
                    modules.entry(module_name.clone()).or_insert(ModuleKind::CustomUnimplemented {
                        source_path: path.clone(), resolved_path: resolved,
                    });
                    direct_functions.insert(function.clone(), (module_name, function.clone()));
                }
            }
        }

        Self { modules, direct_functions }
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
                message: format!("{module_name} was not imported"),
            });
        };

        match kind {
            ModuleKind::Builtin(BuiltinModule::Net) => call_net(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Env) => call_env(function_name, args),
            ModuleKind::CustomUnimplemented { source_path, resolved_path } => {
                let note = if resolved_path.as_os_str().is_empty() {
                    String::new()
                } else {
                    format!(" (would resolve to {})", resolved_path.display())
                };
                Err(ModuleError {
                    message: format!(
                        "{module_name}.{function_name}(...) — \"{source_path}\"{note} \
                         is not implemented yet; this .route file parses, but the call cannot run until the module lands"
                    ),
                })
            }
        }
    }
}

fn call_net(function_name: &str, _args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "ping" => {
            let mut fields = HashMap::new();
            fields.insert("ok".to_string(), Value::Bool(true));
            Ok(Value::Object(fields))
        }
        other => Err(ModuleError {
            message: format!("net.{other} is not implemented — only net.ping() exists"),
        }),
    }
}

fn call_env(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "get" => {
            let Some(Value::String(key)) = args.first() else {
                return Err(ModuleError { message: "env.get(key) requires a string argument".into() });
            };
            Ok(match std::env::var(key) {
                Ok(v) => Value::String(v),
                Err(_) => Value::Null,
            })
        }
        other => Err(ModuleError {
            message: format!("env.{other} is not implemented — only env.get(key) exists"),
        }),
    }
}
