//! Discovers `.route` files, validates them during boot, generates AOT
//! artifacts, and builds the Axum router. Compiler diagnostics are kept
//! out of normal runtime logs and written to `data/admin/compiler-error.txt`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::routing::MethodRouter;
use axum::Router;
use core_lib::AppState;

use crate::analyzer::{analyze, Severity};
use crate::ast::{FunctionDef, ModuleFile, RouteFile, Value};
use crate::lexer::Lexer;
use crate::module_eval::ModuleExecutor;
use crate::module_runtime::{ModuleProgram, ServiceInterfaces};
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::terminal::Terminal;
use crate::transpiler::transpile_file;
use crate::video_host::VideoHostCapabilities;

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

struct CacheEntry {
    hash: u64,
    file: Arc<RouteFile>,
}

#[derive(Default)]
pub struct RouteCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
}

impl RouteCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn load(&self, path: &Path) -> anyhow::Result<Arc<RouteFile>> {
        let bytes = fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let hash = hash_bytes(&bytes);
        if let Some(entry) = self.entries.lock().unwrap().get(path) {
            if entry.hash == hash {
                return Ok(entry.file.clone());
            }
        }

        let source = String::from_utf8(bytes)
            .map_err(|e| anyhow::anyhow!("{}: not valid UTF-8: {e}", path.display()))?;
        let tokens = Lexer::new(&source).tokenize().map_err(|e| {
            anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message)
        })?;
        let file = Parser::new(tokens).parse_file().map_err(|e| {
            anyhow::anyhow!("{}:{}:{}: {}", path.display(), e.line, e.column, e.message)
        })?;
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

