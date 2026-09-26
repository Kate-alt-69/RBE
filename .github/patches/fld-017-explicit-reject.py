from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one patch anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != expected:
        raise SystemExit(f"{path}: expected {expected} patch anchors, found {count}")
    file.write_text(text.replace(old, new), encoding="utf-8")


module_eval = "engine/crates/route-engine/src/module_eval.rs"
replace_once(
    module_eval,
    "const MAX_MODULE_CALL_DEPTH: usize = 64;\n",
    "const MAX_MODULE_CALL_DEPTH: usize = 64;\nconst MAX_FIELD_REJECTION_REASON_BYTES: usize = 128;\n",
)
replace_once(
    module_eval,
    '''pub struct ModuleExecutor<'a> {
    program: &'a ModuleProgram,
    services: Option<Arc<dyn ServiceCaller>>,
    host_capabilities: Option<Arc<dyn HostCapabilityCaller>>,
    classes: Arc<HashMap<String, ServiceClassDef>>,
}
''',
    '''pub struct ModuleExecutor<'a> {
    program: &'a ModuleProgram,
    services: Option<Arc<dyn ServiceCaller>>,
    host_capabilities: Option<Arc<dyn HostCapabilityCaller>>,
    classes: Arc<HashMap<String, ServiceClassDef>>,
    field_reject_enabled: bool,
}
''',
)
replace_count(
    module_eval,
    "            classes: Arc::new(HashMap::new()),\n",
    "            classes: Arc::new(HashMap::new()),\n            field_reject_enabled: false,\n",
    4,
)
replace_count(
    module_eval,
    "            classes,\n        }\n",
    "            classes,\n            field_reject_enabled: false,\n        }\n",
    2,
)
replace_once(
    module_eval,
    '''    pub fn with_services(program: &'a ModuleProgram, services: ServiceManager) -> Self {
        Self::with_service_caller(program, Arc::new(services))
    }
''',
    '''    pub(crate) fn for_field_resolver(program: &'a ModuleProgram) -> Self {
        let mut executor = Self::new(program);
        executor.field_reject_enabled = true;
        executor
    }

    pub fn with_services(program: &'a ModuleProgram, services: ServiceManager) -> Self {
        Self::with_service_caller(program, Arc::new(services))
    }
''',
)
replace_once(
    module_eval,
    '''    async fn call_host_capability(
        &self,
        scope: Option<String>,
''',
    '''    fn reject_field_request(&self, args: &[Value]) -> Result<Value, ModuleEvalError> {
        if args.len() != 1 {
            return Err(ModuleEvalError::new(
                "FLD5002",
                format!("field reject() expects exactly one reason string, got {} argument(s)", args.len()),
            ));
        }
        let Value::String(reason) = &args[0] else {
            return Err(ModuleEvalError::new(
                "FLD5002",
                "field reject() reason must be a string",
            ));
        };
        let valid_reason = !reason.is_empty()
            && reason.len() <= MAX_FIELD_REJECTION_REASON_BYTES
            && reason.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            });
        if !valid_reason {
            return Err(ModuleEvalError::new(
                "FLD5002",
                format!(
                    "field reject() reason must be 1..={MAX_FIELD_REJECTION_REASON_BYTES} bytes and contain only ASCII letters, digits, `_`, `-`, `.`, or `:`"
                ),
            ));
        }
        Err(ModuleEvalError::new("FLD4003", reason.clone()))
    }

    async fn call_host_capability(
        &self,
        scope: Option<String>,
''',
)
replace_once(
    module_eval,
    '''                    if let Expr::Ident(name) = callee.as_ref() {
                        if let Some((module, function)) = self.builtin_functions.get(name).cloned()
''',
    '''                    if let Expr::Ident(name) = callee.as_ref() {
                        if name == "reject"
                            && self.executor.field_reject_enabled
                            && !self.scope.contains_key(name)
                            && !self.is_import_binding(name)
                        {
                            return self.executor.reject_field_request(&args);
                        }
                        if let Some((module, function)) = self.builtin_functions.get(name).cloned()
''',
)

