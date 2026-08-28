//! Boot-time loader and dependency validator for `.module` files.

use std::collections::{HashMap, HashSet};

pub type ServiceInterfaces = HashMap<String, HashSet<String>>;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ast::{ImportTarget, ModuleFile};
use crate::discovery::collect_files;
use crate::lexer::Lexer;
use crate::modules::binding_name;
use crate::parser::Parser;
use crate::paths::{binary_dir, default_module_dir, resolve_custom_import};

#[derive(Debug, Clone)]
pub struct ModuleCompileError {
    pub code: &'static str,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl std::fmt::Display for ModuleCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}:{}:{}: {}",
            self.code,
            self.path.display(),
            self.line,
            self.column,
            self.message
        )
    }
}

#[derive(Debug, Clone)]
pub struct ModuleCompileErrors(pub Vec<ModuleCompileError>);

impl ModuleCompileErrors {
    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl std::fmt::Display for ModuleCompileErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} module compiler error(s)", self.0.len())
    }
}

impl std::error::Error for ModuleCompileErrors {}

#[derive(Debug, Clone)]
pub struct ModuleProgram {
    binary_root: PathBuf,
    module_dir: PathBuf,
    modules: HashMap<PathBuf, Arc<ModuleFile>>,
}

impl ModuleProgram {
    pub fn load_default() -> Result<Self, ModuleCompileErrors> {
        Self::load(&default_module_dir())
    }

    pub fn load_default_with_services(
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        Self::load_with_services(&default_module_dir(), services)
    }

    pub fn load(module_dir: &Path) -> Result<Self, ModuleCompileErrors> {
        Self::load_internal(module_dir, None)
    }

    pub fn load_with_services(
        module_dir: &Path,
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        Self::load_internal(module_dir, Some(services))
    }

