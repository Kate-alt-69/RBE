from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:180]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


compiler = Path("engine/crates/route-engine/src/wasm_compiler.rs")
compiler.write_text(r'''//! Native REL `.route` -> WebAssembly lowering.
//!
//! This compiler deliberately exposes a small executable subset first instead
//! of disguising interpreter execution as WASM. Unsupported REL constructs are
//! classified as an explicit interpreter fallback. Native artifacts use the
//! RBE worker ABI and return JSON bytes through `rbe.output_write`.

use sha2::{Digest, Sha256};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::ast::{Expr, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 1;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const WASM_PAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct RouteWasmArtifact {
    pub verb: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub enum RouteWasmCompilation {
    Native(RouteWasmArtifact),
    InterpreterFallback { reason: String },
}

impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

/// Compile the currently supported native route subset.
///
/// The first slice is intentionally strict: one route method whose result is a
/// JSON-literal REL value and no imported/helper execution. This already
/// produces real WebAssembly executed by Wasmtime. Dynamic request expressions,
/// local functions and host capabilities remain explicit interpreter fallback
/// until their individual ABI lowering is implemented.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    if !file.imports.is_empty() {
        return fallback("route imports are not WASM-native yet");
    }
    if !file.functions.is_empty() {
        return fallback("route helper functions are not WASM-native yet");
    }
    if file.methods.len() != 1 {
        return fallback("native route compilation currently requires exactly one HTTP method");
    }

    let method = &file.methods[0];
    let [Statement::Return(expr)] = method.body.as_slice() else {
        return fallback("native route body currently requires one literal return statement");
    };
    let Some(value) = static_json(expr) else {
        return fallback("route return value depends on runtime REL evaluation");
    };
    let output = match serde_json::to_vec(&value) {
        Ok(output) => output,
        Err(error) => return fallback(format!("static route result could not be encoded: {error}")),
    };
    if output.len() > MAX_STATIC_OUTPUT_BYTES {
        return fallback("static route result exceeds the WASM execution output limit");
    }

    let bytes = encode_static_json_module(&output);
    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
    })
}

fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {
    RouteWasmCompilation::InterpreterFallback {
        reason: reason.into(),
    }
}

fn static_json(expr: &Expr) -> Option<serde_json::Value> {
    match expr {
        Expr::String(value) => Some(serde_json::Value::String(value.clone())),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(serde_json::Value::Number),
        Expr::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Array(values) => values
            .iter()
            .map(static_json)
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Expr::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), static_json(value)?);
            }
            Some(serde_json::Value::Object(object))
        }
        Expr::Ident(_)
        | Expr::Member(_, _)
        | Expr::Call(_, _)
        | Expr::UnaryNot(_)
        | Expr::Binary { .. } => None,
    }
}

fn encode_static_json_module(output: &[u8]) -> Vec<u8> {
    // Function type 0: rbe.output_write(i32 ptr, i32 len) -> i32.
    // Function type 1: run() -> i32.
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function([], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "output_write", EntityType::Function(0));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let pages = output.len().max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // output_write is imported function index 0; run is defined function index 1.
    exports.export("run", ExportKind::Func, 1);

    let mut run = Function::new([]);
    run.instructions()
        .i32_const(0)
        .i32_const(output.len() as i32)
        .call(0)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), output.iter().copied());

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    #[test]
    fn compiles_literal_route_to_valid_deterministic_wasm() {
        let route = parse("class Route { get(req) { return { ok: true, count: 7 }; } }");
        let RouteWasmCompilation::Native(first) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        let RouteWasmCompilation::Native(second) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        assert_eq!(first.verb, "get");
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(&first.bytes[..4], b"\0asm");
        wasmparser::validate(&first.bytes).unwrap();
    }

    #[test]
    fn runtime_expression_is_explicit_interpreter_fallback() {
        let route = parse("class Route { post(req) { return req.body; } }");
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("dynamic route must not pretend to be native WASM");
        };
        assert!(reason.contains("runtime REL evaluation"));
    }

    #[test]
    fn multiple_methods_are_not_collapsed_into_one_run_export() {
        let route = parse(
            "class Route { get(req) { return true; } post(req) { return false; } }",
        );
        assert!(!compile_route(&route).is_native());
    }
}
''', encoding="utf-8")

# Expose the compiler for cache/runtime-image integration.
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub mod transpiler;\nmod video_host;",
    "pub mod transpiler;\npub mod wasm_compiler;\nmod video_host;",
)
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "pub use source_registry::{\n    RelSource, RelSourceKind, RelSourceRegistry, SourceId, SourceOrigin, SourceRegistryError,\n};",
    "pub use source_registry::{\n    RelSource, RelSourceKind, RelSourceRegistry, SourceId, SourceOrigin, SourceRegistryError,\n};\n"
    "pub use wasm_compiler::{\n"
    "    compile_route as compile_route_wasm, RouteWasmArtifact, RouteWasmCompilation,\n"
    "    ROUTE_WASM_ABI_VERSION,\n"
    "};",
)

