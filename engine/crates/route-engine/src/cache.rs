//! On-disk cache for AOT-generated Rust artifacts from `.route` files.

use std::path::{Path, PathBuf};

use crate::analyzer::{analyze, Severity};
use crate::ast::RouteFile;
use crate::discovery::{collect_route_files, hash_bytes};
use crate::lexer::Lexer;
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::transpiler::transpile_file;

const SOURCE_HASH_PREFIX: &str = "// source-hash: ";

pub struct SyncOutcome {
    pub route_path: PathBuf,
    pub artifact_path: PathBuf,
    pub result: Result<SyncAction, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SyncAction {
    UpToDate,
    Regenerated,
}

fn artifact_path_for(cache_dir: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    let relative = route_path.strip_prefix(api_dir).unwrap_or(route_path);
    cache_dir.join("artifact").join(relative).with_extension("rs")
}

fn existing_hash_matches(io: &atomic_io::AtomicIo, artifact_path: &Path, current_hash: u64) -> bool {
    let Ok(existing) = io.read(artifact_path) else { return false };
    let Ok(existing_text) = String::from_utf8(existing) else { return false };
    let Some(first_line) = existing_text.lines().next() else { return false };
    let Some(recorded) = first_line.strip_prefix(SOURCE_HASH_PREFIX) else { return false };
    recorded.trim() == current_hash.to_string()
}

fn diagnostic_text(route_path: &Path, severity: Severity, message: &str) -> String {
    let level = match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    format!("{}: {level}: {message}", route_path.display())
}

fn sync_one(
    io: &atomic_io::AtomicIo,
    api_dir: &Path,
    cache_dir: &Path,
    route_path: &Path,
) -> Result<SyncAction, String> {
    let bytes = std::fs::read(route_path)
        .map_err(|e| format!("failed to read {}: {e}", route_path.display()))?;
    let current_hash = hash_bytes(&bytes);
    let artifact_path = artifact_path_for(cache_dir, api_dir, route_path);

    if existing_hash_matches(io, &artifact_path, current_hash) {
        return Ok(SyncAction::UpToDate);
    }

    let source = String::from_utf8(bytes)
        .map_err(|e| format!("{}: not valid UTF-8: {e}", route_path.display()))?;
    let tokens = Lexer::new(&source)
        .tokenize()
        .map_err(|e| format!("{}:{}:{}: {}", route_path.display(), e.line, e.column, e.message))?;
    let file: RouteFile = Parser::new(tokens)
        .parse_file()
        .map_err(|e| format!("{}:{}:{}: {}", route_path.display(), e.line, e.column, e.message))?;

    let diagnostics = analyze(&file);
    let errors: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| diagnostic_text(route_path, d.severity, &d.message))
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
    let source_display = route_path.display().to_string();
    let generated = transpile_file(&file, &source_display, &module_names)
        .map_err(|e| format!("{}: {}", route_path.display(), e.message))?;

    let with_hash_header = format!("{SOURCE_HASH_PREFIX}{current_hash}\n{generated}");
    io.write_atomic(&artifact_path, with_hash_header.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", artifact_path.display()))?;

    Ok(SyncAction::Regenerated)
}

pub fn sync(io: &atomic_io::AtomicIo, api_dir: &Path, cache_dir: &Path) -> anyhow::Result<Vec<SyncOutcome>> {
    let mut route_paths = Vec::new();
    collect_route_files(api_dir, &mut route_paths)?;

    let mut outcomes = Vec::with_capacity(route_paths.len());
    for route_path in route_paths {
        let artifact_path = artifact_path_for(cache_dir, api_dir, &route_path);
        let result = sync_one(io, api_dir, cache_dir, &route_path);
        outcomes.push(SyncOutcome { route_path, artifact_path, result });
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
    fn generates_artifact_for_a_simple_route() {
        let root = temp_dir("basic");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("ping.route"),
            "class Route { get(req) { return { ok: true }; } }",
        )
        .unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        let outcomes = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].result, Ok(SyncAction::Regenerated));
        assert!(outcomes[0].artifact_path.exists());

        let contents = std::fs::read_to_string(&outcomes[0].artifact_path).unwrap();
        assert!(contents.starts_with(SOURCE_HASH_PREFIX));
        assert!(contents.contains("pub fn get"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn semantic_error_blocks_only_that_artifact() {
        let root = temp_dir("semantic-error");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("bad.route"),
            "class Route { get(req) { return missing; } }",
        )
        .unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        let outcomes = sync(&io, &api_dir, &cache_dir).unwrap();
        assert!(outcomes[0].result.is_err());
        assert!(!outcomes[0].artifact_path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn second_sync_with_unchanged_source_is_a_no_op() {
        let root = temp_dir("unchanged");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(api_dir.join("ping.route"), "class Route { get(req) { return true; } }").unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));
        let second = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::UpToDate));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn changing_source_triggers_regeneration() {
        let root = temp_dir("changed");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        let route_path = api_dir.join("ping.route");
        std::fs::write(&route_path, "class Route { get(req) { return true; } }").unwrap();
        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        sync(&io, &api_dir, &cache_dir).unwrap();
        std::fs::write(&route_path, "class Route { get(req) { return false; } }").unwrap();
        let second = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::Regenerated));
        let _ = std::fs::remove_dir_all(&root);
    }
}
