//! Discovers `.route` files, validates them during boot, generates AOT
//! artifacts, and builds the Axum router. Compiler diagnostics are kept
//! out of normal runtime logs and written to `data/admin/compiler-error.txt`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::response::{IntoResponse, Json};
use axum::routing::MethodRouter;
use axum::Router;
use core_lib::AppState;

use crate::analyzer::{analyze, Severity};
use crate::ast::{FunctionDef, MethodDef, RouteFile, Value};
use crate::interpreter::{Interpreter, RequestContext};
use crate::lexer::Lexer;
use crate::modules::{binding_name, ModuleRegistry};
use crate::parser::Parser;
use crate::transpiler::transpile_file;

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

struct CacheEntry { hash: u64, file: Arc<RouteFile> }

#[derive(Default)]
pub struct RouteCache { entries: Mutex<HashMap<PathBuf, CacheEntry>> }

impl RouteCache {
    pub fn new() -> Self { Self::default() }

    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {
        let bytes = fs::read(path).map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let hash = hash_bytes(&bytes);
        if let Some(entry) = self.entries.lock().unwrap().get(path) {
            if entry.hash == hash { return Ok(entry.file.clone()); }
        }

        let source = String::from_utf8(bytes)
            .map_err(|e| anyhow::anyhow!("{}: not valid UTF-8: {e}", path.display()))?;
        let tokens = Lexer::new(&source).tokenize()
            .map_err(|e| anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message))?;
        let file = Parser::new(tokens).parse_file()
            .map_err(|e| anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message))?;
        let file = Arc::new(file);
        self.entries.lock().unwrap().insert(path.to_path_buf(), CacheEntry { hash, file: file.clone() });
        Ok(file)
    }
}

pub(crate) fn collect_files(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.exists() { return Ok(()); }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, extension, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
            out.push(path);
        }
    }
    Ok(())
}

pub(crate) fn collect_route_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    collect_files(dir, "route", out)
}

pub(crate) fn url_path_for(api_dir: &Path, file_path: &Path) -> String {
    let relative = file_path.strip_prefix(api_dir).unwrap_or(file_path);
    let without_ext = relative.with_extension("");
    let mut segments: Vec<String> = without_ext.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    if segments.last().map(|s| s == "index").unwrap_or(false) { segments.pop(); }
    format!("/api/{}", segments.join("/"))
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Number(n) => serde_json::Number::from_f64(*n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Null => serde_json::Value::Null,
        Value::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map { obj.insert(k.clone(), value_to_json(v)); }
            serde_json::Value::Object(obj)
        }
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
    }
}

async fn execute(
    method_def: Arc<MethodDef>,
    functions: Arc<Vec<FunctionDef>>,
    modules: Arc<ModuleRegistry>,
    module_names: Arc<Vec<String>>,
    http_method: String,
    path: String,
) -> axum::response::Response {
    let req_ctx = RequestContext { method: http_method, path, params: HashMap::new(), query: HashMap::new() };
    let mut interpreter = Interpreter::new(&modules).with_functions(functions.as_ref());

    match interpreter.run(&method_def, &req_ctx, &module_names) {
        Ok(value) => Json(value_to_json(&value)).into_response(),
        Err(err) => {
            tracing::error!(error = %err, path = %req_ctx.path, "route evaluation failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            ).into_response()
        }
    }
}

fn build_method_router(
    file: &RouteFile,
    modules: Arc<ModuleRegistry>,
    module_names: Arc<Vec<String>>,
    url_path: String,
) -> MethodRouter<AppState> {
    let mut router = MethodRouter::<AppState>::new();
    let functions = Arc::new(file.functions.clone());

    for method_def in &file.methods {
        let method_def = Arc::new(method_def.clone());
        let modules = modules.clone();
        let module_names = module_names.clone();
        let functions = functions.clone();
        let url_path = url_path.clone();
        let verb = method_def.verb.clone();

        let handler = move || {
            let method_def = method_def.clone();
            let modules = modules.clone();
            let module_names = module_names.clone();
            let functions = functions.clone();
            let path = url_path.clone();
            async move { execute(method_def, functions, modules, module_names, verb.to_uppercase(), path).await }
        };

        router = match verb.as_str() {
            "get" => router.get(handler),
            "post" => router.post(handler),
            "put" => router.put(handler),
            "delete" => router.delete(handler),
            "patch" => router.patch(handler),
            "head" => router.head(handler),
            "options" => router.options(handler),
            _ => router,
        };
    }

    router
}

fn compiler_error_path() -> PathBuf {
    PathBuf::from("data").join("admin").join("compiler-error.txt")
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 40)
        .unwrap_or(80)
}

