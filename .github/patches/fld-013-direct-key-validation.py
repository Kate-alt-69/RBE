from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


field_path = Path("engine/crates/route-engine/src/field_manager.rs")
text = field_path.read_text(encoding="utf-8")
old = '''    fn call_direct(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        let key = string_arg(args.first(), "FieldManager key/prefix")?;
        match function {
            "has" => {
                require_arity(function, args, 1, 1)?;
                Ok(Value::Bool(self.query.contains_key(key)))
            }
            "required" => {
                require_arity(function, args, 1, 1)?;
                self.query.get(key).cloned().ok_or_else(|| {
                    field_module_error(format!("required query field {key:?} is missing"))
                })
            }
            "optional" => {
                require_arity(function, args, 1, 1)?;
                Ok(self.query.get(key).cloned().unwrap_or(Value::Null))
            }
            "dynamic" => {
                require_arity(function, args, 1, 2)?;
'''
new = '''    fn call_direct(&self, function: &str, args: &[Value]) -> Result<Value, ModuleEvalError> {
        if function == "dynamic" {
            require_arity(function, args, 1, 2)?;
        } else {
            require_arity(function, args, 1, 1)?;
        }
        let key = string_arg(args.first(), "FieldManager key/prefix")?;
        if key.is_empty() {
            return Err(field_module_error(
                "FieldManager key/prefix must be a non-empty string",
            ));
        }
        match function {
            "has" => Ok(Value::Bool(self.query.contains_key(key))),
            "required" => {
                self.query.get(key).cloned().ok_or_else(|| {
                    field_module_error(format!("required query field {key:?} is missing"))
                })
            }
            "optional" => Ok(self.query.get(key).cloned().unwrap_or(Value::Null)),
            "dynamic" => {
'''
text = replace_once(text, old, new, "direct helper validation")

insert = r'''

    #[test]
    fn direct_field_helpers_reject_empty_keys_and_prefixes() {
        let context = FieldRuntimeContext {
            query: HashMap::new(),
            resolved: HashMap::new(),
            allowed_resolvers: HashSet::new(),
            direct_enabled: true,
        };
        for function in ["has", "required", "optional", "dynamic"] {
            let error = context
                .call(function, &[Value::String(String::new())])
                .unwrap_err();
            assert!(error.message.contains("non-empty"), "{function}: {}", error.message);
        }
    }

    #[test]
    fn direct_field_helpers_validate_arity_before_key_type() {
        let context = FieldRuntimeContext {
            query: HashMap::new(),
            resolved: HashMap::new(),
            allowed_resolvers: HashSet::new(),
            direct_enabled: true,
        };
        let error = context.call("required", &[]).unwrap_err();
        assert!(error.message.contains("argument"));
    }
'''
last_close = text.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs final module close not found")
text = text[:last_close] + insert + text[last_close:]
field_path.write_text(text, encoding="utf-8")

doc_path = Path("doc/field-manager.md")
doc = doc_path.read_text(encoding="utf-8")
old_doc = '''`:import[field]` enables the direct request helpers over the same snapshot:
'''
new_doc = '''`:import[field]` enables the direct request helpers over the same snapshot. Direct helper keys/prefixes must be non-empty strings, matching declarative FieldManager identity rules:
'''
doc = replace_once(doc, old_doc, new_doc, "direct helper docs")
doc_path.write_text(doc, encoding="utf-8")
