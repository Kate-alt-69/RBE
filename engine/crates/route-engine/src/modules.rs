//! Import resolution and the built-in module registry.
//!
//! `:import[net]`-style bare identifiers resolve here, against a fixed,
//! curated set of real Rust functions — deliberately not "whatever
//! Node's `net` module exposes." Exposing exactly what we choose to
//! implement (not an arbitrary capability surface) is what makes this
//! safe to run without the container-runtime sandbox around it — see
//! migration-plan §5's isolation discussion for why an unconstrained
//! capability surface is the thing to avoid.
//!
//! `:import["./path"]`-style custom modules, and the `:import[module&name]`
//! shorthand for the default `/module/` folder (both desugar to
//! `ImportTarget::Custom` in the parser — see
//! `parser::classify_bareword_import`), are recognized here but full
//! `.module` file support is intentionally not implemented yet, per
//! this turn's explicit scope call ("basic API for .route files, full
//! .module later"). They parse successfully — so `.route` files that
//! import them don't fail to load — but any call into one produces a
//! clear error at request time instead of silently doing nothing.

use std::collections::HashMap;
use std::fmt;

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
}

pub struct ModuleRegistry {
    modules: HashMap<String, ModuleKind>,
}

/// Derives the in-file binding name for an import — the name a
/// `.route` file's body refers to it by. Builtins bind to their own
/// name (`net` -> `net`); custom imports bind to their file stem
/// (`"./module/storage"` -> `storage`), matching the example in the
/// original request.
pub fn binding_name(target: &ImportTarget) -> String {
    match target {
        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::Custom(path) => std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string(),
    }
}

impl ModuleRegistry {
    /// Builds a registry scoped to exactly the imports one `.route`
    /// file declared — not a global registry every file shares, so a
    /// file that didn't `:import[net]` can't accidentally call it even
    /// if it guesses the name right.
    pub fn from_imports(imports: &[ImportTarget]) -> Self {
        let mut modules = HashMap::new();

        for target in imports {
            let name = binding_name(target);
            let kind = match target {
                ImportTarget::Builtin(builtin_name) => match builtin_name.as_str() {
                    "net" => ModuleKind::Builtin(BuiltinModule::Net),
                    "env" => ModuleKind::Builtin(BuiltinModule::Env),
                    other => {
                        // Registering an unknown builtin as "unimplemented"
                        // rather than failing the whole file to parse —
                        // consistent with the philosophy that new builtins
                        // get added deliberately, not silently.
                        modules.insert(
                            name.clone(),
                            ModuleKind::CustomUnimplemented {
                                source_path: format!("builtin:{other}"),
                                resolved_path: std::path::PathBuf::new(),
                            },
                        );
                        continue;
                    }
                },
                ImportTarget::Custom(path) => {
                    let resolved =
                        crate::paths::resolve_custom_import(&crate::paths::binary_dir(), path);
                    ModuleKind::CustomUnimplemented {
                        source_path: path.clone(),
                        resolved_path: resolved,
                    }
                }
            };
            modules.insert(name, kind);
        }

        Self { modules }
    }

    pub fn call(
        &self,
        module_name: &str,
        function_name: &str,
        args: &[Value],
    ) -> Result<Value, ModuleError> {
        let Some(kind) = self.modules.get(module_name) else {
            return Err(ModuleError {
                message: format!(
                    "{module_name} was not imported — add :import[{module_name}] at the top of this .route file"
                ),
            });
        };

        match kind {
            ModuleKind::Builtin(BuiltinModule::Net) => call_net(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Env) => call_env(function_name, args),
            ModuleKind::CustomUnimplemented {
                source_path,
                resolved_path,
            } => {
                let resolution_note = if resolved_path.as_os_str().is_empty() {
                    String::new()
                } else {
                    format!(" (would resolve to {})", resolved_path.display())
                };
                Err(ModuleError {
                    message: format!(
                        "{module_name}.{function_name}(...) — \"{source_path}\"{resolution_note} \
                         is not implemented yet; this .route file parses fine, but this call can't \
                         run until it lands"
                    ),
                })
            }
        }
    }
}

fn call_net(function_name: &str, _args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        // Deliberately trivial placeholder proving the wiring end to
        // end — real networking (outbound HTTP, DNS, etc.) is a real
        // capability surface decision to make carefully, not something
        // to wire in as a side effect of proving the parser works.
        "ping" => {
            let mut fields = HashMap::new();
            fields.insert("ok".to_string(), Value::Bool(true));
            Ok(Value::Object(fields))
        }
        other => Err(ModuleError {
            message: format!("net.{other} is not implemented — only net.ping() exists in v1"),
        }),
    }
}

fn call_env(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "get" => {
            let Some(Value::String(key)) = args.first() else {
                return Err(ModuleError {
                    message: "env.get(key) requires a string argument".to_string(),
                });
            };
            Ok(match std::env::var(key) {
                Ok(v) => Value::String(v),
                Err(_) => Value::Null,
            })
        }
        other => Err(ModuleError {
            message: format!("env.{other} is not implemented — only env.get(key) exists in v1"),
        }),
    }
}