fn find_symbol_location(source: &str, symbol: Option<&str>) -> (usize, usize) {
    let Some(symbol) = symbol.filter(|value| !value.is_empty()) else { return (1, 1); };
    for (line_idx, line) in source.lines().enumerate() {
        if let Some(byte_idx) = line.find(symbol) {
            let column = line[..byte_idx].chars().count() + 1;
            return (line_idx + 1, column);
        }
    }
    (1, 1)
}

fn frame_diagnostic(path: &Path, source: &str, line: usize, column: usize, message: &str) -> String {
    frame_diagnostic_with_symbol(path, source, line, column, message, None)
}

fn frame_diagnostic_with_symbol(path: &Path, source: &str, fallback_line: usize, fallback_column: usize, message: &str, symbol: Option<&str>) -> String {
    let (line, column) = if symbol.is_some() {
        find_symbol_location(source, symbol)
    } else {
        (fallback_line, fallback_column)
    };
    let lines: Vec<&str> = source.lines().collect();
    let start = line.saturating_sub(1);
    let end = (start + 4).min(lines.len());
    let line_numbers = if end > start { end.to_string().len() } else { 1 };
    let content_width = terminal_width().saturating_sub(line_numbers + 8).max(24);
    let border = "#".repeat(content_width + line_numbers + 7);
    let mut out = String::new();

    out.push_str(&format!("{path}\n{border}\n"));
    for (idx, text) in lines.iter().enumerate().take(end).skip(start) {
        let number = idx + 1;
        let clipped: String = text.chars().take(content_width).collect();
        let marker = if number == line {
            let used = format!("|{number:>width$}| {clipped}", width = line_numbers).chars().count();
            let remaining = border.len().saturating_sub(used + 1);
            format!(" {}", "<".repeat(remaining.max(2)))
        } else {
            String::new()
        };
        out.push_str(&format!("|{number:>width$}| {clipped}{marker}\n", width = line_numbers));
    }
    out.push_str(&format!("{border}\n"));

    let pointer_indent = line_numbers + 4 + column.saturating_sub(1);
    out.push_str(&format!("{}^\n{}\nline {}, column {}\n\n", " ".repeat(pointer_indent), message, line, column));
    out
}

fn render_progress(state: &str, current: usize, total: usize) {
    if !io::stdout().is_terminal() { return; }

    let width = terminal_width();
    let counter = format!("{}/{}", current, total);
    let label = format!("\x1b[1m{state}\x1b[0m {counter}");
    let plain_label_width = state.chars().count() + 1 + counter.chars().count();
    let bar_width = width.saturating_sub(plain_label_width + 3).max(12).min(width.saturating_sub(2));
    let filled = if total == 0 { bar_width } else { (current.saturating_mul(bar_width) / total).min(bar_width) };
    let bar = format!("[\x1b[36m{}\x1b[90m{}\x1b[0m]", "█".repeat(filled), "░".repeat(bar_width - filled));

    print!("\x1b[2K\r{label}\n\x1b[2K\r{bar}");
    let _ = io::stdout().flush();
}

fn print_compiler_header(route_count: usize) {
    if io::stdout().is_terminal() {
        println!("\x1b[2J\x1b[H\x1b[1;36mRBE Route Compiler\x1b[0m");
        println!("Scanning ./api...");
        println!("Found {route_count} route files");
        println!();
    } else {
        println!("RBE Route Compiler — scanning ./api ({route_count} route files)");
    }
}