pub(crate) fn collect_files(
    dir: &Path,
    extension: &str,
    out: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
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
    let mut segments: Vec<String> = without_ext
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if segments.last().map(|s| s == "index").unwrap_or(false) {
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

fn append_runtime_error(path: &str, error: &str) {
    let error_path = compiler_error_path();
    if let Some(parent) = error_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&error_path)
    {
        let _ = writeln!(file, "E4000: route evaluation failed at {path}: {error}");
    }
}

const INLINE_ROUTE_HANDLER: &str = "\0rbe-route-handler";

fn request_value(method: &str, path: &str) -> Value {
    let mut fields = HashMap::new();
    fields.insert("method".into(), Value::String(method.to_string()));
    fields.insert("path".into(), Value::String(path.to_string()));
    fields.insert("params".into(), Value::Object(HashMap::new()));
    fields.insert("query".into(), Value::Object(HashMap::new()));
    Value::Object(fields)
}

async fn execute(
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    takes_request: bool,
    state: AppState,
    http_method: String,
    path: String,
) -> axum::response::Response {
    let args = if takes_request {
        vec![request_value(&http_method, &path)]
    } else {
        Vec::new()
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(
        module_program.as_ref(),
        state.services.clone(),
        Arc::new(VideoHostCapabilities::from_state(&state)),
    );
    match executor
        .call_inline(inline_file, INLINE_ROUTE_HANDLER, args)
        .await
    {
        Ok(value) => Json(value_to_json(&value)).into_response(),
        Err(err) => {
            tracing::error!(error = %err, path = %path, "route evaluation failed");
            append_runtime_error(&path, &err.to_string());
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
    module_program: Arc<ModuleProgram>,
    url_path: String,
) -> MethodRouter<AppState> {
    let mut router = MethodRouter::<AppState>::new();
    for method_def in &file.methods {
        let mut functions = file.functions.clone();
        functions.push(FunctionDef {
            name: INLINE_ROUTE_HANDLER.to_string(),
            params: method_def.param_name.clone().into_iter().collect(),
            body: method_def.body.clone(),
        });
        let inline_file = Arc::new(ModuleFile {
            imports: file.imports.clone(),
            functions,
            exports: Vec::new(),
        });
        let takes_request = method_def.param_name.is_some();
        let module_program = module_program.clone();
        let url_path = url_path.clone();
        let verb = method_def.verb.clone();
        let handler_verb = verb.clone();
        let handler = move |State(state): State<AppState>| {
            let inline_file = inline_file.clone();
            let module_program = module_program.clone();
            let path = url_path.clone();
            let method = handler_verb.to_uppercase();
            async move {
                execute(
                    inline_file,
                    module_program,
                    takes_request,
                    state,
                    method,
                    path,
                )
                .await
            }
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
    PathBuf::from("data")
        .join("admin")
        .join("compiler-error.txt")
}

fn find_symbol_location(source: &str, symbol: Option<&str>) -> (usize, usize) {
    let Some(symbol) = symbol.filter(|value| !value.is_empty()) else {
        return (1, 1);
    };
    for (line_idx, line) in source.lines().enumerate() {
        if let Some(byte_idx) = line.find(symbol) {
            let column = line[..byte_idx].chars().count() + 1;
            return (line_idx + 1, column);
        }
    }
    (1, 1)
}

fn frame_diagnostic_with_symbol(
    path: &Path,
    source: &str,
    fallback_line: usize,
    fallback_column: usize,
    message: &str,
    symbol: Option<&str>,
    terminal_width: usize,
) -> String {
    let (line, column) = if symbol.is_some() {
        find_symbol_location(source, symbol)
    } else {
        (fallback_line, fallback_column)
    };
    let lines: Vec<&str> = source.lines().collect();
    let start = line.saturating_sub(1);
    let end = (start + 4).min(lines.len());
    let line_numbers = if end > start {
        end.to_string().len()
    } else {
        1
    };
    let content_width = terminal_width.saturating_sub(line_numbers + 8).max(24);
    let border = "#".repeat(content_width + line_numbers + 7);
    let mut out = String::new();

    out.push_str(&format!("{}\n{}\n", path.display(), border));
    for (idx, text) in lines.iter().enumerate().take(end).skip(start) {
        let number = idx + 1;
        let clipped: String = text.chars().take(content_width).collect();
        let marker = if number == line {
            let used = format!("|{number:>width$}| {clipped}", width = line_numbers)
                .chars()
                .count();
            let remaining = border.len().saturating_sub(used + 1);
            format!(" {}", "<".repeat(remaining.max(2)))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "|{number:>width$}| {clipped}{marker}\n",
            width = line_numbers
        ));
    }
    out.push_str(&format!("{}\n", border));

    let pointer_indent = line_numbers + 4 + column.saturating_sub(1);
    out.push_str(&format!(
        "{}^\n{}\nline {}, column {}\n\n",
        " ".repeat(pointer_indent),
        message,
        line,
        column
    ));
    out
}

struct FileDiagnosticReport {
    path: PathBuf,
    errors: usize,
    warnings: usize,
    details: Vec<String>,
    error_details: Vec<String>,
}

fn render_diagnostic_reports(reports: &[FileDiagnosticReport]) {
    if reports.is_empty() {
        return;
    }

    for report in reports {
        println!(
            "{} error{}, {} warning{} in file {}",
            report.errors,
            if report.errors == 1 { "" } else { "s" },
            report.warnings,
            if report.warnings == 1 { "" } else { "s" },
            report.path.display()
        );
        for detail in &report.details {
            println!("{}", detail);
        }
    }
}

/// Run the route compiler as a boot-owned terminal session. Every file gets
/// three work units: parse, semantic analysis, and Rust artifact generation.
/// Syntax/semantic errors are accumulated across the entire tree and written
/// to `data/admin/compiler-error.txt` before boot is allowed to continue.
fn boot_compile(
    _api_dir: &Path,
    files: &[PathBuf],
) -> anyhow::Result<Vec<(PathBuf, Arc<RouteFile>)>> {
    let error_path = compiler_error_path();
    if let Some(parent) = error_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut error_file = fs::File::create(&error_path)?;

    let terminal = Terminal::new();
    let terminal_width = terminal.width();
    let total_units = files.len().saturating_mul(3);
    let mut done = 0usize;
    let mut valid = Vec::with_capacity(files.len());
    let mut reports = Vec::new();

    terminal.begin_boot();
    terminal.render(files.len(), None, "Parsing", 0, total_units);

    for path in files {
        let display_path = path.to_string_lossy();
        let mut report = FileDiagnosticReport {
            path: path.clone(),
            errors: 0,
            warnings: 0,
            details: Vec::new(),
            error_details: Vec::new(),
        };

        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.errors = 1;
                let detail = format!("{}: failed to read file: {}\n", path.display(), error);
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let source = match String::from_utf8(bytes) {
            Ok(source) => source,
            Err(error) => {
                report.errors = 1;
                let detail = format!("{}: invalid UTF-8: {}\n", path.display(), error);
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let tokens = match Lexer::new(&source).tokenize() {
            Ok(tokens) => tokens,
            Err(error) => {
                report.errors = 1;
                let detail = frame_diagnostic_with_symbol(
                    path,
                    &source,
                    error.line,
                    error.column,
                    &error.message,
                    None,
                    terminal_width,
                );
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 3;
                terminal.render(
                    files.len(),
                    Some(&display_path),
                    "Parsing",
                    done,
                    total_units,
                );
                reports.push(report);
                continue;
            }
        };

        let (file_opt, parse_errors) = Parser::new(tokens).parse_file_collecting();
        done += 1;
        terminal.render(
            files.len(),
            Some(&display_path),
            "Parsing",
            done,
            total_units,
        );

        for error in &parse_errors {
            let detail = frame_diagnostic_with_symbol(
                path,
                &source,
                error.line,
                error.column,
                &error.message,
                None,
                terminal_width,
            );
            report.errors += 1;
            report.details.push(detail.clone());
            report.error_details.push(detail);
        }

        let Some(file) = file_opt else {
            done += 2;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Parsing",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        };

        if report.errors > 0 {
            done += 2;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Parsing",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        }

        let file = Arc::new(file);
        terminal.render(
            files.len(),
            Some(&display_path),
            "Semantic",
            done,
            total_units,
        );

        let diagnostics = analyze(&file);
        report.errors += diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        report.warnings = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();

        for diagnostic in &diagnostics {
            let message = if matches!(diagnostic.code, "E3010" | "E3011") {
                diagnostic
                    .message
                    .replace("from the route file", &format!("from {}", path.display()))
            } else {
                diagnostic.message.clone()
            };
            let detail = frame_diagnostic_with_symbol(
                path,
                &source,
                1,
                1,
                &message,
                diagnostic.symbol.as_deref(),
                terminal_width,
            );
            report.details.push(detail.clone());
            if diagnostic.severity == Severity::Error {
                report.error_details.push(detail);
            }
        }

        done += 1;
        terminal.render(
            files.len(),
            Some(&display_path),
            "Semantic",
            done,
            total_units,
        );

        if report.errors > 0 {
            done += 1;
            terminal.render(
                files.len(),
                Some(&display_path),
                "Semantic",
                done,
                total_units,
            );
            reports.push(report);
            continue;
        }

        terminal.render(
            files.len(),
            Some(&display_path),
            "Generating",
            done,
            total_units,
        );
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        match transpile_file(&file, &path.to_string_lossy(), &module_names) {
            Ok(_) => {
                done += 1;
                valid.push((path.clone(), file));
            }
            Err(error) => {
                report.errors += 1;
                let detail = frame_diagnostic_with_symbol(
                    path,
                    &source,
                    1,
                    1,
                    &error.message,
                    None,
                    terminal_width,
                );
                report.details.push(detail.clone());
                report.error_details.push(detail);
                done += 1;
            }
        }
        terminal.render(
            files.len(),
            Some(&display_path),
            "Generating",
            done,
            total_units,
        );

        if report.errors > 0 || report.warnings > 0 {
            reports.push(report);
        }
    }

    let total_errors: usize = reports.iter().map(|report| report.errors).sum();
    if total_errors > 0 {
        for report in &reports {
            for detail in &report.error_details {
                error_file.write_all(detail.as_bytes())?;
            }
        }
        render_diagnostic_reports(&reports);
        terminal.end_boot();
        return Err(anyhow::anyhow!(
            "route compiler found {} error(s); see {}",
            total_errors,
            error_path.display()
        ));
    }

    if !reports.is_empty() {
        render_diagnostic_reports(&reports);
    }

    terminal.render(files.len(), None, "Ready", total_units, total_units);
    terminal.end_boot();
    Ok(valid)
}

/// Scans `api_dir`, validates every `.route` file before routing starts,
/// and only then constructs the Axum router. A broken route fails the boot,
/// but errors from every file are collected into compiler-error.txt first.
pub fn build_routes(
    api_dir: &Path,
    service_interfaces: &ServiceInterfaces,
) -> anyhow::Result<Router<AppState>> {
    let module_program = Arc::new(ModuleProgram::load_default_with_services(
        service_interfaces,
    )?);
    tracing::info!(
        modules = module_program.len(),
        module_dir = %module_program.module_dir().display(),
        "validated .module files"
    );

    let mut files = Vec::new();
    collect_route_files(api_dir, &mut files)?;
    files.sort();

    let compiled = boot_compile(api_dir, &files)?;
    let mut router: Router<AppState> = Router::new();

    for (path, route_file) in compiled {
        let url_path = url_path_for(api_dir, &path);

        tracing::info!(
            path = %path.display(),
            url = %url_path,
            methods = ?route_file.methods.iter().map(|m| &m.verb).collect::<Vec<_>>(),
            "registered .route file"
        );

        router = router.route(
            &url_path,
            build_method_router(&route_file, module_program.clone(), url_path.clone()),
        );
    }

    Ok(router)
}
