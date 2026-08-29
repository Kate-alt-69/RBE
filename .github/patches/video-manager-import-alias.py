from pathlib import Path


def require_replace(source: str, old: str, new: str, label: str, count: int = 1) -> str:
    actual = source.count(old)
    if actual < count:
        raise SystemExit(f"{label} missing: expected at least {count}, found {actual}")
    return source.replace(old, new, count)


# Capability registry: expose only vm / video-manager, never legacy video.
path = Path("crates/route-engine/src/modules.rs")
source = path.read_text()
source = require_replace(source, "    Video,\n}", "    VideoManager,\n}", "Video enum anchor")

start = source.find('        "video" => matches!(')
if start < 0:
    raise SystemExit("video builtin function anchor missing")
end = source.find('        _ => false,', start)
if end < 0:
    raise SystemExit("builtin function match tail missing")
replacement = '''        "vm" | "video-manager" => matches!(
            function,
            "status"
                | "databaseHealth"
                | "database_health"
                | "get"
                | "create"
                | "queueDownload"
                | "queue_download"
        ),
'''
source = source[:start] + replacement + source[end:]

source = source.replace("BuiltinModule::Video", "BuiltinModule::VideoManager")
registry_anchor = '"video" => ModuleKind::Builtin(BuiltinModule::VideoManager),'
if source.count(registry_anchor) != 2:
    raise SystemExit("expected two legacy video registry anchors")
source = source.replace(
    registry_anchor,
    '"vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),',
)
source = require_replace(
    source,
    "requires the privileged module Video host capability",
    "requires the privileged module Video Manager host capability",
    "Video Manager registry message",
)

test_anchor = "    #[test]\n    fn private_health_reports_runtime_status() {"
tests = '''    #[test]
    fn video_manager_uses_only_explicit_supported_import_names() {
        assert!(builtin_function_exists("vm", "status"));
        assert!(builtin_function_exists("video-manager", "queueDownload"));
        assert!(!builtin_function_exists("video", "status"));
        assert!(!route_capability_allowed("vm"));
        assert!(!route_capability_allowed("video-manager"));
        assert!(!route_capability_allowed("video"));
    }

    #[test]
    fn private_health_reports_runtime_status() {'''
source = require_replace(source, test_anchor, tests, "modules test insertion anchor")
path.write_text(source)

# Host bridge accepts exactly the two public import names.
path = Path("crates/route-engine/src/video_host.rs")
source = path.read_text()
host_anchor = '            if module != "video" {\n                return Ok(None);\n            }'
host_replacement = '            if !matches!(module, "vm" | "video-manager") {\n                return Ok(None);\n            }'
source = require_replace(source, host_anchor, host_replacement, "video host module name anchor")
source = require_replace(
    source,
    'message: "video capability requires a resolved .module identity".into(),',
    'message: "Video Manager capability requires a resolved .module identity".into(),',
    "video host scope message",
)
path.write_text(source)

# Boot validation: legacy :import[video] must fail immediately with guidance.
path = Path("crates/route-engine/src/module_runtime.rs")
source = path.read_text()
anchor = '        let Some(services) = services else {\n            continue;\n        };\n        match import_base(import) {'
replacement = '''        match import_base(import) {
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
            _ => {}
        }

        let Some(services) = services else {
            continue;
        };
        match import_base(import) {'''
source = require_replace(source, anchor, replacement, "module runtime service validation anchor")
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("module runtime tests tail missing")
source = source[:tests_end] + r'''

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
''' + source[tests_end:]
path.write_text(source)

# Evaluator regression: host capabilities are import-gated, never global.
path = Path("crates/route-engine/src/module_eval.rs")
source = path.read_text()
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("module evaluator tests tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn host_capability_is_not_global_without_import() {
        let root = root();
        fs::write(
            root.join("module/unimported.module"),
            "export function run(value) { return host.double(value); }",
        )
        .unwrap();
        let program = ModuleProgram::load(&root.join("module")).unwrap();
        let executor =
            ModuleExecutor::with_host_capabilities(&program, Arc::new(TestHostCapability));
        let error = block_on_ready(executor.call(
            "./module/unimported",
            "run",
            vec![Value::Number(6.0)],
        ))
        .expect_err("host capability must require an explicit import");
        assert_eq!(error.code, "MOD3201");
        let _ = fs::remove_dir_all(root);
    }
''' + source[tests_end:]
path.write_text(source)

# Parser regression: both public import names parse, including hyphenated long form.
path = Path("crates/route-engine/src/lib.rs")
source = path.read_text()
anchor = "    #[test]\n    fn parses_executable_service_program() {"
test = r'''    #[test]
    fn parses_video_manager_import_names_for_modules() {
        let tokens = Lexer::new(
            r#":import[vm as short, video-manager as media, video-manager.status as status]
            export function run() { return status(); }"#,
        )
        .tokenize()
        .expect("lex failed");
        let file = Parser::new(tokens)
            .parse_module_file()
            .expect("module parse failed");
        assert_eq!(file.imports.len(), 3);
        assert_eq!(binding_name(&file.imports[0]), "short");
        assert_eq!(binding_name(&file.imports[1]), "media");
        assert_eq!(binding_name(&file.imports[2]), "status");
    }

    #[test]
    fn parses_executable_service_program() {'''
source = require_replace(source, anchor, test, "route-engine parser test insertion anchor")
path.write_text(source)
