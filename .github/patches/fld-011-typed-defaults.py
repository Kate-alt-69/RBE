from pathlib import Path

PARSER = Path("engine/crates/route-engine/src/parser.rs")
FIELD = Path("engine/crates/route-engine/src/field_manager.rs")
DOC = Path("doc/field-manager.md")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


parser = PARSER.read_text(encoding="utf-8")

needle = '''    fn parse_field_type(&mut self) -> Result<FieldValueType, ParseError> {\n        match self.expect_ident()?.as_str() {\n            "string" => Ok(FieldValueType::String),\n            "int" => Ok(FieldValueType::Int),\n            "bool" => Ok(FieldValueType::Bool),\n            other => Err(self.error_here(&format!(\n                "unknown FieldManager type {other:?}; expected string, int, or bool"\n            ))),\n        }\n    }\n\n'''
helper = needle + '''    fn validate_field_default(\n        &self,\n        value_type: FieldValueType,\n        value: &Value,\n    ) -> Result<(), ParseError> {\n        let valid = match (value_type, value) {\n            (_, Value::Null) => true,\n            (FieldValueType::String, Value::String(_)) => true,\n            (FieldValueType::Bool, Value::Bool(_)) => true,\n            (FieldValueType::Int, Value::Number(value)) => {\n                value.is_finite()\n                    && value.fract() == 0.0\n                    && *value >= i64::MIN as f64\n                    && *value <= i64::MAX as f64\n            }\n            _ => false,\n        };\n        if valid {\n            return Ok(());\n        }\n\n        let expected = match value_type {\n            FieldValueType::String => "string or null",\n            FieldValueType::Int => "integer or null",\n            FieldValueType::Bool => "boolean or null",\n        };\n        Err(self.error_here(&format!(\n            "FieldManager default does not match declared type; expected {expected}"\n        )))\n    }\n\n'''
parser = replace_once(parser, needle, helper, "insert typed default validator")

old = '''        } else {\n            if seen_options.contains("stripPrefix") {\n                return Err(self.error_here("stripPrefix is only valid for dynamic(...)"));\n            }\n            if seen_options.contains("maxMatches") {\n                return Err(self.error_here("maxMatches is only valid for dynamic(...)"));\n            }\n        }\n        Ok(FieldBinding {\n'''
new = '''        } else {\n            if seen_options.contains("stripPrefix") {\n                return Err(self.error_here("stripPrefix is only valid for dynamic(...)"));\n            }\n            if seen_options.contains("maxMatches") {\n                return Err(self.error_here("maxMatches is only valid for dynamic(...)"));\n            }\n        }\n        if mode == FieldBindingMode::Optional {\n            if let Some(default) = default.as_ref() {\n                self.validate_field_default(value_type, default)?;\n            }\n        }\n        Ok(FieldBinding {\n'''
parser = replace_once(parser, old, new, "validate optional default after option parsing")
PARSER.write_text(parser, encoding="utf-8")

field = FIELD.read_text(encoding="utf-8")
insert = r'''

    #[test]
    fn optional_defaults_must_match_the_declared_type() {
        for binding in [
            r#"optional("page", type = int, default = "oops")"#,
            r#"optional("page", default = "oops", type = int)"#,
            r#"optional("page", type = int, default = 1.5)"#,
            r#"optional("enabled", type = bool, default = "false")"#,
            r#"optional("name", type = string, default = 1)"#,
        ] {
            let source = format!(
                ":import[field]\nfields {{ value = {binding}; }}\nclass Route {{ get(req) {{ return req.fields; }} }}"
            );
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_file().unwrap_err();
            assert!(error
                .message
                .contains("FieldManager default does not match declared type"));
        }
    }

    #[test]
    fn optional_defaults_accept_typed_constants_and_null() {
        let route = route(
            r#":import[field]
               fields {
                   page = optional("page", type = int, default = 1);
                   enabled = optional("enabled", type = bool, default = false);
                   name = optional("name", type = string, default = "guest");
                   missing = optional("missing", type = int, default = null);
               }
               class Route { get(req) { return req.fields; } }"#,
        );
        assert!(matches!(route.field_bindings[0].default, Some(Value::Number(value)) if value == 1.0));
        assert!(matches!(route.field_bindings[1].default, Some(Value::Bool(false))));
        assert!(matches!(route.field_bindings[2].default, Some(Value::String(ref value)) if value == "guest"));
        assert!(matches!(route.field_bindings[3].default, Some(Value::Null)));
    }
'''
last_close = field.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs: could not find final test module close")
field = field[:last_close] + insert + field[last_close:]
FIELD.write_text(field, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
needle = "Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option. Top-level `.field` metadata is strict too: each metadata key may appear once, and `required` / `optional` are mutually exclusive aliases rather than last-value-wins flags."
replacement = needle + " Optional defaults are checked against the final declared type at compile time (regardless of option order): string defaults must be strings, bool defaults booleans, and int defaults finite whole integers in range. `null` is accepted as an explicit no-value default for every optional type."
doc = replace_once(doc, needle, replacement, "document typed defaults")
DOC.write_text(doc, encoding="utf-8")
