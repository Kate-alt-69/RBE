from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Route AST owns route-local declarative FieldManager bindings. Reusable
# `.field` sources keep their existing FieldFile AST and execution path.
replace_once(
    "engine/crates/route-engine/src/ast.rs",
    '''pub struct RouteFile {
    pub imports: Vec<ImportTarget>,
    pub functions: Vec<FunctionDef>,
    pub class_name: String,
    pub methods: Vec<MethodDef>,
}
''',
    '''pub struct RouteFile {
    pub imports: Vec<ImportTarget>,
    pub field_bindings: Vec<FieldBinding>,
    pub functions: Vec<FunctionDef>,
    pub class_name: String,
    pub methods: Vec<MethodDef>,
}
''',
    "RouteFile FieldManager bindings",
)

# Parser: one route-local `fields { ... }` block after imports and before
# helper functions / class Route. The entries deliberately reuse the exact
# required()/optional()/dynamic() parser already used by `.field` files.
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''const KNOWN_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];
''',
    '''const KNOWN_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];
const RESERVED_INLINE_FIELD_NAMES: &[&str] = &["required", "optional", "has", "dynamic"];
''',
    "route-local FieldManager reserved names",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''        let mut errors = Vec::new();
        let mut imports = Vec::new();
        let mut functions = Vec::new();

        while self.check(&TokenKind::Colon) {
            match self.parse_imports() {
                Ok(entries) => imports.extend(entries),
                Err(error) => {
                    errors.push(error);
                    self.recover_top_level();
                }
            }
        }

        while self.check(&TokenKind::Function) {
''',
    '''        let mut errors = Vec::new();
        let mut imports = Vec::new();
        let mut field_bindings = Vec::new();
        let mut functions = Vec::new();

        while self.check(&TokenKind::Colon) {
            match self.parse_imports() {
                Ok(entries) => imports.extend(entries),
                Err(error) => {
                    errors.push(error);
                    self.recover_top_level();
                }
            }
        }

        if self.is_ident("fields") {
            match self.parse_route_fields_block() {
                Ok(bindings) => field_bindings = bindings,
                Err(error) => {
                    errors.push(error);
                    self.recover_top_level();
                }
            }
        }
        if self.is_ident("fields") {
            errors.push(self.error_here("duplicate route-local `fields { ... }` block"));
            self.recover_top_level();
        }
        if !field_bindings.is_empty()
            && !imports.iter().any(|import| {
                matches!(import, ImportTarget::Builtin(module) if module == "field")
            })
        {
            errors.push(self.error_here(
                "route-local `fields { ... }` requires the direct `:import[field]` namespace import",
            ));
        }

        while self.check(&TokenKind::Function) {
''',
    "parse route-local FieldManager block",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''            Some(RouteFile {
                imports,
                functions,
                class_name,
                methods,
            }),
''',
    '''            Some(RouteFile {
                imports,
                field_bindings,
                functions,
                class_name,
                methods,
            }),
''',
    "RouteFile parser construction",
)

replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''    fn parse_imports(&mut self) -> Result<Vec<ImportTarget>, ParseError> {
''',
    '''    fn parse_route_fields_block(&mut self) -> Result<Vec<FieldBinding>, ParseError> {
        let keyword = self.expect_ident()?;
        debug_assert_eq!(keyword, "fields");
        self.expect(TokenKind::LBrace)?;

        let mut bindings = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let name = self.expect_ident()?;
            if RESERVED_INLINE_FIELD_NAMES.contains(&name.as_str()) {
                return Err(self.error_here(&format!(
                    "route-local FieldManager name {name:?} is reserved by the direct field namespace"
                )));
            }
            self.expect(TokenKind::Eq)?;
            let binding = self.parse_field_binding(name)?;
            self.expect(TokenKind::Semicolon)?;
            if bindings
                .iter()
                .any(|existing: &FieldBinding| existing.name == binding.name)
            {
                return Err(self.error_here("duplicate route-local FieldManager binding"));
            }
            bindings.push(binding);
        }
        self.expect(TokenKind::RBrace)?;
        if bindings.is_empty() {
            return Err(self.error_here("route-local `fields { ... }` cannot be empty"));
        }
        Ok(bindings)
    }

    fn parse_imports(&mut self) -> Result<Vec<ImportTarget>, ParseError> {
''',
    "route-local FieldManager parser helper",
)

