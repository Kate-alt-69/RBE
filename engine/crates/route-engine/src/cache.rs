//! The on-disk half of Phase 1 (RBE Upgrade Plan §2): mirrors `.route`
//! files under `api_dir` into generated Rust artifacts at
//! `.cache/backend/artifact/<same relative path>.rs` — `api/uac/check.route`
//! produces `.cache/backend/artifact/uac/check.rs`; `api/index.route`
//! produces `.cache/backend/artifact/index.rs`. Deliberately mirrors
//! the SOURCE file's path, not the URL `discovery::url_path_for`
//! derives from it (no `index` stripping, no `/api` prefix) — the
//! cache's job is "one artifact per source file," a simpler and more
//! stable relationship than the URL-routing one, which can change
//! independently later without needing to reshuffle the cache.
//!
//! **Best-effort, not on the critical path.** A transpile failure here
//! is logged and skipped — it does NOT stop the backend from booting
//! or from serving that route via the interpreter, which is what
//! actually handles requests in Phase 1 (see `crate::transpiler`'s doc
//! comment: nothing here is wired into request serving yet). This
//! module exists to prove artifact generation is correct and to make
//! the artifacts inspectable, not to gate uptime on it.
//!
//! **Ephemeral, by construction, not by a separate cleanup step.** Each
//! generated file's first line records the source content hash it was
//! generated from (`// source-hash: <hash>`); [`sync`] only regenerates
//! a file whose recorded hash doesn't match the current source. There's
//! no separate manifest/index file tracking staleness — deleting the
//! entire `.cache/` directory just means every hash check "misses" and
//! everything regenerates from the `.route` source on the next call,
//! which is the whole point (RBE Upgrade Plan §2, "Ephemeral Design").

use std::path::{Path, PathBuf};

use crate::ast::RouteFile;
use crate::discovery::{collect_route_files, hash_bytes};
use crate::lexer::Lexer;
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::transpiler::transpile_file;

const SOURCE_HASH_PREFIX: &str = "// source-hash: ";

/// One file's outcome — returned per-file so the caller (`main.rs`'s
/// boot sequence) can log a useful summary instead of either total
/// silence or a wall of per-file lines.
pub struct SyncOutcome {
    pub route_path: PathBuf,
    pub artifact_path: PathBuf,
    pub result: Result<SyncAction, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// Artifact already matched this file's current content hash —
    /// nothing written.
    UpToDate,
    /// Written fresh, or regenerated because the source changed.
    Regenerated,
}

fn artifact_path_for(cache_dir: &Path, api_dir: &Path, route_path: &Path) -> PathBuf {
    let relative = route_path.strip_prefix(api_dir).unwrap_or(route_path);
    cache_dir.join("artifact").join(relative).with_extension("rs")
}

fn existing_hash_matches(io: &atomic_io::AtomicIo, artifact_path: &Path, current_hash: u64) -> bool {
    let Ok(existing) = io.read(artifact_path) else {
        return false; // doesn't exist yet, or unreadable — needs (re)generation
    };
    let Ok(existing_text) = String::from_utf8(existing) else {
        return false; // shouldn't happen for our own generated files, but don't trust it blindly
    };
    let Some(first_line) = existing_text.lines().next() else {
        return false;
    };
    let Some(recorded) = first_line.strip_prefix(SOURCE_HASH_PREFIX) else {
        return false;
    };
    recorded.trim() == current_hash.to_string()
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
        .map_err(|e| format!("{}:{}: {}", route_path.display(), e.line, e.message))?;
    let file: RouteFile = Parser::new(tokens)
        .parse_file()
        .map_err(|e| format!("{}: {}", route_path.display(), e.message))?;

    let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
    let source_display = route_path.display().to_string();
    let generated = transpile_file(&file, &source_display, &module_names)
        .map_err(|e| format!("{}: {}", route_path.display(), e.message))?;

    let with_hash_header = format!("{SOURCE_HASH_PREFIX}{current_hash}\n{generated}");
    io.write_atomic(&artifact_path, with_hash_header.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", artifact_path.display()))?;

    Ok(SyncAction::Regenerated)
}

/// Walks `api_dir` for `.route` files and mirrors each into
/// `cache_dir/artifact/...` (see module doc comment). Never returns an
/// `Err` for an individual file's transpile failure — those are
/// carried in each [`SyncOutcome::result`] instead, so one broken
/// route's artifact doesn't stop the others from being generated. Only
/// returns `Err` for something that makes the whole sweep meaningless
/// (can't even list `api_dir`).
pub fn sync(io: &atomic_io::AtomicIo, api_dir: &Path, cache_dir: &Path) -> anyhow::Result<Vec<SyncOutcome>> {
    let mut route_paths = Vec::new();
    collect_route_files(api_dir, &mut route_paths)?;

    let mut outcomes = Vec::with_capacity(route_paths.len());
    for route_path in route_paths {
        let artifact_path = artifact_path_for(cache_dir, api_dir, &route_path);
        let result = sync_one(io, api_dir, cache_dir, &route_path);
        outcomes.push(SyncOutcome {
            route_path,
            artifact_path,
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
    fn generates_artifact_for_a_simple_route() {
        let root = temp_dir("basic");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("ping.route"),
            r#"
            :import[net]
            class Route {
                async get(req) {
                    return { ok: true };
                }
            }
            "#,
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
    fn second_sync_with_unchanged_source_is_a_no_op() {
        let root = temp_dir("unchanged");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("ping.route"),
            "class Route {\n  get(req) {\n    return true;\n  }\n}\n",
        )
        .unwrap();

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
        std::fs::write(&route_path, "class Route {\n  get(req) {\n    return true;\n  }\n}\n").unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        sync(&io, &api_dir, &cache_dir).unwrap();

        std::fs::write(&route_path, "class Route {\n  get(req) {\n    return false;\n  }\n}\n").unwrap();
        let second = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(second[0].result, Ok(SyncAction::Regenerated));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_the_cache_dir_causes_full_regeneration() {
        let root = temp_dir("deleted-cache");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("ping.route"),
            "class Route {\n  get(req) {\n    return true;\n  }\n}\n",
        )
        .unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        sync(&io, &api_dir, &cache_dir).unwrap();

        std::fs::remove_dir_all(&cache_dir).unwrap();
        let after_delete = sync(&io, &api_dir, &cache_dir).unwrap();
        assert_eq!(
            after_delete[0].result,
            Ok(SyncAction::Regenerated),
            "with the cache dir gone, sync should regenerate rather than error"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_broken_route_does_not_block_the_others() {
        let root = temp_dir("one-broken");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("good.route"),
            "class Route {\n  get(req) {\n    return true;\n  }\n}\n",
        )
        .unwrap();
        // A bare module reference — parses fine (imports aren't
        // validated at parse time), but transpiles to an Err per
        // `transpiler`'s test coverage of the same case.
        std::fs::write(
            api_dir.join("bad.route"),
            ":import[net]\nclass Route {\n  get(req) {\n    return net;\n  }\n}\n",
        )
        .unwrap();

        let cache_dir = root.join(".cache/backend");
        let io = atomic_io::AtomicIo::new();
        let outcomes = sync(&io, &api_dir, &cache_dir).unwrap();

        assert_eq!(outcomes.len(), 2);
        let good = outcomes.iter().find(|o| o.route_path.ends_with("good.route")).unwrap();
        let bad = outcomes.iter().find(|o| o.route_path.ends_with("bad.route")).unwrap();
        assert_eq!(good.result, Ok(SyncAction::Regenerated));
        assert!(bad.result.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
