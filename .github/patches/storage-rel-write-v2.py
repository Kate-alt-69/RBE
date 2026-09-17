from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Lexer: keep $$/... symbolic. No host path is ever materialized here.
replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """    Dollar,\n    Import,""",
    """    Dollar,\n    ProjectPath(String),\n    Import,""",
    "ProjectPath token",
)
replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """                '$' => {\n                    self.bump();\n                    TokenKind::Dollar\n                }""",
    """                '$' => {\n                    self.bump();\n                    if self.take_if('$') {\n                        if !self.take_if('/') {\n                            return Err(LexError {\n                                message: \"project-root path must start with $$/\".into(),\n                                line,\n                                column,\n                            });\n                        }\n                        let mut path = String::from(\"$$/\");\n                        while let Some(&next) = self.chars.peek() {\n                            if next.is_whitespace()\n                                || matches!(next, ']' | ',' | ')' | ';' | '}')\n                            {\n                                break;\n                            }\n                            path.push(next);\n                            self.bump();\n                        }\n                        if path == \"$$/\" {\n                            return Err(LexError {\n                                message: \"project-root path must name a relative target\".into(),\n                                line,\n                                column,\n                            });\n                        }\n                        TokenKind::ProjectPath(path)\n                    } else {\n                        TokenKind::Dollar\n                    }\n                }""",
    "ProjectPath scanner",
)
replace_once(
    "engine/crates/route-engine/src/lexer.rs",
    """    fn reserved_word_is_identifier_after_dot() {\n        let tokens = Lexer::new(\"module.import(); video.delete();\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"import\")));\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"delete\")));\n    }\n}""",
    """    fn reserved_word_is_identifier_after_dot() {\n        let tokens = Lexer::new(\"module.import(); video.delete();\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"import\")));\n        assert!(tokens\n            .iter()\n            .any(|token| matches!(&token.kind, TokenKind::Ident(name) if name == \"delete\")));\n    }\n\n    #[test]\n    fn project_root_path_stays_symbolic() {\n        let tokens = Lexer::new(\"write[$$/generated/data.json]\")\n            .tokenize()\n            .unwrap();\n        assert!(tokens.iter().any(|token| {\n            matches!(&token.kind, TokenKind::ProjectPath(path) if path == \"$$/generated/data.json\")\n        }));\n    }\n}""",
    "ProjectPath lexer test",
)

# Parser: descriptor brackets are only syntax markers. They are deliberately
# NOT normalized based on the callee spelling; capability-aware lowering below
# decides whether the markers belong to the exact imported storage.write call.
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """                TokenKind::LParen => {\n                    self.advance();\n                    let mut args = Vec::new();\n                    if !self.check(&TokenKind::RParen) {\n                        loop {\n                            args.push(self.parse_expression()?);\n                            if !self.check(&TokenKind::Comma) {\n                                break;\n                            }\n                            self.advance();\n                        }\n                    }\n                    self.expect(TokenKind::RParen)?;\n                    expr = Expr::Call(Box::new(expr), args);\n                }\n                _ => break,""",
    """                TokenKind::LParen => {\n                    self.advance();\n                    let mut args = Vec::new();\n                    if !self.check(&TokenKind::RParen) {\n                        loop {\n                            args.push(self.parse_expression()?);\n                            if !self.check(&TokenKind::Comma) {\n                                break;\n                            }\n                            self.advance();\n                        }\n                    }\n                    self.expect(TokenKind::RParen)?;\n                    expr = Expr::Call(Box::new(expr), args);\n                }\n                TokenKind::LBracket => {\n                    let Expr::Ident(descriptor) = &expr else {\n                        break;\n                    };\n                    if !matches!(descriptor.as_str(), \"encode\" | \"data\" | \"write\" | \"level\") {\n                        break;\n                    }\n                    let descriptor = descriptor.clone();\n                    self.advance();\n                    let value = self.parse_expression()?;\n                    self.expect(TokenKind::RBracket)?;\n                    expr = Expr::Object(vec![\n                        (\"__rbeStorageDescriptor\".into(), Expr::String(descriptor)),\n                        (\"value\".into(), value),\n                    ]);\n                }\n                _ => break,""",
    "descriptor postfix syntax",
)
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    """            TokenKind::Null => Ok(Expr::Null),\n            TokenKind::Ident(name) => Ok(Expr::Ident(name)),\n            TokenKind::Dollar => {""",
    """            TokenKind::Null => Ok(Expr::Null),\n            TokenKind::Ident(name) => Ok(Expr::Ident(name)),\n            TokenKind::ProjectPath(path) => Ok(Expr::String(path)),\n            TokenKind::Dollar => {""",
    "ProjectPath primary",
)

