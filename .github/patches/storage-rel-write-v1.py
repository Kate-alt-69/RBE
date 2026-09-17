from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# Lexer: $$/<relative-path> is a symbolic project-root path token. It is never
# expanded to a host path here; trusted Container Rust resolves it later.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """    Dollar,\n    Import,""",
    """    Dollar,\n    ProjectPath(String),\n    Import,""",
    "lexer ProjectPath token",
)

replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """                '$' => {\n                    self.bump();\n                    TokenKind::Dollar\n                }""",
    """                '$' => {\n                    self.bump();\n                    if self.take_if('$') {\n                        if !self.take_if('/') {\n                            return Err(LexError {\n                                message: \"project-root path must start with $$/\".into(),\n                                line,\n                                column,\n                            });\n                        }\n                        let mut path = String::from(\"$$/\");\n                        while let Some(&next) = self.chars.peek() {\n                            if next.is_whitespace()\n                                || matches!(next, ']' | ',' | ')' | ';' | '}')\n                            {\n                                break;\n                            }\n                            path.push(next);\n                            self.bump();\n                        }\n                        if path == \"$$/\" {\n                            return Err(LexError {\n                                message: \"project-root path must name a relative target\".into(),\n                                line,\n                                column,\n                            });\n                        }\n                        TokenKind::ProjectPath(path)\n                    } else {\n                        TokenKind::Dollar\n                    }\n                }""",
    "lexer project-root path scanning",
)

replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """    fn reserved_word_is_identifier_after_dot() {\n        let tokens = Lexer::new(\"module.import(); video.delete();\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"import\")));\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"delete\")));\n    }\n}""",
    """    fn reserved_word_is_identifier_after_dot() {\n        let tokens = Lexer::new(\"module.import(); video.delete();\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"import\")));\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"delete\")));\n    }\n\n    #[test]\n    fn project_root_path_stays_symbolic() {\n        let tokens = Lexer::new(\"write[$$/generated/data.json]\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens.iter().any(|token| {\n            matches!(&token.kind, TokenKind::ProjectPath(path) if path == \"$$/generated/data.json\")\n        }));\n    }\n}""",
    "lexer project-root test",
)

# ---------------------------------------------------------------------------
# Parser: descriptor brackets are Storage-write-specific syntax represented
# with ordinary AST objects. The complete call is normalized to the single
# object expected by the trusted Storage capability boundary.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """                TokenKind::LParen => {\n                    self.advance();\n                    let mut args = Vec::new();\n                    if !self.check(&TokenKind::RParen) {\n                        loop {\n                            args.push(self.parse_expression()?);\n                            if !self.check(&TokenKind::Comma) {\n                                break;\n                            }\n                            self.advance();\n                        }\n                    }\n                    self.expect(TokenKind::RParen)?;\n                    expr = Expr::Call(Box::new(expr), args);\n                }\n                _ => break,""",
    """                TokenKind::LParen => {\n                    self.advance();\n                    let mut args = Vec::new();\n                    if !self.check(&TokenKind::RParen) {\n                        loop {\n                            args.push(self.parse_expression()?);\n                            if !self.check(&TokenKind::Comma) {\n                                break;\n                            }\n                            self.advance();\n                        }\n                    }\n                    self.expect(TokenKind::RParen)?;\n                    args = normalize_storage_write_args(&expr, args)?;\n                    expr = Expr::Call(Box::new(expr), args);\n                }\n                TokenKind::LBracket => {\n                    let Expr::Ident(descriptor) = &expr else {\n                        break;\n                    };\n                    if !matches!(descriptor.as_str(), \"encode\" | \"data\" | \"write\" | \"level\") {\n                        break;\n                    }\n                    let descriptor = descriptor.clone();\n                    self.advance();\n                    let value = self.parse_expression()?;\n                    self.expect(TokenKind::RBracket)?;\n                    expr = Expr::Object(vec![\n                        (\"__rbeStorageDescriptor\".into(), Expr::String(descriptor)),\n                        (\"value\".into(), value),\n                    ]);\n                }\n                _ => break,""",
    "parser descriptor postfix",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """            TokenKind::Null => Ok(Expr::Null),\n            TokenKind::Ident(name) => Ok(Expr::Ident(name)),\n            TokenKind::Dollar => {""",
    """            TokenKind::Null => Ok(Expr::Null),\n            TokenKind::Ident(name) => Ok(Expr::Ident(name)),\n            TokenKind::ProjectPath(path) => Ok(Expr::String(path)),\n            TokenKind::Dollar => {""",
    "parser ProjectPath primary",
)

# Helper functions live outside Parser impl and only inspect syntax; they do not
# resolve $$ or perform any host I/O.
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """    fn peek_kind(&self) -> TokenKind {""",
    """    fn peek_kind(&self) -> TokenKind {""",
    "parser helper anchor presence",
)