# Analyzer: a fields block consumes the direct `field` namespace even when
# handlers read only req.fields. Also update the one hand-built RouteFile test.
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    '''    for method in &file.methods {
        analyze_method(method, &globals, &mut used_globals, &mut diagnostics);
    }

    for (name, symbol) in globals {
''',
    '''    for method in &file.methods {
        analyze_method(method, &globals, &mut used_globals, &mut diagnostics);
    }
    if !file.field_bindings.is_empty() {
        if globals
            .get("field")
            .is_some_and(|symbol| symbol.kind == SymbolKind::Module)
        {
            used_globals.insert("field".into());
        } else {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E3030",
                message: "route-local `fields { ... }` requires `:import[field]`".into(),
                symbol: Some("field".into()),
            });
        }
    }

    for (name, symbol) in globals {
''',
    "FieldManager analyzer namespace usage",
)

replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    '''        let file = RouteFile {
            imports: vec![target],
            functions: Vec::new(),
''',
    '''        let file = RouteFile {
            imports: vec![target],
            field_bindings: Vec::new(),
            functions: Vec::new(),
''',
    "RouteFile analyzer test construction",
)

replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    '''    #[test]
    fn used_import_is_not_reported_unused() {
''',
    '''    #[test]
    fn route_local_fields_consume_direct_field_namespace() {
        let file = parse(
            r#":import[field]
               fields { page = optional("page", type = int, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let diagnostics = analyze(&file);
        assert!(diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("import `field` is never used")));
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != Severity::Error));
    }

    #[test]
    fn used_import_is_not_reported_unused() {
''',
    "FieldManager analyzer regression test",
)

# Runtime: fold route-local bindings into the exact same resolved map and
# capability namespace as reusable `.field` resolvers.
replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''pub(crate) struct FieldRoutePlan {
    direct_enabled: bool,
    resolvers: Vec<(String, Arc<FieldFile>)>,
}
''',
    '''pub(crate) struct FieldRoutePlan {
    direct_enabled: bool,
    inline_bindings: Vec<FieldBinding>,
    resolvers: Vec<(String, Arc<FieldFile>)>,
}
''',
    "FieldRoutePlan inline bindings",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''        let mut direct_enabled = false;
        let mut resolver_names = HashSet::new();
        let mut resolvers = Vec::new();

        for import in &route.imports {
''',
    '''        let mut direct_enabled = false;
        let inline_bindings = route.field_bindings.clone();
        if !inline_bindings.is_empty() && !route.imports.iter().any(is_direct_field_import) {
            return Err(format!(
                "Route {route_logical_name:?} uses route-local fields but does not import `:import[field]`"
            ));
        }
        let mut resolver_names = inline_bindings
            .iter()
            .map(|binding| binding.name.clone())
            .collect::<HashSet<_>>();
        let mut resolvers = Vec::new();

        for import in &route.imports {
''',
    "FieldRoutePlan build inline bindings",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''                    if !resolver_names.insert(function.clone()) {
                        continue;
                    }
''',
    '''                    if !resolver_names.insert(function.clone()) {
                        return Err(format!(
                            "Route {route_logical_name:?} declares duplicate FieldManager resolver name {function:?}"
                        ));
                    }
''',
    "FieldManager inline/reusable collision",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''        Ok(Self {
            direct_enabled,
            resolvers,
        })
''',
    '''        Ok(Self {
            direct_enabled,
            inline_bindings,
            resolvers,
        })
''',
    "FieldRoutePlan construction",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''    pub(crate) fn is_active(&self) -> bool {
        self.direct_enabled || !self.resolvers.is_empty()
    }
''',
    '''    pub(crate) fn is_active(&self) -> bool {
        self.direct_enabled || !self.inline_bindings.is_empty() || !self.resolvers.is_empty()
    }