    fn load_internal(
        module_dir: &Path,
        services: Option<&ServiceInterfaces>,
    ) -> Result<Self, ModuleCompileErrors> {
        let binary_root = module_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(binary_dir);
        let mut files = Vec::new();
        if let Err(error) = collect_files(module_dir, "module", &mut files) {
            return Err(ModuleCompileErrors(vec![ModuleCompileError {
                code: "MOD1000",
                path: module_dir.to_path_buf(),
                line: 0,
                column: 0,
                message: format!("failed to scan module directory: {error}"),
            }]));
        }
        files.sort();

        let mut errors = Vec::new();
        let mut modules = HashMap::new();
        for path in files {
            match load_one(&path) {
                Ok(file) => {
                    validate_local(&path, &file, services, &mut errors);
                    modules.insert(normalize(&path), Arc::new(file));
                }
                Err(error) => errors.push(error),
            }
        }

        let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for (path, file) in &modules {
            let mut dependencies = Vec::new();
            for import in &file.imports {
                if let Some(raw_path) = custom_import_path(import) {
                    let resolved = normalize(&resolve_custom_import(&binary_root, raw_path));
                    if modules.contains_key(&resolved) {
                        dependencies.push(resolved);
                    } else {
                        errors.push(ModuleCompileError {
                            code: "MOD2001",
                            path: path.clone(),
                            line: 1,
                            column: 1,
                            message: format!(
                                "module import {raw_path:?} resolves to missing file {}",
                                resolved.display()
                            ),
                        });
                    }
                }
            }
            graph.insert(path.clone(), dependencies);
        }
        detect_cycles(&graph, &mut errors);

        if !errors.is_empty() {
            return Err(ModuleCompileErrors(errors));
        }

        Ok(Self {
            binary_root,
            module_dir: module_dir.to_path_buf(),
            modules,
        })
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    pub fn module_dir(&self) -> &Path {
        &self.module_dir
    }

    pub fn resolve(&self, raw_path: &str) -> Option<Arc<ModuleFile>> {
        let path = normalize(&resolve_custom_import(&self.binary_root, raw_path));
        self.modules.get(&path).cloned()
    }
}

fn load_one(path: &Path) -> Result<ModuleFile, ModuleCompileError> {
    let bytes = fs::read(path).map_err(|error| ModuleCompileError {
        code: "MOD1001",
        path: path.to_path_buf(),
        line: 0,
        column: 0,
        message: format!("failed to read module: {error}"),
    })?;
    let source = String::from_utf8(bytes).map_err(|error| ModuleCompileError {
        code: "MOD1002",
        path: path.to_path_buf(),
        line: 0,
        column: 0,
        message: format!("module is not valid UTF-8: {error}"),
    })?;
    let tokens = Lexer::new(&source)
        .tokenize()
        .map_err(|error| ModuleCompileError {
            code: "MOD1100",
            path: path.to_path_buf(),
            line: error.line,
            column: error.column,
            message: error.message,
        })?;
    Parser::new(tokens)
        .parse_module_file()
        .map_err(|error| ModuleCompileError {
            code: "MOD1200",
            path: path.to_path_buf(),
            line: error.line,
            column: error.column,
            message: error.message,
        })
}

fn validate_local(
    path: &Path,
    file: &ModuleFile,
    services: Option<&ServiceInterfaces>,
    errors: &mut Vec<ModuleCompileError>,
) {
    let mut functions = HashSet::new();
    for function in &file.functions {
        if !functions.insert(function.name.clone()) {
            errors.push(ModuleCompileError {
                code: "MOD2002",
                path: path.to_path_buf(),
                line: 1,
                column: 1,
                message: format!("duplicate function {:?}", function.name),
            });
        }
    }

    let mut exports = HashSet::new();
    for export in &file.exports {
        if !exports.insert(export.clone()) {
            errors.push(ModuleCompileError {
                code: "MOD2003",
                path: path.to_path_buf(),
                line: 1,
                column: 1,
                message: format!("duplicate export {export:?}"),
            });
        }
        if !functions.contains(export) {
            errors.push(ModuleCompileError {
                code: "MOD2004",
                path: path.to_path_buf(),
                line: 1,
                column: 1,
                message: format!("export {export:?} has no function body"),
            });
        }
    }

    let mut bindings = HashSet::new();
    let mut sources = HashSet::new();
    for import in &file.imports {
        let binding = binding_name(import);
        if !bindings.insert(binding.clone()) {
            errors.push(ModuleCompileError {
                code: "MOD2005",
                path: path.to_path_buf(),
                line: 1,
                column: 1,
                message: format!("duplicate import binding {binding:?}"),
            });
        }
        let source = import_source_key(import);
        if !sources.insert(source.clone()) {
            errors.push(ModuleCompileError {
                code: "MOD2006",
                path: path.to_path_buf(),
                line: 1,
                column: 1,
                message: format!("duplicate import source {source:?}"),
            });
        }

        let Some(services) = services else {
            continue;
        };
        match import_base(import) {
            ImportTarget::Service(service) => {
                if !services.contains_key(service) {
                    errors.push(ModuleCompileError {
                        code: "MOD2008",
                        path: path.to_path_buf(),
                        line: 1,
                        column: 1,
                        message: format!("module imports unknown service {service:?}"),
                    });
                }
            }
            ImportTarget::ServiceFunction { service, function } => match services.get(service) {
                None => errors.push(ModuleCompileError {
                    code: "MOD2008",
                    path: path.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: format!("module imports unknown service {service:?}"),
                }),
                Some(exports) if !exports.contains(function) => {
                    errors.push(ModuleCompileError {
                        code: "MOD2009",
                        path: path.to_path_buf(),
                        line: 1,
                        column: 1,
                        message: format!("service {service:?} does not export {function:?}"),
                    });
                }
                Some(_) => {}
            },
            _ => {}
        }
    }
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => target.as_ref(),
        other => other,
    }
}

fn custom_import_path(import: &ImportTarget) -> Option<&str> {
    match import_base(import) {
        ImportTarget::Custom(path) | ImportTarget::CustomFunction { path, .. } => {
            Some(path.as_str())
        }
        _ => None,
    }
}

fn import_source_key(import: &ImportTarget) -> String {
    match import_base(import) {
        ImportTarget::Builtin(name) => format!("builtin:{name}"),
        ImportTarget::BuiltinFunction { module, function } => {
            format!("builtin:{module}.{function}")
        }
        ImportTarget::Custom(path) => format!("module:{path}"),
        ImportTarget::CustomFunction { path, function } => {
            format!("module:{path}.{function}")
        }
        ImportTarget::Service(service) => format!("service:{service}"),
        ImportTarget::ServiceFunction { service, function } => {
            format!("service:{service}.{function}")
        }
        ImportTarget::Aliased { .. } => unreachable!(),
    }
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Done,
}

