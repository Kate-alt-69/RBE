//! Scans an `/api/` directory for `.route` files, parses each (cached
//! by content hash — see [`RouteCache`]), and builds the Axum router
//! for them. File path -> URL path follows the common file-router
//! convention (Next.js-style): `api/account/profile.route` ->
//! `/api/account/profile`; `index.route` maps to its directory.
//!
//! No dynamic path segments (`[id].route`) in v1 — `RequestContext::params`
//! is always empty for now. Worth adding once real routes need it;
//! deliberately out of scope for proving the base grammar out.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::response::{IntoResponse, Json};
use axum::routing::MethodRouter;
use axum::Router;
use core_lib::AppState;

use crate::ast::{MethodDef, RouteFile, Value};
use crate::interpreter::{Interpreter, RequestContext};
use crate::lexer::Lexer;
use crate::modules::{binding_name, ModuleRegistry};
use crate::parser::Parser;

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

struct CacheEntry {
    hash: u64,
    file: Arc<RouteFile>,
}

/// Content-hash-keyed cache: a `.route` file only gets re-lexed and
/// re-parsed when its bytes actually changed. This is the real,
/// buildable version of "smart caching" — at file granularity, via a
/// dependency-graph-shaped hash check, not sub-file diffing (see this
/// turn's design discussion for why that distinction matters).
#[derive(Default)]
pub struct RouteCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
}

impl RouteCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let hash = hash_bytes(&bytes);

        if let Some(entry) = self.entries.lock().unwrap().get(path) {
            if entry.hash == hash {
                return Ok(entry.file.clone());
            }
        }

        let source = String::from_utf8(bytes)
            .map_err(|e| anyhow::anyhow!("{}: not valid UTF-8: {e}", path.display()))?;
        let tokens = Lexer::new(&source)
            .tokenize()
            .map_err(|e| anyhow::anyhow!("{}:{}: {}", path.display(), e.line, e.message))?;
        let file = Parser::new(tokens)
            .parse_file()
            .map_err(|e| anyhow::anyhow!("{}: {}", path.display(), e.message))?;
        let file = Arc::new(file);

        self.entries.lock().unwrap().insert(
            path.to_path_buf(),
            CacheEntry {
                hash,
                file: file.clone(),
            },
        );

        Ok(file)
    }
}

pub(crate) fn collect_route_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_route_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("route") {
            out.push(path);
        }
    }
    Ok(())
}

pub(crate) fn url_path_for(api_dir: &Path, file_path: &Path) -> String {
    let relative = file_path.strip_prefix(api_dir).unwrap_or(file_path);
    let without_ext = relative.with_extension("");
    let mut segments: Vec<String> = without_ext
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();

    if segments
        .last()
        .map(|s| s.as_str() == "index")
        .unwrap_or(false)
    {
        segments.pop();
    }

    format!("/api/{}", segments.join("/"))
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Null => serde_json::Value::Null,
        Value::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), value_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
    }
}

async fn execute(
    method_def: Arc<MethodDef>,
    modules: Arc<ModuleRegistry>,
    module_names: Arc<Vec<String>>,
    http_method: String,
    path: String,
) -> axum::response::Response {
    let req_ctx = RequestContext {
        method: http_method,
        path,
        params: HashMap::new(),
        query: HashMap::new(),
    };

    let mut interpreter = Interpreter::new(&modules);
    match interpreter.run(&method_def, &req_ctx, &module_names) {
        Ok(value) => Json(value_to_json(&value)).into_response(),
        Err(err) => {
            tracing::error!(error = %err, path = %req_ctx.path, "route evaluation failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

fn build_method_router(
    file: &RouteFile,
    modules: Arc<ModuleRegistry>,
    module_names: Arc<Vec<String>>,
    url_path: String,
) -> MethodRouter<AppState> {
    let mut router: MethodRouter<AppState> = MethodRouter::new();

    for method_def in &file.methods {
        let method_def = Arc::new(method_def.clone());
        let modules = modules.clone();
        let module_names = module_names.clone();
        let url_path = url_path.clone();
        let verb = method_def.verb.clone();

        let handler = move || {
            let method_def = method_def.clone();
            let modules = modules.clone();
            let module_names = module_names.clone();
            let verb_upper = method_def.verb.to_uppercase();
            let path = url_path.clone();
            async move { execute(method_def, modules, module_names, verb_upper, path).await }
        };

        router = match verb.as_str() {
            "get" => router.get(handler),
            "post" => router.post(handler),
            "put" => router.put(handler),
            "delete" => router.delete(handler),
            "patch" => router.patch(handler),
            "head" => router.head(handler),
            "options" => router.options(handler),
            // Unreachable: the parser already rejects unknown verbs.
            _ => router,
        };
    }

    router
}

/// Scans `api_dir` for `.route` files and returns the Axum router
/// serving them. Fails fast (matches §3.2's boot philosophy) on any
/// parse error — a broken `.route` file should stop the backend from
/// starting, not silently drop that route.
pub fn build_routes(api_dir: &Path) -> anyhow::Result<Router<AppState>> {
    let cache = RouteCache::new();
    let mut files = Vec::new();
    collect_route_files(api_dir, &mut files)?;

    let mut router: Router<AppState> = Router::new();

    for path in files {
        let route_file = cache.load(&path)?;
        let url_path = url_path_for(api_dir, &path);
        let module_names: Arc<Vec<String>> =
            Arc::new(route_file.imports.iter().map(binding_name).collect());
        let modules = Arc::new(ModuleRegistry::from_imports(&route_file.imports));

        tracing::info!(
            path = %path.display(),
            url = %url_path,
            methods = ?route_file.methods.iter().map(|m| &m.verb).collect::<Vec<_>>(),
            "registered .route file"
        );

        let method_router =
            build_method_router(&route_file, modules, module_names, url_path.clone());
        router = router.route(&url_path, method_router);
    }

    Ok(router)
}