# RELC/Runtime Image exact operation allowlist.
replace_once(
    "engine/crates/route-engine/src/runtime_image.rs",
    'pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];',
    'pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 5] =\n    ["read", "list", "snapshot", "commit", "write"];',
    "Storage write allowlist",
)

# WASM compiler: only the proven imported Storage `write` operation consumes
# descriptor markers. All other functions named write remain untouched.
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """    let args = host_args\n        .iter()\n        .map(|argument| static_json_with_bindings(argument, &bindings))\n        .collect::<Option<Vec<_>>>()\n        .ok_or_else(|| {\n            \"native linked Module host arguments must resolve to static JSON values\".to_string()\n        })?;""",
    """    let args = if host.kind == ContainerCapabilityKind::Storage && host.operation == \"write\" {\n        lower_storage_write_args(host_args, &bindings)?\n    } else {\n        host_args\n            .iter()\n            .map(|argument| static_json_with_bindings(argument, &bindings))\n            .collect::<Option<Vec<_>>>()\n            .ok_or_else(|| {\n                \"native linked Module host arguments must resolve to static JSON values\".to_string()\n            })?\n    };""",
    "capability-aware Storage write normalization",
)

replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {""",
    r'''fn lower_storage_write_args(
    args: &[Expr],
    bindings: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<serde_json::Value>, String> {
    if args.len() != 4 {
        return Err(
            "native storage.write requires encode[], data[], write[], and level[] descriptors"
                .into(),
        );
    }

    let mut descriptor_values = BTreeMap::<String, serde_json::Value>::new();
    for argument in args {
        let (kind, value_expr) = storage_descriptor_expr(argument).ok_or_else(|| {
            "native storage.write arguments must use descriptor brackets".to_string()
        })?;
        let value = static_json_with_bindings(value_expr, bindings).ok_or_else(|| {
            format!("native storage.write {kind}[] value must resolve to static JSON")
        })?;
        if descriptor_values.insert(kind.to_string(), value).is_some() {
            return Err(format!(
                "native storage.write descriptor {kind}[] was provided more than once"
            ));
        }
    }

    let take = |values: &mut BTreeMap<String, serde_json::Value>, name: &str| {
        values
            .remove(name)
            .ok_or_else(|| format!("native storage.write is missing {name}[]"))
    };
    let mut values = descriptor_values;
    let encoding = take(&mut values, "encode")?;
    let data = take(&mut values, "data")?;
    let path = take(&mut values, "write")?;
    let level = take(&mut values, "level")?;

    if !matches!(&path, serde_json::Value::String(value) if value.starts_with("$$/")) {
        return Err("native storage.write write[] must contain a symbolic $$/ path".into());
    }
    let level_valid = level
        .as_u64()
        .is_some_and(|value| (1..=3).contains(&value));
    if !level_valid {
        return Err("native storage.write level[] must be 1, 2, or 3".into());
    }
    if !encoding.is_string() {
        return Err("native storage.write encode[] must be a string".into());
    }

    Ok(vec![serde_json::json!({
        "path": path,
        "data": data,
        "encoding": encoding,
        "level": level,
    })])
}

fn storage_descriptor_expr(expr: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Object(fields) = expr else {
        return None;
    };
    let descriptor = fields
        .iter()
        .find(|(name, _)| name == "__rbeStorageDescriptor")?;
    let value = fields.iter().find(|(name, _)| name == "value")?;
    let Expr::String(kind) = &descriptor.1 else {
        return None;
    };
    matches!(kind.as_str(), "encode" | "data" | "write" | "level")
        .then_some((kind.as_str(), &value.1))
}

fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {''',
    "Storage descriptor lowerer",
)

# Parser regression: marker syntax remains four distinct arguments until the
# capability-aware compiler proves storage.write.
replace_once(
    "engine/crates/route-engine/src/lib.rs",
    """    #[test]\n    fn parses_video_manager_import_names_for_modules() {""",
    """    #[test]\n    fn parses_storage_write_descriptor_syntax_without_global_write_rewrite() {\n        let tokens = Lexer::new(\n            r#\":import[storage.write]\n            export function save() {\n                return write(\n                    encode[\"UTF8\"],\n                    data[{ ok: true }],\n                    write[$$/generated/data.json],\n                    level[1]\n                );\n            }\"#,\n        )\n        .tokenize()\n        .expect(\"lex failed\");\n        let file = Parser::new(tokens)\n            .parse_module_file()\n            .expect(\"module parse failed\");\n        let Statement::Return(Expr::Call(_, args)) = &file.functions[0].body[0] else {\n            panic!(\"expected storage.write call\");\n        };\n        assert_eq!(args.len(), 4);\n        assert!(matches!(&args[2], Expr::Object(fields) if fields.iter().any(|(key, value)| {\n            key == \"value\" && matches!(value, Expr::String(path) if path == \"$$/generated/data.json\")\n        })));\n    }\n\n    #[test]\n    fn parses_video_manager_import_names_for_modules() {""",
    "storage descriptor parser regression",
)

