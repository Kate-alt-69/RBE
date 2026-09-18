from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    file.write_text(text.replace(old, new, 1))


# route-engine dependency
replace_once(
    "engine/crates/route-engine/Cargo.toml",
    'hmac = "0.12"\nrand = "0.8"\n',
    'hmac = "0.12"\nargon2 = "0.5"\nrand = "0.8"\n',
    "argon2 dependency",
)

# Import AST: `<library> from <module>` is a namespace import, not a function import.
replace_once(
    "engine/crates/route-engine/src/ast.rs",
    '''    BuiltinFunction {
        module: String,
        function: String,
    },
    Custom(String),
''',
    '''    BuiltinFunction {
        module: String,
        function: String,
    },
    BuiltinSubLibrary {
        module: String,
        library: String,
    },
    Custom(String),
''',
    "builtin sub-library AST",
)

# Parser: :import[argon from crypto]
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''            TokenKind::Ident(name) => {
                if name == "service" && self.check(&TokenKind::Colon) {
''',
    '''            TokenKind::Ident(name) => {
                if self.is_from_keyword() {
                    self.advance();
                    let module = self.expect_ident()?;
                    Ok(ImportTarget::BuiltinSubLibrary {
                        module,
                        library: name,
                    })
                } else if name == "service" && self.check(&TokenKind::Colon) {
''',
    "parser sub-library import",
)
replace_once(
    "engine/crates/route-engine/src/parser.rs",
    '''    fn is_as_keyword(&self) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(TokenKind::Ident(name)) if name == "as")
    }

''',
    '''    fn is_as_keyword(&self) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(TokenKind::Ident(name)) if name == "as")
    }

    fn is_from_keyword(&self) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(TokenKind::Ident(name)) if name == "from")
    }

