from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one occurrence, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    "Err(message) if file.directive.optional => Value::Null,",
    "Err(_) if file.directive.optional => Value::Null,",
    "optional Field resolver ignored error binding",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    "Err(message) if binding.mode == FieldBindingMode::Optional => Ok(Value::Null),",
    "Err(_) if binding.mode == FieldBindingMode::Optional => Ok(Value::Null),",
    "optional Field binding ignored error binding",
)

replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    '''    if let (Some(snapshot), Some(fields)) = (request_snapshot.as_mut(), field_context.as_ref()) {
        if let Value::Object(request) = snapshot {
            request.insert("fields".into(), fields.resolved_object());
        }
    }
''',
    '''    if let (Some(Value::Object(request)), Some(fields)) =
        (request_snapshot.as_mut(), field_context.as_ref())
    {
        request.insert("fields".into(), fields.resolved_object());
    }
''',
    "Field request snapshot collapsible match",
)

field_manager = Path("engine/crates/route-engine/src/field_manager.rs")
text = field_manager.read_text(encoding="utf-8")

runtime_image_import = "use crate::runtime_image::RuntimeImage;\n"
if text.count(runtime_image_import) != 1:
    raise SystemExit(
        f"FieldManager obsolete RuntimeImage test import: expected one occurrence, found {text.count(runtime_image_import)}"
    )
text = text.replace(runtime_image_import, "", 1)

request_helper = '''    fn request(entries: &[(&str, &str)]) -> Value {
        Value::Object(HashMap::from([(
            "query".into(),
            Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).into(), Value::String((*value).into())))
                    .collect(),
            ),
        )]))
    }
'''
empty_program_helper = request_helper + '''
    fn empty_program(name: &str) -> ModuleProgram {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-field-{name}-test-{}-{nonce}",
            std::process::id()
        ));
        ModuleProgram::load(&root.join("module")).expect("module load failed")
    }
'''
if text.count(request_helper) != 1:
    raise SystemExit(
        f"FieldManager request test helper: expected one occurrence, found {text.count(request_helper)}"
    )
text = text.replace(request_helper, empty_program_helper, 1)

obsolete_program = "let program = ModuleProgram::from_runtime_image(&RuntimeImage::test_empty()).unwrap();"
if text.count(obsolete_program) != 2:
    raise SystemExit(
        f"FieldManager obsolete test ModuleProgram constructor: expected two occurrences, found {text.count(obsolete_program)}"
    )
text = text.replace(obsolete_program, 'let program = empty_program("resolver");', 1)
text = text.replace(obsolete_program, 'let program = empty_program("failure-contract");', 1)
field_manager.write_text(text, encoding="utf-8")