/// Run the route compiler as a boot-owned terminal session. Every file gets
/// three work units: parse, semantic analysis, and Rust artifact generation.
/// Syntax/semantic errors are accumulated across the entire tree and written
/// to `data/admin/compiler-error.txt` before boot is allowed to continue.
fn boot_compile(_api_dir: &Path, files: &[PathBuf]) -> anyhow::Result<Vec<(PathBuf, Arc<RouteFile>)>> {
    let error_path = compiler_error_path();
    if let Some(parent) = error_path.parent() { fs::create_dir_all(parent)?; }
    let mut error_file = fs::File::create(&error_path)?;

    let total_units = files.len().saturating_mul(3);
    let mut done = 0usize;
    let mut valid = Vec::with_capacity(files.len());
    let mut errors = Vec::new();

    print_compiler_header(files.len());
    render_progress("Parsing", 0, total_units);

    for path in files {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!("{}: failed to read file: {error}\n", path.display()));
                done += 3;
                continue;
            }
        };
        let source = match String::from_utf8(bytes) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("{}: invalid UTF-8: {error}\n", path.display()));
                done += 3;
                continue;
            }
        };

        let tokens = match Lexer::new(&source).tokenize() {
            Ok(tokens) => tokens,
            Err(error) => {
                errors.push(frame_diagnostic(path, &source, error.line, error.column, &error.message));
                done += 3;
                continue;
            }
        };

        let file = match Parser::new(tokens).parse_file() {
            Ok(file) => Arc::new(file),
            Err(error) => {
                errors.push(frame_diagnostic(path, &source, error.line, error.column, &error.message));
                done += 3;
                continue;
            }
        };
        done += 1;
        render_progress("Parsing", done, total_units);
        render_progress("Semantic", done, total_units);

        let diagnostics = analyze(&file);
        for diagnostic in diagnostics.iter().filter(|d| d.severity == Severity::Error) {
            errors.push(frame_diagnostic_with_symbol(path, &source, 1, 1, &diagnostic.message, diagnostic.symbol.as_deref()));
        }
        if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            done += 2;
            continue;
        }
        done += 1;

        render_progress("Generating", done, total_units);
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        match transpile_file(&file, &path.to_string_lossy(), &module_names) {
            Ok(_) => {
                done += 1;
                valid.push((path.clone(), file));
            }
            Err(error) => {
                errors.push(frame_diagnostic(path, &source, 1, 1, &error.message));
                done += 1;
            }
        }
        render_progress("Generating", done, total_units);
    }

    println!();
    if !errors.is_empty() {
        for diagnostic in &errors { error_file.write_all(diagnostic.as_bytes())?; }
        return Err(anyhow::anyhow!("route compiler found {} error(s); see {}", errors.len(), error_path.display()));
    }

    render_progress("Ready", total_units, total_units);
    println!();

    Ok(valid)
}

/// Scans `api_dir`, validates every `.route` file before routing starts,
/// and only then constructs the Axum router. A broken route fails the boot,
/// but errors from every file are collected into compiler-error.txt first.
pub fn build_routes(api_dir: &Path) -> anyhow::Result<Router<AppState>> {
    let mut files = Vec::new();
    collect_route_files(api_dir, &mut files)?;
    files.sort();

    let compiled = boot_compile(api_dir, &files)?;
    let mut router: Router<AppState> = Router::new();

    for (path, route_file) in compiled {
        let url_path = url_path_for(api_dir, &path);
        let module_names: Arc<Vec<String>> = Arc::new(route_file.imports.iter().map(binding_name).collect());
        let modules = Arc::new(ModuleRegistry::from_imports(&route_file.imports));

        tracing::info!(
            path = %path.display(),
            url = %url_path,
            methods = ?route_file.methods.iter().map(|m| &m.verb).collect::<Vec<_>>(),
            "registered .route file"
        );

        router = router.route(&url_path, build_method_router(&route_file, modules, module_names, url_path));
    }

    Ok(router)
}