''',
    "FieldRoutePlan active state",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''        let query = request_query(request)?.clone();
        let mut resolved = HashMap::new();
        let mut allowed_resolvers = HashSet::new();
        for (name, file) in &self.resolvers {
''',
    '''        let query = request_query(request)?.clone();
        let mut resolved = HashMap::new();
        let mut allowed_resolvers = HashSet::new();
        for binding in &self.inline_bindings {
            let value = resolve_binding(&query, binding, "route-local")?;
            resolved.insert(binding.name.clone(), value);
            allowed_resolvers.insert(binding.name.clone());
        }
        for (name, file) in &self.resolvers {
''',
    "FieldRoutePlan inline resolution",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''fn import_base(import: &ImportTarget) -> &ImportTarget {
''',
    '''fn is_direct_field_import(import: &ImportTarget) -> bool {
    matches!(import, ImportTarget::Builtin(module) if module == "field")
}

fn import_base(import: &ImportTarget) -> &ImportTarget {
''',
    "FieldManager direct import helper",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''    fn field(source: &str) -> FieldFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_field_file().unwrap()
    }

    fn request(entries: &[(&str, &str)]) -> Value {
''',
    '''    fn field(source: &str) -> FieldFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_field_file().unwrap()
    }

    fn route(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    fn request(entries: &[(&str, &str)]) -> Value {
''',
    "FieldManager route test helper",
)

replace_once(
    "engine/crates/route-engine/src/field_manager.rs",
    '''    #[test]
    fn direct_field_namespace_reads_the_same_query_snapshot() {
''',
    '''    #[test]
    fn route_local_fields_share_the_existing_resolver_runtime() {
        let route = route(
            r#":import[field]
               fields {
                   page = optional("page", type = int, default = 1);
                   debug = optional("debug", type = bool, default = false);
                   tracking = dynamic("utm_", stripPrefix = true);
                   cookie = required("cookie");
               }
               class Route { get(req) { return field.page(); } }"#,
        );
        assert_eq!(route.field_bindings.len(), 4);
        let plan = FieldRoutePlan {
            direct_enabled: true,
            inline_bindings: route.field_bindings.clone(),
            resolvers: Vec::new(),
        };
        let program = empty_program("route-local");
        let context = block_on_ready(plan.resolve(
            &request(&[("debug", "true"), ("utm_source", "chat"), ("cookie", "abc")]),
            &program,
        ))
        .unwrap();

        assert!(matches!(context.call("page", &[]).unwrap(), Value::Number(value) if value == 1.0));
        assert!(matches!(context.call("debug", &[]).unwrap(), Value::Bool(true)));
        assert!(matches!(context.call("cookie", &[]).unwrap(), Value::String(value) if value == "abc"));
        let Value::Object(tracking) = context.call("tracking", &[]).unwrap() else {
            panic!("expected tracking object")
        };
        assert!(matches!(tracking.get("source"), Some(Value::String(value)) if value == "chat"));
    }

    #[test]
    fn route_local_fields_require_direct_field_import() {
        let tokens = Lexer::new(
            r#"fields { page = optional("page", type = int, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
        )
        .tokenize()
        .unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error.message.contains(":import[field]"));
    }

    #[test]
    fn route_local_fields_reject_reserved_direct_helper_names() {
        let tokens = Lexer::new(
            r#":import[field]
               fields { required = optional("required"); }
               class Route { get(req) { return req.fields; } }"#,
        )
        .tokenize()
        .unwrap();
        let error = Parser::new(tokens).parse_file().unwrap_err();
        assert!(error.message.contains("reserved"));
    }

    #[test]
    fn direct_field_namespace_reads_the_same_query_snapshot() {
''',
    "FieldManager route-local runtime tests",
)

# Docs: FLD-003 is now an implemented runtime slice, not a future placeholder.
replace_once(
    "doc/field-manager.md",
    '''## Remaining runtime slice

Route-local declarative `fields { ... }` blocks are the next FLD-003 syntax/runtime slice. They will compile into the same resolver engine rather than duplicating request parsing or validation.
''',
    '''## Route-local declarative fields (FLD-003)

Small endpoint-owned fields can stay directly in the `.route` file while using the same FieldManager resolver engine:

```text
:import[field]

fields {
    page = optional("page", type = int, default = 1);
    debug = optional("debug", type = bool, default = false);
    tracking = dynamic("utm_", stripPrefix = true);
    cookie = required("cookie");
}

class Route {
    get(req) {
        return {
            page: field.page(),
            tracking: field.tracking(),
            all: req.fields
        };
    }
}
```

The block is intentionally declarative: it reuses the same `required(...)`, `optional(...)`, `dynamic(...)`, coercion, default, and structured failure behavior as reusable `.field` files. It requires the direct `:import[field]` namespace import, may appear once per Route, and cannot reuse the reserved direct helper names `required`, `optional`, `has`, or `dynamic`.

Inline and reusable FieldManager names share one per-Route namespace. A collision fails closed instead of silently shadowing one resolver. Field-backed Routes continue to use the linked evaluator path until native Route-WASM can consume the same pre-resolved Field context without creating a second resolution model.
''',
    "FieldManager FLD-003 docs",
)