# Pin direct encoder/parser versions for deterministic generated artifacts.
manifest = Path("engine/crates/route-engine/Cargo.toml")
text = manifest.read_text(encoding="utf-8")
if 'hex = "0.4"' not in text:
    text = text.rstrip() + '\nhex = "0.4"\nwasm-encoder = "0.248"\n\n[dev-dependencies]\nwasmparser = "0.248"\n'
else:
    raise SystemExit("route-engine already has unexpected direct hex dependency; review compiler patch")
manifest.write_text(text, encoding="utf-8")

# ---------------------------------------------------------------------------
# Route cache: compiler ABI/version participates in validity, native wasm is
# written atomically, and fallback state is represented explicitly in manifest.
# ---------------------------------------------------------------------------
cache = Path("engine/crates/route-engine/src/cache.rs")
text = cache.read_text(encoding="utf-8")
text = text.replace(
    "use std::path::{Path, PathBuf};\n\n",
    "use std::path::{Path, PathBuf};\n\nuse sha2::{Digest, Sha256};\n\n",
    1,
)
text = text.replace(
    "use crate::transpiler::transpile_file;\n\nconst CACHE_MANIFEST_VERSION: u64 = 1;",
    "use crate::transpiler::transpile_file;\n"
    "use crate::wasm_compiler::{compile_route, RouteWasmCompilation, ROUTE_WASM_ABI_VERSION};\n\n"
    "const CACHE_MANIFEST_VERSION: u64 = 2;",
    1,
)
old = r'''fn existing_hash_matches(
    io: &atomic_io::AtomicIo,
    manifest_path: &Path,
    artifact_path: &Path,
    current_hash: u64,
) -> bool {
    if !artifact_path.is_file() {
        return false;
    }
    let Ok(existing) = io.read(manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&existing) else {
        return false;
    };
    manifest.get("version").and_then(serde_json::Value::as_u64) == Some(CACHE_MANIFEST_VERSION)
        && manifest
            .get("source_hash")
            .and_then(serde_json::Value::as_str)
            == Some(current_hash.to_string().as_str())
}
'''
new = r'''fn existing_hash_matches(
    io: &atomic_io::AtomicIo,
    manifest_path: &Path,
    artifact_path: &Path,
    wasm_path: &Path,
    current_hash: u64,
) -> bool {
    if !artifact_path.is_file() {
        return false;
    }
    let Ok(existing) = io.read(manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&existing) else {
        return false;
    };
    if manifest.get("version").and_then(serde_json::Value::as_u64)
        != Some(CACHE_MANIFEST_VERSION)
        || manifest
            .get("source_hash")
            .and_then(serde_json::Value::as_str)
            != Some(current_hash.to_string().as_str())
    {
        return false;
    }
    let Some(wasm) = manifest.get("wasm").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if wasm.get("abi_version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(ROUTE_WASM_ABI_VERSION))
    {
        return false;
    }
    match wasm.get("status").and_then(serde_json::Value::as_str) {
        Some("native") => {
            let Some(expected) = wasm.get("sha256").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let Ok(bytes) = io.read(wasm_path) else {
                return false;
            };
            hex::encode(Sha256::digest(bytes)) == expected
        }
        Some("interpreter_fallback") => !wasm_path.exists(),
        _ => false,
    }
}
'''
if old not in text:
    raise SystemExit("cache validity anchor missing")
