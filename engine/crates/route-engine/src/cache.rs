//! Per-endpoint on-disk cache for generated `.route` artifacts.
//!
//! Every discovered API endpoint owns a deterministic directory under
//! `./.cache/api/`. The current pipeline stores generated Rust there and
//! reserves `module.wasm` for the compiled WASM image. A source change
//! invalidates both generated artifacts so stale WASM can never survive a
//! route edit.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::analyzer::{analyze, Severity};
use crate::ast::RouteFile;
use crate::discovery::{collect_route_files, hash_bytes};
use crate::lexer::Lexer;
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::transpiler::transpile_file;
use crate::wasm_compiler::{compile_route, RouteWasmCompilation, ROUTE_WASM_ABI_VERSION};

const CACHE_MANIFEST_VERSION: u64 = 2;

pub struct SyncOutcome {
    pub route_path: PathBuf,
    pub endpoint_cache_dir: PathBuf,
    pub artifact_path: PathBuf,
    pub wasm_path: PathBuf,
    pub manifest_path: PathBuf,
    pub result: Result<SyncAction, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SyncAction {
    UpToDate,
    Regenerated,
}

fn endpoint_cache_dir(cache_root: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    let relative = route_path.strip_prefix(api_dir).unwrap_or(route_path);
    let mut endpoint = cache_root.join("api").join(relative);
    endpoint.set_extension("");
    endpoint
}

fn artifact_path_for(cache_root: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    endpoint_cache_dir(cache_root, api_dir, route_path).join("generated.rs")
}

fn wasm_path_for(cache_root: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    endpoint_cache_dir(cache_root, api_dir, route_path).join("module.wasm")
}

fn manifest_path_for(cache_root: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    endpoint_cache_dir(cache_root, api_dir, route_path).join("manifest.json")
}

fn existing_hash_matches(
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
    if manifest.get("version").and_then(serde_json::Value::as_u64) != Some(CACHE_MANIFEST_VERSION)
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

fn diagnostic_text(route_path: &Path, severity: Severity, message: &str) -> String {
    let level = match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    format!("{}: {level}: {message}", route_path.display())
}

fn invalidate_endpoint_cache(paths: &[&Path]) -> Result<(), String> {
    for path in paths {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|error| format!("failed to invalidate {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn write_manifest(
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

fn sync_one(
    io: &atomic_io::AtomicIo,
    api_dir: &Path,
    cache_root: &Path,
    route_path: &Path,
) -> Result<SyncAction, String> {
    let bytes = std::fs::read(route_path)
        .map_err(|error| format!("failed to read {}: {error}", route_path.display()))?;
    let current_hash = hash_bytes(&bytes);

    let endpoint_dir = endpoint_cache_dir(cache_root, api_dir, route_path);
    std::fs::create_dir_all(&endpoint_dir)
        .map_err(|error| format!("failed to create {}: {error}", endpoint_dir.display()))?;

    let artifact_path = artifact_path_for(cache_root, api_dir, route_path);
    let wasm_path = wasm_path_for(cache_root, api_dir, route_path);
    let manifest_path = manifest_path_for(cache_root, api_dir, route_path);

    if existing_hash_matches(io, &manifest_path, &artifact_path, &wasm_path, current_hash) {
        return Ok(SyncAction::UpToDate);
    }

    // Never let a stale executable artifact survive source drift. The future
    // WASM compiler may atomically repopulate module.wasm after this stage.
    invalidate_endpoint_cache(&[&artifact_path, &wasm_path, &manifest_path])?;

    let source = String::from_utf8(bytes)
        .map_err(|error| format!("{}: not valid UTF-8: {error}", route_path.display()))?;
    let tokens = Lexer::new(&source).tokenize().map_err(|error| {
        format!(
            "{}:{}:{}: {}",
            route_path.display(),
            error.line,
            error.column,
            error.message
        )
    })?;
    let file: RouteFile = Parser::new(tokens).parse_file().map_err(|error| {
        format!(
            "{}:{}:{}: {}",
            route_path.display(),
            error.line,
            error.column,
            error.message
        )
    })?;

    let diagnostics = analyze(&file);
    let errors: Vec<String> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .map(|diagnostic| diagnostic_text(route_path, diagnostic.severity, &diagnostic.message))
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
    let source_display = route_path.display().to_string();
    let generated = transpile_file(&file, &source_display, &module_names)
        .map_err(|error| format!("{}: {}", route_path.display(), error.message))?;

    let wasm = compile_route(&file);
    if let RouteWasmCompilation::Native(native) = &wasm {
        io.write_atomic(&wasm_path, &native.bytes)
            .map_err(|error| format!("failed to write {}: {error}", wasm_path.display()))?;
    }
    io.write_atomic(&artifact_path, generated.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", artifact_path.display()))?;
    // Manifest is committed last and therefore acts as the cache validity marker.
    write_manifest(io, api_dir, route_path, &manifest_path, current_hash, &wasm)?;

    Ok(SyncAction::Regenerated)
}

pub fn sync(
    io: &atomic_io::AtomicIo,
    api_dir: &Path,
    cache_root: &Path,
) -> anyhow::Result<Vec<SyncOutcome>> {
    // The cache root exists even on a fresh deployment with zero route files,
    // so later AOT/WASM stages have one predictable location.
    std::fs::create_dir_all(cache_root.join("api"))?;

    let mut route_paths = Vec::new();
    collect_route_files(api_dir, &mut route_paths)?;

    let mut outcomes = Vec::with_capacity(route_paths.len());
    for route_path in route_paths {
        let endpoint_cache_dir = endpoint_cache_dir(cache_root, api_dir, &route_path);
        let artifact_path = artifact_path_for(cache_root, api_dir, &route_path);
        let wasm_path = wasm_path_for(cache_root, api_dir, &route_path);
        let manifest_path = manifest_path_for(cache_root, api_dir, &route_path);
        let result = sync_one(io, api_dir, cache_root, &route_path);
        outcomes.push(SyncOutcome {
            route_path,
            endpoint_cache_dir,
            artifact_path,
            wasm_path,
            manifest_path,
            result,
        });
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("route-engine-cache-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_a_dedicated_cache_for_every_endpoint() {
        let root = temp_dir("endpoint-layout");
        let api_dir = root.join("api");
        std::fs::create_dir_all(api_dir.join("example")).unwrap();
        std::fs::write(
            api_dir.join("health.route"),
            "class Route { get(req) { return { ok: true }; } }",
        )
        .unwrap();
        std::fs::write(
            api_dir.join("example").join("ping.route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let outcomes = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(outcomes.len(), 2);

        for outcome in outcomes {
            assert!(outcome.endpoint_cache_dir.is_dir());
            assert_eq!(
                outcome
                    .artifact_path
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some("generated.rs")
            );
            assert_eq!(
                outcome.wasm_path.file_name().and_then(|name| name.to_str()),
                Some("module.wasm")
            );
            assert!(outcome.artifact_path.is_file());
            assert!(outcome.wasm_path.is_file());
            assert!(outcome.manifest_path.is_file());
            assert_eq!(outcome.result, Ok(SyncAction::Regenerated));
        }

        assert!(cache_root.join("api/health/generated.rs").is_file());
        assert!(cache_root.join("api/example/ping/generated.rs").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn semantic_error_keeps_the_endpoint_slot_but_no_executable_artifact() {
        let root = temp_dir("semantic-error");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("bad.route"),
            "class Route { get(req) { return missing; } }",
        )
        .unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let outcomes = sync(&io, &api_dir, &cache_root).unwrap();
        assert!(outcomes[0].result.is_err());
        assert!(outcomes[0].endpoint_cache_dir.is_dir());
        assert!(!outcomes[0].artifact_path.exists());
        assert!(!outcomes[0].wasm_path.exists());
        assert!(!outcomes[0].manifest_path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn second_sync_with_unchanged_source_is_a_no_op() {
        let root = temp_dir("unchanged");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("ping.route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));
        let second = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::UpToDate));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
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
}
