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
use crate::runtime_image::RuntimeImage;

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

    pub fn from_runtime_image_with_services(
        image: &RuntimeImage,
        services: &ServiceInterfaces,
    ) -> Result<Self, ModuleCompileErrors> {
        let module_dir = default_module_dir();
        let binary_root = module_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(binary_dir);
        let mut modules = HashMap::new();
        let mut errors = Vec::new();

        for id in &image.modules {
            let Some(manifest) = image.source(id) else {
                errors.push(ModuleCompileError {
                    code: "MOD1010",
                    path: module_dir.clone(),
                    line: 0,
                    column: 0,
                    message: format!("Runtime Image is missing module manifest {id}"),
                });
                continue;
            };
            let Some(file) = image.module_file(id) else {
                errors.push(ModuleCompileError {
                    code: "MOD1011",
                    path: module_dir.clone(),
                    line: 0,
                    column: 0,
                    message: format!("Runtime Image is missing executable module {id}"),
                });
                continue;
            };
            let mut path = module_dir.join(&manifest.logical_name);
            path.set_extension("module");
            // Package-style `X from Y` imports in an immutable Runtime Image
            // have already passed RELC's verified PackageLinkContext gate.
            // The legacy filesystem loader has no such trusted context and
            // therefore keeps rejecting non-builtin sub-library spellings.
            validate_local(&path, file.as_ref(), Some(services), true, &mut errors);
            modules.insert(normalize(&path), file);
        }

        validate_module_graph(&binary_root, &modules, &mut errors);

        if !errors.is_empty() {
            return Err(ModuleCompileErrors(errors));
        }

        Ok(Self {
            binary_root,
            module_dir,
            modules,
        })
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
                    validate_local(&path, &file, services, false, &mut errors);
                    modules.insert(normalize(&path), Arc::new(file));
                }
                Err(error) => errors.push(error),
            }
        }

        validate_module_graph(&binary_root, &modules, &mut errors);

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
        self.resolve_scoped(raw_path).map(|(_, file)| file)
    }

    pub fn resolve_scoped(&self, raw_path: &str) -> Option<(String, Arc<ModuleFile>)> {
        let path = normalize(&resolve_custom_import(&self.binary_root, raw_path));
        let owner = module_owner(&self.module_dir, &path);
        self.modules.get(&path).cloned().map(|file| (owner, file))
    }
}