field_manager = "engine/crates/route-engine/src/field_manager.rs"
replace_once(
    field_manager,
    '''    ModuleExecutor::new(program)
        .call_inline_definition(synthetic, resolver, args)
        .await
        .map_err(|error| {
            resolve_error(
                "FLD4003",
                resolver_name,
                format!("Field resolver rejected the request: {}", error.message),
            )
        })
''',
    '''    ModuleExecutor::for_field_resolver(program)
        .call_inline_definition(synthetic, resolver, args)
        .await
        .map_err(|error| {
            if error.code == "FLD4003" {
                resolve_error(
                    "FLD4003",
                    resolver_name,
                    format!("Field resolver rejected the request: {}", error.message),
                )
            } else {
                resolve_error(
                    "FLD5002",
                    resolver_name,
                    format!("Field resolver execution failed internally: {error}"),
                )
            }
        })
''',
)
replace_once(
    field_manager,
    '''    #[test]
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
}
''',
    '''    #[test]
    fn executable_field_reject_is_client_validation() {
        let file = field(
            r#":field[source = query, key = "username"]
               resolve(raw) {
                   if (raw == "bad") {
                       reject("invalid_username");
                   }
                   return raw;
               }"#,
        );
        let program = empty_program("explicit-reject");
        let error = block_on_ready(resolve_field_file(
            &file,
            &request(&[("username", "bad")]),
            &program,
            "username",
        ))
        .unwrap_err();
        assert_eq!(error.code, "FLD4003");
        assert_eq!(error.field, "username");
        assert!(error.message.contains("invalid_username"));

        let accepted = block_on_ready(resolve_field_file(
            &file,
            &request(&[("username", "good")]),
            &program,
            "username",
        ))
        .unwrap();
        assert!(matches!(accepted, Value::String(value) if value == "good"));
    }

    #[test]
    fn executable_field_evaluator_fault_is_internal() {
        let file = field(
            r#":field[source = query, key = "username"]
               resolve(raw) {
                   missing(raw);
                   return raw;
               }"#,
        );
        let program = empty_program("resolver-fault");
        let error = block_on_ready(resolve_field_file(
            &file,
            &request(&[("username", "bad")]),
            &program,
            "username",
        ))
        .unwrap_err();
        assert_eq!(error.code, "FLD5002");
        assert!(error.message.contains("MOD3201"));
    }

    #[test]
    fn malformed_field_reject_is_internal() {
        let file = field(
            r#":field[source = query, key = "username"]
               resolve(raw) {
                   reject("not a reason code");
                   return raw;
               }"#,
        );
        let program = empty_program("malformed-reject");
        let error = block_on_ready(resolve_field_file(
            &file,
            &request(&[("username", "bad")]),
            &program,
            "username",
        ))
        .unwrap_err();
        assert_eq!(error.code, "FLD5002");
        assert!(error.message.contains("reject() reason"));
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
}
''',
)

docs = "doc/field-manager.md"
replace_once(
    docs,
    '''resolve(raw, context) {
    return math.trim(raw);
}
```

The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` keeps ordinary patterns on Rust's linear-time regex engine. `regx.test(value, descriptor)` provides readable validation descriptors (`allow`, `require`, `min`, `max`, `exclude`, `excludeContains`, `noConsecutive`, `ignoreCase`). `regx.raw(pattern)` validates advanced syntax, while `regx.raw(pattern, value)` uses a separately bounded backtracking engine for lookarounds and backreferences. `regx` is a shared REL builtin usable from `.route`, `.module`, `.service`, `server.server`, and `.field`; Field REL remains intentionally restricted to the pure `math` + `regx` capability set.
''',
    '''resolve(raw, context) {
    const slug = math.trim(raw);
    if (slug == "blocked") {
        reject("invalid_slug");
    }
    return slug;
}
```

The resolver is compiled as REL but remains pure. Executable `.field` resolvers additionally receive the field-only `reject("reason_code")` intrinsic. `reject` takes exactly one bounded reason code (1–128 ASCII bytes; letters, digits, `_`, `-`, `.`, and `:` only) and intentionally terminates resolution as `FLD4003`, which becomes the normal HTTP 400 `field_validation_failed` path. Ordinary evaluator/programming failures are not client validation: they become internal `FLD5002` failures and the HTTP edge returns `field_runtime_failed`. `reject` is not enabled for `.route`, `.module`, `.service`, or `server.server`, and it does not grant any new host capability.

`regx.test(pattern, value)` keeps ordinary patterns on Rust's linear-time regex engine. `regx.test(value, descriptor)` provides readable validation descriptors (`allow`, `require`, `min`, `max`, `exclude`, `excludeContains`, `noConsecutive`, `ignoreCase`). `regx.raw(pattern)` validates advanced syntax, while `regx.raw(pattern, value)` uses a separately bounded backtracking engine for lookarounds and backreferences. `regx` is a shared REL builtin usable from `.route`, `.module`, `.service`, `server.server`, and `.field`; Field REL remains intentionally restricted to the pure `math` + `regx` capability set.
''',
)