path = Path("engine/crates/route-engine/src/parser.rs")
text = path.read_text(encoding="utf-8")
anchor = "\n    fn peek_kind(&self) -> TokenKind {"
pos = text.find(anchor)
if pos < 0:
    raise SystemExit("parser helper insertion anchor missing")
helpers = r'''

fn normalize_storage_write_args(callee: &Expr, args: Vec<Expr>) -> Result<Vec<Expr>, ParseError> {
    let is_write = matches!(callee, Expr::Ident(name) if name == "write")
        || matches!(callee, Expr::Member(base, function)
            if function == "write" && matches!(base.as_ref(), Expr::Ident(module) if module == "storage"));
    if !is_write {
        return Ok(args);
    }
    if args.len() != 4 {
        return Err(ParseError {
            message: "storage.write expects encode[], data[], write[], and level[] descriptors".into(),
            line: 0,
            column: 0,
        });
    }

    let mut encoding = None;
    let mut data = None;
    let mut path = None;
    let mut level = None;
    for arg in args {
        let Some((kind, value)) = storage_descriptor(arg) else {
            return Err(ParseError {
                message: "storage.write arguments must use descriptor brackets".into(),
                line: 0,
                column: 0,
            });
        };
        let slot = match kind.as_str() {
            "encode" => &mut encoding,
            "data" => &mut data,
            "write" => &mut path,
            "level" => &mut level,
            _ => unreachable!("descriptor parser is closed above"),
        };
        if slot.replace(value).is_some() {
            return Err(ParseError {
                message: format!("storage.write descriptor {kind}[] was provided more than once"),
                line: 0,
                column: 0,
            });
        }
    }

    let required = |value: Option<Expr>, name: &str| {
        value.ok_or_else(|| ParseError {
            message: format!("storage.write is missing {name}[]"),
            line: 0,
            column: 0,
        })
    };
    Ok(vec![Expr::Object(vec![
        ("path".into(), required(path, "write")?),
        ("data".into(), required(data, "data")?),
        ("encoding".into(), required(encoding, "encode")?),
        ("level".into(), required(level, "level")?),
    ])])
}

fn storage_descriptor(expr: Expr) -> Option<(String, Expr)> {
    let Expr::Object(mut fields) = expr else {
        return None;
    };
    if fields.len() != 2 {
        return None;
    }
    let value_index = fields.iter().position(|(key, _)| key == "value")?;
    let descriptor_index = fields
        .iter()
        .position(|(key, _)| key == "__rbeStorageDescriptor")?;
    let (_, descriptor) = fields.swap_remove(descriptor_index);
    let value_index = if descriptor_index < value_index {
        value_index - 1
    } else {
        value_index
    };
    let (_, value) = fields.swap_remove(value_index);
    let Expr::String(descriptor) = descriptor else {
        return None;
    };
    Some((descriptor, value))
}
'''
text = text[:pos] + helpers + text[pos:]
path.write_text(text, encoding="utf-8")

# ---------------------------------------------------------------------------
# RELC/Runtime Image: write is now an exact Storage operation and can be granted
# the same way read/list/snapshot/commit already are.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    'pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];',
    'pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 5] =\n    ["read", "list", "snapshot", "commit", "write"];',
    "Runtime Image Storage write allowlist",
)

# ---------------------------------------------------------------------------
# Public parser regression: descriptor syntax lowers to one normalized object
# while preserving the symbolic $$ path string.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    """    #[test]\n    fn parses_video_manager_import_names_for_modules() {""",
    """    #[test]\n    fn parses_storage_write_descriptors_into_one_capability_argument() {\n        let tokens = Lexer::new(\n            r#\":import[storage.write]\n            export function save() {\n                return write(\n                    encode[\"UTF8\"],\n                    data[{ ok: true }],\n                    write[$$/generated/data.json],\n                    level[1]\n                );\n            }\"#,\n        )\n        .tokenize()\n        .expect(\"lex failed\");\n        let file = Parser::new(tokens)\n            .parse_module_file()\n            .expect(\"module parse failed\");\n        let Statement::Return(Expr::Call(_, args)) = &file.functions[0].body[0] else {\n            panic!(\"expected normalized storage.write call\");\n        };\n        assert_eq!(args.len(), 1);\n        let Expr::Object(fields) = &args[0] else {\n            panic!(\"expected normalized descriptor object\");\n        };\n        assert!(fields.iter().any(|(key, value)| {\n            key == \"path\" && matches!(value, Expr::String(path) if path == \"$$/generated/data.json\")\n        }));\n    }\n\n    #[test]\n    fn parses_video_manager_import_names_for_modules() {""",
    "public storage.write parser regression",
)