text = text.replace(old, new, 1)
old = r'''fn write_manifest(
    io: &atomic_io::AtomicIo,
    api_dir: &Path,
    route_path: &Path,
    manifest_path: &Path,
    current_hash: u64,
) -> Result<(), String> {
    let relative = route_path.strip_prefix(api_dir).unwrap_or(route_path);
    let manifest = serde_json::json!({
        "version": CACHE_MANIFEST_VERSION,
        "route": relative.to_string_lossy(),
        "source_hash": current_hash.to_string(),
        "generated_rust": "generated.rs",
        "wasm_artifact": "module.wasm",
    });
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to encode {}: {error}", manifest_path.display()))?;
    io.write_atomic(manifest_path, &bytes)
        .map_err(|error| format!("failed to write {}: {error}", manifest_path.display()))
}
'''
new = r'''fn write_manifest(
    io: &atomic_io::AtomicIo,
    api_dir: &Path,
    route_path: &Path,
    manifest_path: &Path,
    current_hash: u64,
    wasm: &RouteWasmCompilation,
) -> Result<(), String> {
    let relative = route_path.strip_prefix(api_dir).unwrap_or(route_path);
    let wasm = match wasm {
        RouteWasmCompilation::Native(artifact) => serde_json::json!({
            "status": "native",
            "abi_version": ROUTE_WASM_ABI_VERSION,
            "sha256": artifact.sha256,
            "verb": artifact.verb,
        }),
        RouteWasmCompilation::InterpreterFallback { reason } => serde_json::json!({
            "status": "interpreter_fallback",
            "abi_version": ROUTE_WASM_ABI_VERSION,
            "reason": reason,
        }),
    };
    let manifest = serde_json::json!({
        "version": CACHE_MANIFEST_VERSION,
        "route": relative.to_string_lossy(),
        "source_hash": current_hash.to_string(),
        "generated_rust": "generated.rs",
        "wasm_artifact": "module.wasm",
        "wasm": wasm,
    });
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to encode {}: {error}", manifest_path.display()))?;
    io.write_atomic(manifest_path, &bytes)
        .map_err(|error| format!("failed to write {}: {error}", manifest_path.display()))
}
'''
if old not in text:
    raise SystemExit("cache manifest anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    "if existing_hash_matches(io, &manifest_path, &artifact_path, current_hash) {",
    "if existing_hash_matches(\n        io,\n        &manifest_path,\n        &artifact_path,\n        &wasm_path,\n        current_hash,\n    ) {",
    1,
)
old = r'''    io.write_atomic(&artifact_path, generated.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", artifact_path.display()))?;
    write_manifest(io, api_dir, route_path, &manifest_path, current_hash)?;

    Ok(SyncAction::Regenerated)
'''
new = r'''    let wasm = compile_route(&file);
    if let RouteWasmCompilation::Native(native) = &wasm {
        io.write_atomic(&wasm_path, &native.bytes)
            .map_err(|error| format!("failed to write {}: {error}", wasm_path.display()))?;
    }
    io.write_atomic(&artifact_path, generated.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", artifact_path.display()))?;
    // Manifest is committed last and therefore acts as the cache validity marker.
    write_manifest(
        io,
        api_dir,
        route_path,
        &manifest_path,
        current_hash,
        &wasm,
    )?;

    Ok(SyncAction::Regenerated)
'''
if old not in text:
    raise SystemExit("cache artifact write anchor missing")
text = text.replace(old, new, 1)
# Native literal route cache entries now contain real WASM.
text = text.replace(
    "            assert!(outcome.artifact_path.is_file());\n            assert!(outcome.manifest_path.is_file());",
    "            assert!(outcome.artifact_path.is_file());\n            assert!(outcome.wasm_path.is_file());\n            assert!(outcome.manifest_path.is_file());",
    1,
)
old_test = r'''    #[test]
    fn source_change_invalidates_a_stale_wasm_image() {
        let root = temp_dir("invalidate-wasm");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        let route_path = api_dir.join("ping.route");
        std::fs::write(&route_path, "class Route { get(req) { return true; } }").unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_root).unwrap();
        std::fs::write(&first[0].wasm_path, b"old-wasm").unwrap();
        assert!(first[0].wasm_path.is_file());

        std::fs::write(&route_path, "class Route { get(req) { return false; } }").unwrap();
        let second = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::Regenerated));
        assert!(!second[0].wasm_path.exists());

        let manifest = std::fs::read_to_string(&second[0].manifest_path).unwrap();
        assert!(manifest.contains("\"wasm_artifact\": \"module.wasm\""));
        let _ = std::fs::remove_dir_all(&root);
    }
'''
new_test = r'''    #[test]
    fn source_change_replaces_a_stale_wasm_image() {
        let root = temp_dir("invalidate-wasm");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        let route_path = api_dir.join("ping.route");
        std::fs::write(&route_path, "class Route { get(req) { return true; } }").unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_root).unwrap();
        std::fs::write(&first[0].wasm_path, b"old-wasm").unwrap();

        std::fs::write(&route_path, "class Route { get(req) { return false; } }").unwrap();
        let second = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::Regenerated));
        let wasm = std::fs::read(&second[0].wasm_path).unwrap();
        assert_ne!(wasm, b"old-wasm");
        wasmparser::validate(&wasm).unwrap();

        let manifest = std::fs::read_to_string(&second[0].manifest_path).unwrap();
        assert!(manifest.contains("\"status\": \"native\""));
        assert!(manifest.contains("\"wasm_artifact\": \"module.wasm\""));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dynamic_route_records_explicit_interpreter_fallback_without_wasm() {
        let root = temp_dir("wasm-fallback");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("echo.route"),
            "class Route { post(req) { return req.body; } }",
        )
        .unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));
        assert!(!first[0].wasm_path.exists());
        let manifest = std::fs::read_to_string(&first[0].manifest_path).unwrap();
        assert!(manifest.contains("\"status\": \"interpreter_fallback\""));

        let second = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::UpToDate));
        let _ = std::fs::remove_dir_all(&root);
    }
'''
if old_test not in text:
    raise SystemExit("cache wasm invalidation test anchor missing")
text = text.replace(old_test, new_test, 1)
cache.write_text(text, encoding="utf-8")

print("native REL route WASM compiler applied")