fn validate_module_graph(
    binary_root: &Path,
    modules: &HashMap<PathBuf, Arc<ModuleFile>>,
    errors: &mut Vec<ModuleCompileError>,
) {
    let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (owner_path, file) in modules {
        let mut dependencies = Vec::new();
        for import in &file.imports {
            let (raw_path, requested_export) = match import_base(import) {
                ImportTarget::Custom(path) => (path.as_str(), None),
                ImportTarget::CustomFunction { path, function } => {
                    (path.as_str(), Some(function.as_str()))
                }
                _ => continue,
            };
            let resolved = normalize(&resolve_custom_import(binary_root, raw_path));
            let Some(target) = modules.get(&resolved) else {
                errors.push(ModuleCompileError {
                    code: "MOD2001",
                    path: owner_path.clone(),
                    line: 1,
                    column: 1,
                    message: format!(
                        "module import {raw_path:?} resolves to missing file {}",
                        resolved.display()
                    ),
                });
                continue;
            };
            if let Some(export) = requested_export {
                if !target.exports.iter().any(|candidate| candidate == export) {
                    errors.push(ModuleCompileError {
                        code: "MOD2011",
                        path: owner_path.clone(),
                        line: 1,
                        column: 1,
                        message: format!(
                            "module import {raw_path:?} requests missing export {export:?}"
                        ),
                    });
                }
            }
            dependencies.push(resolved);
        }
        graph.insert(owner_path.clone(), dependencies);
    }
    detect_cycles(&graph, errors);
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
    linked_package_imports: bool,
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

        match import_base(import) {
            ImportTarget::Builtin(module) if module == "video" => {
                errors.push(ModuleCompileError {
                    code: "MOD2010",
                    path: path.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: "Video Manager must be imported as `vm` or `video-manager`; legacy `video` is not a capability".into(),
                });
            }
            ImportTarget::BuiltinFunction { module, .. } if module == "video" => {
                errors.push(ModuleCompileError {
                    code: "MOD2010",
                    path: path.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: "Video Manager must be imported as `vm` or `video-manager`; legacy `video` is not a capability".into(),
                });
            }
            ImportTarget::BuiltinSubLibrary { module, library }
                if (module == "crypto" && library != "argon")
                    || (module != "crypto" && !linked_package_imports) =>
            {
                errors.push(ModuleCompileError {
                    code: "MOD2012",
                    path: path.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: format!(
                        "unknown builtin sub-library {library:?} from {module:?}; supported local builtin: argon from crypto"
                    ),
                });
            }
            _ => {}
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
        ImportTarget::BuiltinSubLibrary { module, library } => {
            format!("builtin:{module}/{library}")
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

fn module_owner(module_dir: &Path, path: &Path) -> String {
    let root = normalize(module_dir);
    let relative = path.strip_prefix(&root).unwrap_or(path).with_extension("");
    canonical_module_owner(&relative)
}

/// Canonical capability principal for one linked Module REL logical name.
/// RELC uses this exact function so propagated host authority has the same
/// owner identity that ModuleExecutor supplies to VideoLanguage at runtime.
pub(crate) fn module_owner_from_logical_name(logical_name: &str) -> String {
    let parts = logical_name
        .split('/')
        .map(encode_owner_segment)
        .collect::<Vec<_>>();
    canonical_module_owner_parts(parts)
}

fn canonical_module_owner(relative: &Path) -> String {
    let parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                Some(encode_owner_segment(&value.to_string_lossy()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    canonical_module_owner_parts(parts)
}

fn canonical_module_owner_parts(parts: Vec<String>) -> String {
    if parts.is_empty() {
        "root".into()
    } else {
        parts.join(".")
    }
}

fn encode_owner_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    if value.is_empty() {
        return "_00".into();
    }
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        // `.` is reserved exclusively as the path-component separator and `_`
        // is reserved as the escape prefix. Escaping both makes this mapping
        // injective instead of allowing `a/b`, `a.b`, and literal `_HH`
        // spellings to collapse onto one capability principal.
        if byte.is_ascii_alphanumeric() || *byte == b'-' {
            out.push(*byte as char);
        } else {
            out.push('_');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
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
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::relc::{
        PackageExportLink, PackageLinkContext, PackageRootLink, PACKAGE_LINK_FORMAT,
    };

    use super::*;

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
    fn rejects_direct_import_of_missing_module_export() {
        let root = root();
        fs::write(
            root.join("module/b.module"),
            "export function present() { return true; }",
        )
        .unwrap();
        fs::write(
            root.join("module/a.module"),
            ":import[\"./module/b\".missing as missing]\nexport function run() { return true; }",
        )
        .unwrap();
        let errors = ModuleProgram::load(&root.join("module"))
            .expect_err("direct imports must reference a real export");
        assert!(errors.0.iter().any(|error| error.code == "MOD2011"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_loader_rejects_unlinked_package_imports() {
        let root = root();
        fs::write(
            root.join("module/package.module"),
            ":import[request from advancenet]\nexport function run() { return true; }",
        )
        .unwrap();
        let errors = ModuleProgram::load(&root.join("module"))
            .expect_err("filesystem loading has no verified package-link context");
        assert!(errors.0.iter().any(|error| error.code == "MOD2012"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_image_accepts_relc_verified_package_imports() {
        let package_links = PackageLinkContext {
            format: PACKAGE_LINK_FORMAT,
            roots: BTreeMap::from([(
                "advancenet".into(),
                PackageRootLink {
                    version: "2.0.0".into(),
                    artifact_sha256: "a".repeat(64),
                    exports: BTreeMap::from([(
                        "request".into(),
                        PackageExportLink {
                            entry: "components/request/request.ts".into(),
                            language: "typescript".into(),
                        },
                    )]),
                },
            )]),
        };
        let source = crate::PhysicalRelSource::new(
            crate::RelSourceKind::Module,
            "consumer",
            "module/consumer.module",
            ":import[request from advancenet]\nexport function run() { return true; }",
        );
        let image = crate::relc::compile_runtime_image_with_packages(
            "server Main {}",
            vec![source],
            &serde_json::json!({}),
            &package_links,
        )
        .expect("RELC should link the explicit package root");

        let program =
            ModuleProgram::from_runtime_image_with_services(&image, &ServiceInterfaces::new())
                .expect(
                    "linked Runtime Image must not be rejected as an unknown builtin sub-library",
                );
        assert_eq!(program.len(), 1);
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

    #[test]
    fn scoped_resolution_uses_canonical_nested_module_identity() {
        let root = root();
        fs::create_dir_all(root.join("module/learning")).unwrap();
        fs::write(
            root.join("module/learning/catalog.module"),
            "export function run() { return true; }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let (owner, _) = program
            .resolve_scoped("./module/learning/catalog")
            .expect("nested module should resolve");
        assert_eq!(owner, "learning.catalog");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn linked_logical_name_uses_same_canonical_owner_as_runtime_resolution() {
        assert_eq!(
            module_owner_from_logical_name("learning/catalog"),
            "learning.catalog"
        );
        assert_eq!(
            module_owner_from_logical_name("user data/auth"),
            "user_20data.auth"
        );
    }

    #[test]
    fn canonical_module_owner_does_not_collapse_separator_or_escape_spellings() {
        let nested = module_owner_from_logical_name("accounts/cache");
        let dotted = module_owner_from_logical_name("accounts.cache");
        let escaped_literal = module_owner_from_logical_name("accounts_2Ecache");
        assert_eq!(nested, "accounts.cache");
        assert_eq!(dotted, "accounts_2Ecache");
        assert_eq!(escaped_literal, "accounts_5F2Ecache");
        assert_ne!(nested, dotted);
        assert_ne!(dotted, escaped_literal);
        assert_ne!(nested, escaped_literal);
    }

    #[test]
    fn canonical_module_owner_preserves_empty_logical_path_segments() {
        assert_ne!(
            module_owner_from_logical_name("accounts//cache"),
            module_owner_from_logical_name("accounts/cache")
        );
        assert_eq!(
            module_owner_from_logical_name("accounts//cache"),
            "accounts._00.cache"
        );
    }

    #[test]
    fn rejects_legacy_video_capability_name_at_boot() {
        let root = root();
        fs::write(
            root.join("module/video.module"),
            ":import[video]\nexport function run() { return true; }",
        )
        .unwrap();
        let errors = ModuleProgram::load(&root.join("module"))
            .expect_err("legacy video capability name must fail module boot");
        assert!(errors.0.iter().any(|error| error.code == "MOD2010"));
        let _ = fs::remove_dir_all(root);
    }
}