''',
    "parser from keyword",
)

# modules.rs registry + Argon2id implementation
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''    Log,
    Crypto,
    Http,
''',
    '''    Log,
    Crypto,
    CryptoArgon,
    Http,
''',
    "crypto argon builtin enum",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''pub fn builtin_function_exists(module: &str, function: &str) -> bool {
''',
    '''pub fn builtin_sublibrary_function_exists(module: &str, library: &str, function: &str) -> bool {
    matches!(
        (module, library, function),
        ("crypto", "argon", "hash")
            | ("crypto", "argon", "hashPassword")
            | ("crypto", "argon", "hash_password")
            | ("crypto", "argon", "verify")
            | ("crypto", "argon", "verifyPassword")
            | ("crypto", "argon", "verify_password")
    )
}

pub fn builtin_function_exists(module: &str, function: &str) -> bool {
''',
    "sublibrary function catalog",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::BuiltinFunction { function, .. } => function.clone(),
        ImportTarget::Custom(path) => std::path::Path::new(path)
''',
    '''        ImportTarget::Builtin(name) => name.clone(),
        ImportTarget::BuiltinFunction { function, .. } => function.clone(),
        ImportTarget::BuiltinSubLibrary { library, .. } => library.clone(),
        ImportTarget::Custom(path) => std::path::Path::new(path)
''',
    "sublibrary binding name",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''                ImportTarget::BuiltinFunction { module, function } => {
                    let kind = match module.as_str() {
''',
    '''                ImportTarget::BuiltinSubLibrary { module, library } => {
                    let kind = match (module.as_str(), library.as_str()) {
                        ("crypto", "argon") => ModuleKind::Builtin(BuiltinModule::CryptoArgon),
                        _ => ModuleKind::CustomUnimplemented {
                            source_path: format!("builtin:{library} from {module}"),
                            resolved_path: std::path::PathBuf::new(),
                        },
                    };
                    modules.insert(binding_name(target), kind);
                }
                ImportTarget::BuiltinFunction { module, function } => {
                    let kind = match module.as_str() {
''',
    "registry sublibrary branch",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''            ModuleKind::Builtin(BuiltinModule::Crypto) => call_crypto(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Http) => Err(ModuleError {
''',
    '''            ModuleKind::Builtin(BuiltinModule::Crypto) => call_crypto(function_name, args),
            ModuleKind::Builtin(BuiltinModule::CryptoArgon) => {
                call_crypto_argon(function_name, args)
            }
            ModuleKind::Builtin(BuiltinModule::Http) => Err(ModuleError {
''',
    "crypto argon dispatch",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''const CRYPTO_MAX_HMAC_KEY_BYTES: usize = 4 * 1024;
const CRYPTO_MAX_RANDOM_BYTES: usize = 4 * 1024;
''',
    '''const CRYPTO_MAX_HMAC_KEY_BYTES: usize = 4 * 1024;
const CRYPTO_MAX_PASSWORD_BYTES: usize = 4 * 1024;
const CRYPTO_MAX_PASSWORD_HASH_BYTES: usize = 1024;
const CRYPTO_MAX_RANDOM_BYTES: usize = 4 * 1024;
''',
    "argon bounds",
)
replace_once(
    "engine/crates/route-engine/src/modules.rs",
    '''fn call_env(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
''',
    '''fn call_crypto_argon(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
    use argon2::Argon2;

    match function_name {
        "hash" | "hashPassword" | "hash_password" => {
            crypto_expect_arity(function_name, args, 1)?;
            let password = crypto_string(
                function_name,
                args,
                0,
                "password",
                CRYPTO_MAX_PASSWORD_BYTES,
            )?;
            let salt = SaltString::generate(&mut rand::rngs::OsRng);
            let encoded = Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map_err(|error| {
                    crypto_error("CRY4001", format!("Argon2id password hashing failed: {error}"))
                })?
                .to_string();
            Ok(Value::String(encoded))
        }
        "verify" | "verifyPassword" | "verify_password" => {
            crypto_expect_arity(function_name, args, 2)?;
            let password = crypto_string(
                function_name,
                args,
                0,
                "password",
                CRYPTO_MAX_PASSWORD_BYTES,
            )?;
            let encoded = crypto_string(
                function_name,
                args,
                1,
                "encoded hash",
                CRYPTO_MAX_PASSWORD_HASH_BYTES,
            )?;
            let parsed = PasswordHash::new(encoded).map_err(|error| {
                crypto_error("CRY1001", format!("argon encoded hash is invalid: {error}"))
            })?;
            if parsed.algorithm.as_str() != "argon2id" {
                return Err(crypto_error(
                    "CRY1001",
                    "argon.verifyPassword() accepts Argon2id hashes only",
                ));
            }
            Ok(Value::Bool(
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok(),
            ))
        }
        other => Err(crypto_error(
            "CRY1002",
            format!("crypto sub-library argon.{other}() does not exist"),
        )),
    }
}

fn call_env(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
''',
    "argon implementation",
)

# Analyzer: routes cannot import expensive KDF sub-libraries.
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    '''        ImportTarget::BuiltinFunction { module, function } => {
            format!("builtin:{module}.{function}")
        }
        ImportTarget::Custom(path) => format!("custom:{path}"),
''',
    '''        ImportTarget::BuiltinFunction { module, function } => {
            format!("builtin:{module}.{function}")
        }
        ImportTarget::BuiltinSubLibrary { module, library } => {
            format!("builtin:{module}/{library}")
        }
        ImportTarget::Custom(path) => format!("custom:{path}"),
''',
    "analyzer import source key",
)
replace_once(
    "engine/crates/route-engine/src/analyzer.rs",
    '''            ImportTarget::BuiltinFunction { module, function } => {
                if !builtin_function_exists(module, function) {
''',
    '''            ImportTarget::BuiltinSubLibrary { module, library } => {
                diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: "E3000",
                    message: format!(
                        "crypto sub-library `{library} from {module}` is not available to `.route` files"
                    ),
                    symbol: Some(name.clone()),
                });
                SymbolKind::Module
            }
            ImportTarget::BuiltinFunction { module, function } => {
                if !builtin_function_exists(module, function) {
''',
    "route sublibrary rejection",
)

# Transpiler namespace classification.
replace_once(
    "engine/crates/route-engine/src/transpiler.rs",
    '''                ImportTarget::Builtin(_) | ImportTarget::Custom(_) | ImportTarget::Service(_) => {
''',
    '''                ImportTarget::Builtin(_)
                | ImportTarget::BuiltinSubLibrary { .. }
                | ImportTarget::Custom(_)
                | ImportTarget::Service(_) => {
''',
    "transpiler sublibrary namespace",
)

# Module compiler source identity + explicit supported sub-library validation.
replace_once(
    "engine/crates/route-engine/src/module_runtime.rs",
    '''            ImportTarget::BuiltinFunction { module, .. } if module == "video" => {
                errors.push(ModuleCompileError {
''',
    '''            ImportTarget::BuiltinFunction { module, .. } if module == "video" => {
                errors.push(ModuleCompileError {
''',
    "module runtime stable anchor",
)
replace_once(
    "engine/crates/route-engine/src/module_runtime.rs",
    '''            _ => {}
        }

        let Some(services) = services else {
''',
    '''            ImportTarget::BuiltinSubLibrary { module, library }
                if !(module == "crypto" && library == "argon") =>
            {
                errors.push(ModuleCompileError {
                    code: "MOD2012",
                    path: path.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: format!(
                        "unknown builtin sub-library {library:?} from {module:?}; supported: argon from crypto"
                    ),
                });
            }
            _ => {}
        }

        let Some(services) = services else {
''',
    "module sublibrary validation",
)
replace_once(
    "engine/crates/route-engine/src/module_runtime.rs",
    '''        ImportTarget::BuiltinFunction { module, function } => {
            format!("builtin:{module}.{function}")
        }
        ImportTarget::Custom(path) => format!("module:{path}"),
''',
    '''        ImportTarget::BuiltinFunction { module, function } => {
            format!("builtin:{module}.{function}")
        }
        ImportTarget::BuiltinSubLibrary { module, library } => {
            format!("builtin:{module}/{library}")
        }
        ImportTarget::Custom(path) => format!("module:{path}"),
''',
    "module source key",
)

# Shared module evaluator: sub-library namespaces are handled by ModuleRegistry,
# not host-capability dispatch.
replace_once(
    "engine/crates/route-engine/src/module_eval.rs",
    '''                ImportTarget::BuiltinFunction { module, function } => {
                    builtin_functions.insert(binding, (module.clone(), function.clone()));
                }
                ImportTarget::Custom(path) => {
''',
    '''                ImportTarget::BuiltinFunction { module, function } => {
                    builtin_functions.insert(binding, (module.clone(), function.clone()));
                }
                ImportTarget::BuiltinSubLibrary { .. } => {}
                ImportTarget::Custom(path) => {
''',
    "module evaluator sublibrary",
)

# RELC role gates and metadata.
relc = Path("engine/crates/route-engine/src/relc.rs")
text = relc.read_text()
old = '''        let base = import_base(import);
        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } =
            base
        {
'''
new = '''        let base = import_base(import);
        if let ImportTarget::BuiltinSubLibrary { module, library } = base {
            if !(module == "crypto" && library == "argon") {
                return Err(RelcError::Capability {
                    code: "RELC2102",
                    source: source.clone(),
                    message: format!(
                        "unknown builtin sub-library {library:?} from {module:?}; supported: `argon from crypto`"
                    ),
                });
            }
            if !matches!(kind, RelSourceKind::Module | RelSourceKind::Service) {
                return Err(RelcError::Capability {
                    code: "RELC2101",
                    source: source.clone(),
                    message: "Argon2id is an expensive crypto sub-library and is available only to Module and Service REL"
                        .into(),
                });
            }
        }
        if let ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } =
            base
        {
'''
if text.count(old) != 1:
    raise SystemExit(f"RELC sublibrary gate anchor: expected 1, found {text.count(old)}")
text = text.replace(old, new, 1)

old = '''        ImportTarget::BuiltinFunction { module, function } => format!("{module}.{function}"),
        ImportTarget::Custom(path) => format!("module:{path}"),
'''
new = '''        ImportTarget::BuiltinFunction { module, function } => format!("{module}.{function}"),
        ImportTarget::BuiltinSubLibrary { module, library } => {
            format!("{library} from {module}")
        }
        ImportTarget::Custom(path) => format!("module:{path}"),
'''
if text.count(old) != 1:
    raise SystemExit(f"RELC import label anchor: expected 1, found {text.count(old)}")
text = text.replace(old, new, 1)

# Any sub-library is pure in-process crypto and owns no external Runtime Image host authority.
old = '''            ImportTarget::Custom(_)
            | ImportTarget::CustomFunction { .. }
            | ImportTarget::Aliased { .. } => {}
'''
new = '''            ImportTarget::BuiltinSubLibrary { .. }
            | ImportTarget::Custom(_)
            | ImportTarget::CustomFunction { .. }
            | ImportTarget::Aliased { .. } => {}
'''
if text.count(old) != 1:
    raise SystemExit(f"RELC direct capability exhaustive anchor: expected 1, found {text.count(old)}")
text = text.replace(old, new, 1)
relc.write_text(text)

# Error book: execution failures get a stable code; malformed hashes remain CRY1001.
replace_once(
    "doc/error-codes/runtime.md",
    '''<a id="cry3001"></a>
### CRY3001 — secure random generation failed
''',
    '''<a id="cry3001"></a>
### CRY3001 — secure random generation failed
''',
    "crypto error book stable anchor",
)
replace_once(
    "doc/error-codes/runtime.md",
    '''**Action:** inspect the host entropy/platform failure. Do not replace this failure with timestamps, counters or non-cryptographic randomness.

<a id="video-manager-codes"></a>
''',
    '''**Action:** inspect the host entropy/platform failure. Do not replace this failure with timestamps, counters or non-cryptographic randomness.

<a id="cry4001"></a>
### CRY4001 — Argon2id password hashing failed

**Status:** Emitted.

The `argon from crypto` sub-library could not complete an Argon2id password-hash operation. RBE owns the Argon2id parameters and random salt generation; REL callers cannot weaken or reuse them manually.

**Action:** preserve the diagnostic and inspect resource/runtime failure. Do not fall back to a fast general-purpose hash for passwords.

<a id="video-manager-codes"></a>
''',
    "argon error code docs",
)

# Focused tests in existing modules test block.
modules = Path("engine/crates/route-engine/src/modules.rs")
text = modules.read_text()
anchor = '''    #[test]\n    fn unknown_private_function_is_reported() {\n'''
tests = r'''    #[test]
    fn crypto_argon_hashes_and_verifies_argon2id_passwords() {
        let target = ImportTarget::BuiltinSubLibrary {
            module: "crypto".into(),
            library: "argon".into(),
        };
        let registry = ModuleRegistry::from_imports(&[target]);
        let Value::String(encoded) = registry
            .call("argon", "hashPassword", &[Value::String("correct horse battery staple".into())])
            .expect("argon hash")
        else {
            panic!("expected encoded password hash");
        };
        assert!(encoded.starts_with("$argon2id$"));
        assert!(matches!(
            registry
                .call(
                    "argon",
                    "verifyPassword",
                    &[
                        Value::String("correct horse battery staple".into()),
                        Value::String(encoded.clone()),
                    ],
                )
                .expect("argon verify"),
            Value::Bool(true)
        ));
        assert!(matches!(
            registry
                .call(
                    "argon",
                    "verifyPassword",
                    &[
                        Value::String("wrong".into()),
                        Value::String(encoded),
                    ],
                )
                .expect("argon mismatch"),
            Value::Bool(false)
        ));
    }

'''
if text.count(anchor) != 1:
    raise SystemExit("argon test insertion anchor drifted")
modules.write_text(text.replace(anchor, tests + anchor, 1))

# Parser/role tests in lib.rs.
lib = Path("engine/crates/route-engine/src/lib.rs")
text = lib.read_text()
anchor = '''    #[test]\n    fn parses_multiple_import_entries_and_aliases() {\n'''
tests = r'''    #[test]
    fn parses_builtin_crypto_sublibrary_import() {
        let tokens = Lexer::new(
            r#":import[argon from crypto]
            export function hash(value) { return argon.hashPassword(value); }"#,
        )
        .tokenize()
        .expect("lex failed");
        let file = Parser::new(tokens)
            .parse_module_file()
            .expect("module parse failed");
        assert!(matches!(
            file.imports.as_slice(),
            [ImportTarget::BuiltinSubLibrary { module, library }]
                if module == "crypto" && library == "argon"
        ));
        assert_eq!(binding_name(&file.imports[0]), "argon");
    }

'''
if text.count(anchor) != 1:
    raise SystemExit("argon parser test insertion anchor drifted")
lib.write_text(text.replace(anchor, tests + anchor, 1))
