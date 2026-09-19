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
