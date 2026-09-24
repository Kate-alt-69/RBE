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

parser = replace_once(
    parser,
    '''        let mut directive = FieldDirective {\n            source: "query".into(),\n            key: None,\n            optional: false,\n            value_type: FieldValueType::String,\n        };\n        while !self.check(&TokenKind::RBracket) {\n            let name = self.expect_ident()?;\n            self.expect(TokenKind::Eq)?;\n            self.apply_field_directive_option(&mut directive, &name)?;\n''',
    '''        let mut directive = FieldDirective {\n            source: "query".into(),\n            key: None,\n            optional: false,\n            value_type: FieldValueType::String,\n        };\n        let mut seen_options = HashSet::new();\n        while !self.check(&TokenKind::RBracket) {\n            let name = self.expect_ident()?;\n            self.register_field_directive_option(&mut seen_options, &name)?;\n            self.expect(TokenKind::Eq)?;\n            self.apply_field_directive_option(&mut directive, &name)?;\n''',
    "bracket field metadata tracking",
)

parser = replace_once(
    parser,
    '''        let mut directive = FieldDirective {\n            source: "query".into(),\n            key: None,\n            optional: false,\n            value_type: FieldValueType::String,\n        };\n        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {\n            let option = self.expect_ident()?;\n            self.expect(TokenKind::Eq)?;\n            self.apply_field_directive_option(&mut directive, &option)?;\n''',
    '''        let mut directive = FieldDirective {\n            source: "query".into(),\n            key: None,\n            optional: false,\n            value_type: FieldValueType::String,\n        };\n        let mut seen_options = HashSet::new();\n        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {\n            let option = self.expect_ident()?;\n            self.register_field_directive_option(&mut seen_options, &option)?;\n            self.expect(TokenKind::Eq)?;\n            self.apply_field_directive_option(&mut directive, &option)?;\n''',
    "block field metadata tracking",
)

needle = '''    fn apply_field_directive_option(\n        &mut self,\n        directive: &mut FieldDirective,\n        name: &str,\n    ) -> Result<(), ParseError> {\n'''
helper = '''    fn register_field_directive_option(\n        &self,\n        seen_options: &mut HashSet<String>,\n        name: &str,\n    ) -> Result<(), ParseError> {\n        if !seen_options.insert(name.to_string()) {\n            return Err(self.error_here(&format!(\n                "duplicate FieldManager metadata option {name:?}"\n            )));\n        }\n        if (name == "required" && seen_options.contains("optional"))\n            || (name == "optional" && seen_options.contains("required"))\n        {\n            return Err(self.error_here(\n                "FieldManager metadata cannot declare both `required` and `optional`",\n            ));\n        }\n        Ok(())\n    }\n\n'''
parser = replace_once(parser, needle, helper + needle, "field metadata registrar")
PARSER.write_text(parser, encoding="utf-8")

field = FIELD.read_text(encoding="utf-8")
insert = r'''

    #[test]
    fn field_metadata_rejects_duplicate_options_in_both_syntaxes() {
        for source in [
            r#":field[source = query, source = body, key = "value"]"#,
            r#"field {
                   source = query;
                   source = body;
                   key = "value";
               }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_field_file().unwrap_err();
            assert!(error
                .message
                .contains("duplicate FieldManager metadata option"));
            assert!(error.message.contains("source"));
        }
    }

    #[test]
    fn field_metadata_rejects_required_optional_alias_collisions() {
        for source in [
            r#":field[required = true, optional = false, key = "value"]"#,
            r#"field {
                   optional = true;
                   required = false;
                   key = "value";
               }"#,
        ] {
            let tokens = Lexer::new(source).tokenize().unwrap();
            let error = Parser::new(tokens).parse_field_file().unwrap_err();
            assert!(error
                .message
                .contains("cannot declare both `required` and `optional`"));
        }
    }
'''
last_close = field.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs: could not find final test module close")
field = field[:last_close] + insert + field[last_close:]
FIELD.write_text(field, encoding="utf-8")

doc = DOC.read_text(encoding="utf-8")
needle = "Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option."
replacement = needle + " Top-level `.field` metadata is strict too: each metadata key may appear once, and `required` / `optional` are mutually exclusive aliases rather than last-value-wins flags."
doc = replace_once(doc, needle, replacement, "FieldManager strict metadata docs")
DOC.write_text(doc, encoding="utf-8")
