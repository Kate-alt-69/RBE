from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


parser_path = Path("engine/crates/route-engine/src/parser.rs")
text = parser_path.read_text(encoding="utf-8")

old = '''        if mode == FieldBindingMode::Dynamic {
            if seen_options.contains("default") || seen_options.contains("type") {
                return Err(self.error_here(
                    "dynamic(...) supports source, stripPrefix, and maxMatches only; dynamic values stay strings",
                ));
            }
        } else {
            if seen_options.contains("stripPrefix") {
                return Err(self.error_here("stripPrefix is only valid for dynamic(...)"));
            }
            if seen_options.contains("maxMatches") {
                return Err(self.error_here("maxMatches is only valid for dynamic(...)"));
            }
        }
        Ok(FieldBinding {
'''
new = '''        if mode == FieldBindingMode::Dynamic {
            if seen_options.contains("default") || seen_options.contains("type") {
                return Err(self.error_here(
                    "dynamic(...) supports source, stripPrefix, and maxMatches only; dynamic values stay strings",
                ));
            }
        } else {
            if seen_options.contains("stripPrefix") {
                return Err(self.error_here("stripPrefix is only valid for dynamic(...)"));
            }
            if seen_options.contains("maxMatches") {
                return Err(self.error_here("maxMatches is only valid for dynamic(...)"));
            }
        }
        if let Some(default) = default.as_ref() {
            self.validate_field_default(default, value_type)?;
        }
        Ok(FieldBinding {
'''
text = replace_once(text, old, new, "binding validation insertion")

marker = '''    pub fn parse_service_file(mut self) -> Result<ServiceProgram, ParseError> {
'''
helper = '''    fn validate_field_default(
        &self,
        default: &Value,
        value_type: FieldValueType,
    ) -> Result<(), ParseError> {
        let valid = match (value_type, default) {
            (_, Value::Null) => true,
            (FieldValueType::String, Value::String(_)) => true,
            (FieldValueType::Bool, Value::Bool(_)) => true,
            (FieldValueType::Int, Value::Number(value)) => {
                value.is_finite()
                    && value.fract() == 0.0
                    && *value >= i64::MIN as f64
                    && *value <= i64::MAX as f64
            }
            _ => false,
        };
        if valid {
            return Ok(());
        }
        let expected = match value_type {
            FieldValueType::String => "string or null",
            FieldValueType::Int => "integer or null",
            FieldValueType::Bool => "boolean or null",
        };
        Err(self.error_here(&format!(
            "FieldManager default does not match declared type; expected {expected}"
        )))
    }

'''
if marker not in text:
    raise SystemExit("service parser marker not found")
text = text.replace(marker, helper + marker, 1)
parser_path.write_text(text, encoding="utf-8")

field_path = Path("engine/crates/route-engine/src/field_manager.rs")
field = field_path.read_text(encoding="utf-8")
insert = r'''

    #[test]
    fn route_local_defaults_must_match_declared_types_regardless_of_option_order() {
        for source in [
            r#":import[field]
               fields { page = optional("page", type = int, default = "oops"); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { page = optional("page", default = "oops", type = int); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { debug = optional("debug", type = bool, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
            r#":import[field]
               fields { label = optional("label", type = string, default = false); }
               class Route { get(req) { return req.fields; } }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_file().unwrap_err();
            assert!(error.message.contains("default"));
            assert!(error.message.contains("declared type"));
        }
    }

    #[test]
    fn route_local_defaults_accept_matching_values_and_null() {
        let parsed = route(
            r#":import[field]
               fields {
                   page = optional("page", type = int, default = 1);
                   debug = optional("debug", type = bool, default = false);
                   label = optional("label", type = string, default = "ok");
                   nullable = optional("nullable", type = int, default = null);
               }
               class Route { get(req) { return req.fields; } }"#,
        );
        assert!(matches!(parsed.field_bindings[0].default, Some(Value::Number(value)) if value == 1.0));
        assert!(matches!(parsed.field_bindings[1].default, Some(Value::Bool(false))));
        assert!(matches!(parsed.field_bindings[2].default, Some(Value::String(ref value)) if value == "ok"));
        assert!(matches!(parsed.field_bindings[3].default, Some(Value::Null)));
    }
'''
last_close = field.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs final module close not found")
field = field[:last_close] + insert + field[last_close:]
field_path.write_text(field, encoding="utf-8")

doc_path = Path("doc/field-manager.md")
doc = doc_path.read_text(encoding="utf-8")
old_doc = '''Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option. Top-level `.field` metadata is strict too: each metadata key may appear once, and `required` / `optional` are mutually exclusive aliases rather than last-value-wins flags.
'''
new_doc = '''Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option. Optional defaults are type-checked at compile time after all options are parsed, so option order cannot bypass validation; a default must match the declared string/int/bool type or be `null`. Top-level `.field` metadata is strict too: each metadata key may appear once, and `required` / `optional` are mutually exclusive aliases rather than last-value-wins flags.
'''
doc = replace_once(doc, old_doc, new_doc, "FieldManager typed-default docs")
doc_path.write_text(doc, encoding="utf-8")
