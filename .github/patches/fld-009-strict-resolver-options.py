from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


parser_path = Path("engine/crates/route-engine/src/parser.rs")
parser = parser_path.read_text(encoding="utf-8")
parser = replace_once(
    parser,
    "use std::collections::HashMap;\n",
    "use std::collections::{HashMap, HashSet};\n",
    "parser collection imports",
)
parser = replace_once(
    parser,
    """        let mut default = None;\n        let mut strip_prefix = false;\n        let mut max_matches = DEFAULT_DYNAMIC_FIELD_MATCHES;\n        while self.check(&TokenKind::Comma) {\n            self.advance();\n            let option = self.expect_ident()?;\n            self.expect(TokenKind::Eq)?;\n""",
    """        let mut default = None;\n        let mut strip_prefix = false;\n        let mut max_matches = DEFAULT_DYNAMIC_FIELD_MATCHES;\n        let mut seen_options = HashSet::new();\n        while self.check(&TokenKind::Comma) {\n            self.advance();\n            let option = self.expect_ident()?;\n            self.expect(TokenKind::Eq)?;\n            if !seen_options.insert(option.clone()) {\n                return Err(self.error_here(&format!(\n                    \"duplicate FieldManager resolver option {option:?}\"\n                )));\n            }\n""",
    "resolver option duplicate tracking",
)
parser = replace_once(
    parser,
    """        if mode == FieldBindingMode::Required && default.is_some() {\n            return Err(self.error_here(\"required(...) cannot declare a default\"));\n        }\n        if mode == FieldBindingMode::Dynamic\n            && (default.is_some() || value_type != FieldValueType::String)\n        {\n            return Err(self.error_here(\n                \"dynamic(...) supports stripPrefix and maxMatches only; dynamic values stay strings\",\n            ));\n        }\n        if mode != FieldBindingMode::Dynamic && max_matches != DEFAULT_DYNAMIC_FIELD_MATCHES {\n            return Err(self.error_here(\"maxMatches is only valid for dynamic(...)\"));\n        }\n""",
    """        if mode == FieldBindingMode::Required && seen_options.contains(\"default\") {\n            return Err(self.error_here(\"required(...) cannot declare a default\"));\n        }\n        if mode == FieldBindingMode::Dynamic {\n            if seen_options.contains(\"default\") || seen_options.contains(\"type\") {\n                return Err(self.error_here(\n                    \"dynamic(...) supports source, stripPrefix, and maxMatches only; dynamic values stay strings\",\n                ));\n            }\n        } else {\n            if seen_options.contains(\"stripPrefix\") {\n                return Err(self.error_here(\"stripPrefix is only valid for dynamic(...)\"));\n            }\n            if seen_options.contains(\"maxMatches\") {\n                return Err(self.error_here(\"maxMatches is only valid for dynamic(...)\"));\n            }\n        }\n""",
    "mode-specific resolver option validation",
)
parser_path.write_text(parser, encoding="utf-8")

field_path = Path("engine/crates/route-engine/src/field_manager.rs")
field = field_path.read_text(encoding="utf-8")
insert = r'''

    #[test]
    fn resolver_options_fail_closed_when_used_by_the_wrong_mode() {
        let error_for = |binding: &str| {
            let source = format!(
                ":import[field]\nfields {{ value = {binding}; }}\nclass Route {{ get(req) {{ return req.fields; }} }}"
            );
            let tokens = Lexer::new(&source).tokenize().unwrap();
            Parser::new(tokens).parse_file().unwrap_err()
        };

        let error = error_for(r#"optional("page", maxMatches = 64)"#);
        assert!(error.message.contains("maxMatches is only valid for dynamic"));

        let error = error_for(r#"required("page", stripPrefix = false)"#);
        assert!(error.message.contains("stripPrefix is only valid for dynamic"));

        let error = error_for(r#"dynamic("utm_", type = string)"#);
        assert!(error.message.contains("dynamic(...) supports source"));
    }

    #[test]
    fn resolver_options_reject_duplicates_instead_of_last_value_wins() {
        let source = r#":import[field]
            fields { value = optional("page", source = query, source = body); }
            class Route { get(req) { return req.fields; } }"#;
        let tokens = Lexer::new(source).tokenize().unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error.message.contains("duplicate FieldManager resolver option"));
        assert!(error.message.contains("source"));
    }
'''
last_close = field.rfind("\n}")
if last_close == -1:
    raise SystemExit("field_manager.rs: could not find final module close")
field = field[:last_close] + insert + field[last_close:]
field_path.write_text(field, encoding="utf-8")

doc_path = Path("doc/field-manager.md")
doc = doc_path.read_text(encoding="utf-8")
anchor = """The block is intentionally declarative: it reuses the same `required(...)`, `optional(...)`, `dynamic(...)`, coercion, default, and structured failure behavior as reusable `.field` files. It requires the direct `:import[field]` namespace import, may appear once per Route, and cannot reuse the reserved direct helper names `required`, `optional`, `has`, or `dynamic`.\n"""
replacement = anchor + """Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option.\n"""
doc = replace_once(doc, anchor, replacement, "strict resolver option docs")
doc_path.write_text(doc, encoding="utf-8")