# End-to-end native compiler regression for a static linked Module write.
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """    #[test]\n    fn linked_module_video_call_uses_canonical_module_owner() {""",
    """    #[test]\n    fn linked_module_storage_write_lowers_descriptors_for_trusted_boundary() {\n        let module = parse_module(\n            r#\":import[storage.write as writeFile]\n               export function save() {\n                   return writeFile(\n                       encode[\"UTF8\"],\n                       data[{ ok: true }],\n                       write[$$/generated/data.json],\n                       level[2]\n                   );\n               }\"#,\n        );\n        let links = link_module_function(\"save\", \"accounts.cache\", &module, \"save\");\n        let route = parse(\n            r#\":import[\"./module/accounts/cache\".save]\n               class Route { get(req) { return save(); } }\"#,\n        );\n        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)\n        else {\n            panic!(\"static linked Storage write wrapper should compile natively\");\n        };\n        let host: CapabilityHost = Box::new(|request| {\n            assert_eq!(request.kind, ContainerCapabilityKind::Storage);\n            assert_eq!(request.target, \"storage:accounts.cache\");\n            assert_eq!(request.operation, \"write\");\n            assert_eq!(\n                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),\n                serde_json::json!([{\n                    \"path\": \"$$/generated/data.json\",\n                    \"data\": {\"ok\": true},\n                    \"encoding\": \"UTF8\",\n                    \"level\": 2\n                }])\n            );\n            Ok(br#\"{\\\"path\\\":\\\"$$/generated/data.json\\\",\\\"bytes\\\":11}\"#.to_vec())\n        });\n        let result = WasmExecutor::new()\n            .unwrap()\n            .execute_with_input_and_capabilities(\n                &artifact.bytes,\n                &[],\n                ExecutionLimits::default(),\n                Some(host),\n            )\n            .unwrap();\n        assert!(!result.output.is_empty());\n    }\n\n    #[test]\n    fn linked_module_video_call_uses_canonical_module_owner() {""",
    "native Storage write regression",
)
