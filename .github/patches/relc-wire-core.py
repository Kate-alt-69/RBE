from pathlib import Path
import json

ROOT = Path(__file__).resolve().parents[2]


def replace_once(path, old, new):
    p = ROOT / path
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


# Expose the new RELC compiler layers from route-engine.
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "mod analyzer;\nmod ast;\nmod discovery;\nmod interpreter;\n",
    "mod analyzer;\nmod ast;\npub mod dependency_graph;\nmod discovery;\npub mod embedded_rel;\npub mod execution_tracker;\nmod interpreter;\n",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub mod cache;\npub mod server_rel;\npub mod source_registry;\n",
    "pub mod cache;\npub mod middleware_plan;\npub mod relc;\npub mod runtime_env;\npub mod runtime_image;\npub mod server_policy;\npub mod server_rel;\npub mod source_registry;\n",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use discovery::RouteCache;\n",
    "pub use dependency_graph::{SymbolDependencyGraph, SymbolId};\npub use discovery::RouteCache;\npub use embedded_rel::{extract_embedded_rel, EmbeddedRelError, EmbeddedRelSource, ExtractedServerRel};\npub use execution_tracker::{InvocationError, InvocationGuard, InvocationId, InvocationSnapshot, InvocationTracker};\n",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use module_eval::{ModuleEvalError, ModuleExecutor};\n",
    "pub use middleware_plan::{MiddlewarePlan, MiddlewarePlanError, MiddlewareStep};\npub use module_eval::{ModuleEvalError, ModuleExecutor};\n",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use parser::ParseError;\n",
    "pub use parser::ParseError;\npub use relc::{compile_runtime_image, PhysicalRelSource, RelcError};\npub use runtime_env::{RuntimeEnv, RuntimeEnvError, RuntimeEnvOrigin};\npub use runtime_image::{RuntimeImage, RuntimeImageSlot, RuntimeSourceManifest};\npub use server_policy::{PolicyOrigin, RecursionPolicy, ResolvedPolicyValue, ServerPolicy, ServerPolicyError, ServerStatus};\n",
)

# Fix the TokenKind partial-move before Clippy gets a chance to complain.
replace_once(
    "engine/crates/route-engine/src/server_rel.rs",
    "if !matches!(token.kind, TokenKind::Eof) {",
    "if !matches!(&token.kind, TokenKind::Eof) {",
)

# Fold the reserved-setting duplicate check into one condition.
replace_once(
    "engine/crates/route-engine/src/server_rel.rs",
    '''        if matches!(
            normalized.as_str(),
            "status" | "listener" | "env" | "middleware"
        ) {
            if reserved
                .insert(normalized.clone(), (setting.line, setting.column))
                .is_some()
            {
                return Err(ServerCompileError::semantic(
                    setting,
                    format!("duplicate `{}` Server REL section/setting", setting.name),
                ));
            }
        }
''',
    '''        if matches!(
            normalized.as_str(),
            "status" | "listener" | "env" | "middleware"
        ) && reserved
            .insert(normalized.clone(), (setting.line, setting.column))
            .is_some()
        {
            return Err(ServerCompileError::semantic(
                setting,
                format!("duplicate `{}` Server REL section/setting", setting.name),
            ));
        }
''',
)

# The extraction fixture is a raw Rust string. Backslash-escaped quotes would
# be literal REL marker bytes and correctly fail the marker parser.
embedded_path = ROOT / "engine/crates/route-engine/src/embedded_rel.rs"
embedded_text = embedded_path.read_text(encoding="utf-8")
embedded_text = embedded_text.replace('export function name() { return \\\"Auth\\\"; }', 'export function name() { return "Auth"; }')
embedded_text = embedded_text.replace('[file-start:route.Health path=\\\"/health\\\"]', '[file-start:route.Health path="/health"]')
embedded_text = embedded_text.replace("fn split_header<'a>(\n    input: &'a str,\n    line: usize,\n) -> Result<impl Iterator<Item = &'a str>, EmbeddedRelError> {", "fn split_header(\n    input: &str,\n    line: usize,\n) -> Result<impl Iterator<Item = &str>, EmbeddedRelError> {")
embedded_path.write_text(embedded_text, encoding="utf-8")

# Runtime ENV is a typed top-level deployment field in settings.json.
replace_once(
    "engine/crates/config/src/lib.rs",
    "use std::fmt;\nuse std::path::Path;\n",
    "use std::collections::BTreeMap;\nuse std::fmt;\nuse std::path::Path;\n",
)
replace_once(
    "engine/crates/config/src/lib.rs",
    "    #[serde(default)]\n    pub runtime: RuntimeConfig,\n    pub api: ApiConfig,\n",
    "    #[serde(default)]\n    pub runtime: RuntimeConfig,\n    #[serde(default)]\n    pub runtime_env: BTreeMap<String, serde_json::Value>,\n    pub api: ApiConfig,\n",
)

settings_path = ROOT / "engine/settings.json"
settings = json.loads(settings_path.read_text(encoding="utf-8"))
settings.setdefault("runtimeEnv", {})
# Keep runtimeEnv near runtime for humans, rather than silently appending a random key.
ordered = {}
for key, value in settings.items():
    ordered[key] = value
    if key == "runtime":
        ordered["runtimeEnv"] = settings["runtimeEnv"]
ordered.setdefault("runtimeEnv", settings["runtimeEnv"])
settings_path.write_text(json.dumps(ordered, indent=2) + "\n", encoding="utf-8")

# Fix test borrows in the policy file before formatting/building.
p = ROOT / "engine/crates/route-engine/src/server_policy.rs"
text = p.read_text(encoding="utf-8")
text = text.replace(
    "policy.get(\"listener.port\").unwrap().value,\n            ServerValue::Number(7044.0)",
    "&policy.get(\"listener.port\").unwrap().value,\n            ServerValue::Number(value) if *value == 7044.0",
)
text = text.replace(
    "policy.get(\"requestTimeoutMs\").unwrap().value,\n            ServerValue::Number(12000.0)",
    "&policy.get(\"requestTimeoutMs\").unwrap().value,\n            ServerValue::Number(value) if *value == 12000.0",
)
p.write_text(text, encoding="utf-8")

# Clippy prefers a guarded import match over an inner `if`.
p = ROOT / "engine/crates/route-engine/src/relc.rs"
text = p.read_text(encoding="utf-8")
text = text.replace(
    '''                ImportTarget::Service(service)
                | ImportTarget::ServiceFunction { service, .. } => {
                    if registry
                        .get_logical(RelSourceKind::Service, service)
                        .is_none()
                    {
                        return Err(RelcError::Link(format!(
                            "{source_id} imports missing service `{service}`"
                        )));
                    }
                }
''',
    '''                ImportTarget::Service(service)
                | ImportTarget::ServiceFunction { service, .. }
                    if registry
                        .get_logical(RelSourceKind::Service, service)
                        .is_none() =>
                {
                    return Err(RelcError::Link(format!(
                        "{source_id} imports missing service `{service}`"
                    )));
                }
''',
)
p.write_text(text, encoding="utf-8")