fn detect_cycles(graph: &HashMap<PathBuf, Vec<PathBuf>>, errors: &mut Vec<ModuleCompileError>) {
    let mut states = HashMap::new();
    let mut stack = Vec::new();
    let mut reported = HashSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut states, &mut stack, &mut reported, errors);
    }
}

fn visit(
    node: &PathBuf,
    graph: &HashMap<PathBuf, Vec<PathBuf>>,
    states: &mut HashMap<PathBuf, VisitState>,
    stack: &mut Vec<PathBuf>,
    reported: &mut HashSet<String>,
    errors: &mut Vec<ModuleCompileError>,
) {
    match states.get(node) {
        Some(VisitState::Done) => return,
        Some(VisitState::Visiting) => {
            let start = stack.iter().position(|item| item == node).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(node.clone());
            let rendered = cycle
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            if reported.insert(rendered.clone()) {
                errors.push(ModuleCompileError {
                    code: "MOD2007",
                    path: node.clone(),
                    line: 1,
                    column: 1,
                    message: format!("circular module dependency: {rendered}"),
                });
            }
            return;
        }
        None => {}
    }

    states.insert(node.clone(), VisitState::Visiting);
    stack.push(node.clone());
    if let Some(dependencies) = graph.get(node) {
        for dependency in dependencies {
            visit(dependency, graph, states, stack, reported, errors);
        }
    }
    stack.pop();
    states.insert(node.clone(), VisitState::Done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-module-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("module")).unwrap();
        path
    }

    #[test]
    fn loads_exports_and_dependencies() {
        let root = root();
        fs::write(
            root.join("module/b.module"),
            "export function twice(value) { return value * 2; }",
        )
        .unwrap();
        fs::write(
            root.join("module/a.module"),
            ":import[module&b]\nfunction hidden() { return 1; }\nexport function run(value) { return value; }",
        )
        .unwrap();

        let program = ModuleProgram::load(&root.join("module")).unwrap();
        assert_eq!(program.len(), 2);
        let a = program.resolve("./module/a").unwrap();
        assert_eq!(a.exports, vec!["run"]);
        assert_eq!(a.functions.len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_cycles_at_boot() {
        let root = root();
        fs::write(
            root.join("module/a.module"),
            ":import[module&b]\nexport function a() { return 1; }",
        )
        .unwrap();
        fs::write(
            root.join("module/b.module"),
            ":import[module&a]\nexport function b() { return 1; }",
        )
        .unwrap();

        let error = ModuleProgram::load(&root.join("module")).unwrap_err();
        assert!(error.0.iter().any(|item| item.code == "MOD2007"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accepts_service_imports_without_module_dependencies() {
        let root = root();
        fs::write(
            root.join("module/cache.module"),
            ":import[service:uac-cache, service:search.find as lookup]\nexport function run(value) { return value; }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        assert_eq!(program.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn service_contract_validation_rejects_unknown_service_and_export() {
        let root = root();
        fs::write(
  root.join("module/service.module"),
  ":import[service:missing as missing, service:search.nope as nope]\nexport function run() { return true; }",
        )
        .unwrap();
        let mut services = ServiceInterfaces::new();
        services.insert("search".into(), HashSet::from(["find".into()]));
        let errors = ModuleProgram::load_with_services(&root.join("module"), &services)
            .expect_err("invalid service contracts should fail module boot validation");
        assert!(errors.0.iter().any(|error| error.code == "MOD2008"));
        assert!(errors.0.iter().any(|error| error.code == "MOD2009"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn service_contract_validation_accepts_registered_interfaces() {
        let root = root();
        fs::write(
  root.join("module/service.module"),
  ":import[service:uac-cache as cache, service:search.find as lookup]\nexport function run(value) { return value; }",
        )
        .unwrap();
        let mut services = ServiceInterfaces::new();
        services.insert("uac-cache".into(), HashSet::from(["get".into()]));
        services.insert("search".into(), HashSet::from(["find".into()]));
        ModuleProgram::load_with_services(&root.join("module"), &services)
            .expect("registered service interfaces should validate");
        let _ = fs::remove_dir_all(root);
    }
}
