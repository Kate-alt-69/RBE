from pathlib import Path


def replace_block(path: str, start: str, end: str, replacement: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    start_pos = text.find(start)
    if start_pos < 0:
        raise SystemExit(f"missing start anchor in {path}: {start!r}")
    end_pos = text.find(end, start_pos)
    if end_pos < 0:
        raise SystemExit(f"missing end anchor in {path}: {end!r}")
    p.write_text(text[:start_pos] + replacement + text[end_pos:], encoding="utf-8")


cargo = Path("engine/crates/route-engine/Cargo.toml")
text = cargo.read_text(encoding="utf-8")
if 'fancy-regex = "=0.16.2"' not in text:
    anchor = 'regex = "1"\n'
    if anchor not in text:
        raise SystemExit("route-engine Cargo.toml is missing regex dependency anchor")
    text = text.replace(anchor, anchor + 'fancy-regex = "=0.16.2"\n', 1)
cargo.write_text(text, encoding="utf-8")

REGX = r'''const REGX_MAX_PATTERN_BYTES: usize = 4096;
const REGX_MAX_INPUT_BYTES: usize = 1024 * 1024;
const REGX_RAW_BACKTRACK_LIMIT: usize = 1_000_000;

fn regx_string<'a>(
    value: Option<&'a Value>,
    label: &str,
    limit: usize,
) -> Result<&'a str, ModuleError> {
    match value {
        Some(Value::String(value)) if value.len() <= limit => Ok(value),
        Some(Value::String(_)) => Err(ModuleError {
            message: format!("{label} exceeds {limit} bytes"),
        }),
        _ => Err(ModuleError {
            message: format!("{label} must be a string"),
        }),
    }
}

fn regx_descriptor_usize(
    descriptor: &HashMap<String, Value>,
    key: &str,
) -> Result<Option<usize>, ModuleError> {
    match descriptor.get(key) {
        None => Ok(None),
        Some(Value::Number(value))
            if value.is_finite()
                && *value >= 0.0
                && value.fract() == 0.0
                && *value <= usize::MAX as f64 =>
        {
            Ok(Some(*value as usize))
        }
        Some(_) => Err(ModuleError {
            message: format!("regx descriptor `{key}` must be a non-negative integer"),
        }),
    }
}

fn regx_descriptor_bool(
    descriptor: &HashMap<String, Value>,
    key: &str,
) -> Result<bool, ModuleError> {
    match descriptor.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ModuleError {
            message: format!("regx descriptor `{key}` must be a boolean"),
        }),
    }
}

fn regx_descriptor_strings<'a>(
    descriptor: &'a HashMap<String, Value>,
    key: &str,
) -> Result<Vec<&'a str>, ModuleError> {
    let Some(value) = descriptor.get(key) else {
        return Ok(Vec::new());
    };
    let Value::Array(values) = value else {
        return Err(ModuleError {
            message: format!("regx descriptor `{key}` must be an array of strings"),
        });
    };
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => Ok(value.as_str()),
            _ => Err(ModuleError {
                message: format!("regx descriptor `{key}` must contain only strings"),
            }),
        })
        .collect()
}

fn regx_token_matches_char(token: &str, value: char) -> bool {
    match token {
        "letters" => value.is_alphabetic(),
        "lowercase" => value.is_lowercase(),
        "uppercase" => value.is_uppercase(),
        "numbers" | "digits" => value.is_ascii_digit(),
        "alphanumeric" => value.is_alphanumeric(),
        "whitespace" => value.is_whitespace(),
        "symbols" => !value.is_alphanumeric() && !value.is_whitespace(),
        literal => literal.chars().any(|candidate| candidate == value),
    }
}

fn regx_descriptor_test(
    value: &str,
    descriptor: &HashMap<String, Value>,
) -> Result<bool, ModuleError> {
    const SUPPORTED_KEYS: &[&str] = &[
        "allow",
        "require",
        "min",
        "max",
        "exclude",
        "excludeContains",
        "noConsecutive",
        "ignoreCase",
    ];
    if let Some(key) = descriptor
        .keys()
        .find(|key| !SUPPORTED_KEYS.contains(&key.as_str()))
    {
        return Err(ModuleError {
            message: format!("unknown regx descriptor key `{key}`"),
        });
    }

    let length = value.chars().count();
    if let Some(min) = regx_descriptor_usize(descriptor, "min")? {
        if length < min {
            return Ok(false);
        }
    }
    if let Some(max) = regx_descriptor_usize(descriptor, "max")? {
        if length > max {
            return Ok(false);
        }
    }

    let allow = regx_descriptor_strings(descriptor, "allow")?;
    if !allow.is_empty()
        && !value
            .chars()
            .all(|character| allow.iter().any(|token| regx_token_matches_char(token, character)))
    {
        return Ok(false);
    }

    for required in regx_descriptor_strings(descriptor, "require")? {
        if !value
            .chars()
            .any(|character| regx_token_matches_char(required, character))
        {
            return Ok(false);
        }
    }

    let ignore_case = regx_descriptor_bool(descriptor, "ignoreCase")?;
    let normalized = if ignore_case {
        value.to_lowercase()
    } else {
        value.to_string()
    };

    for excluded in regx_descriptor_strings(descriptor, "exclude")? {
        let excluded = if ignore_case {
            excluded.to_lowercase()
        } else {
            excluded.to_string()
        };
        if normalized == excluded {
            return Ok(false);
        }
    }

    for excluded in regx_descriptor_strings(descriptor, "excludeContains")? {
        let excluded = if ignore_case {
            excluded.to_lowercase()
        } else {
            excluded.to_string()
        };
        if !excluded.is_empty() && normalized.contains(&excluded) {
            return Ok(false);
        }
    }

    for token in regx_descriptor_strings(descriptor, "noConsecutive")? {
        if !token.is_empty() && value.contains(&format!("{token}{token}")) {
            return Ok(false);
        }
    }

    Ok(true)
}

fn build_raw_regx(pattern: &str) -> Result<fancy_regex::Regex, ModuleError> {
    let mut builder = fancy_regex::RegexBuilder::new(pattern);
    builder.backtrack_limit(REGX_RAW_BACKTRACK_LIMIT);
    builder.build().map_err(|error| ModuleError {
        message: format!("invalid regx raw pattern: {error}"),
    })
}

fn call_regx(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "raw" => {
            if !(1..=2).contains(&args.len()) {
                return Err(ModuleError {
                    message: "regx.raw() expects pattern[, input]".into(),
                });
            }
            let pattern = regx_string(args.first(), "regx pattern", REGX_MAX_PATTERN_BYTES)?;
            let compiled = build_raw_regx(pattern)?;
            if args.len() == 1 {
                return Ok(Value::String(pattern.to_string()));
            }
            let value = regx_string(args.get(1), "regx input", REGX_MAX_INPUT_BYTES)?;
            compiled
                .is_match(value)
                .map(Value::Bool)
                .map_err(|error| ModuleError {
                    message: format!(
                        "regx.raw() evaluation failed within the bounded backtracking budget: {error}"
                    ),
                })
        }
        "test" => {
            if args.len() != 2 {
                return Err(ModuleError {
                    message: "regx.test() expects exactly two arguments".into(),
                });
            }
            if let Some(Value::Object(descriptor)) = args.get(1) {
                let value = regx_string(args.first(), "regx input", REGX_MAX_INPUT_BYTES)?;
                return regx_descriptor_test(value, descriptor).map(Value::Bool);
            }

            // Compatibility path: existing REL uses test(pattern, value).
            // Keep it on Rust regex's bounded linear-time engine.
            let pattern = regx_string(args.first(), "regx pattern", REGX_MAX_PATTERN_BYTES)?;
            let value = regx_string(args.get(1), "regx input", REGX_MAX_INPUT_BYTES)?;
            let compiled = regex::Regex::new(pattern).map_err(|error| ModuleError {
                message: format!("invalid regx pattern: {error}"),
            })?;
            Ok(Value::Bool(compiled.is_match(value)))
        }
        other => Err(ModuleError {
            message: format!("regx.{other}() does not exist"),
        }),
    }
}

'''
replace_block(
    "engine/crates/route-engine/src/modules.rs",
    "const REGX_MAX_PATTERN_BYTES: usize = 4096;",
    "fn request_object(",
    REGX,
)

modules = Path("engine/crates/route-engine/src/modules.rs")
text = modules.read_text(encoding="utf-8")
anchor = "    #[test]\n    fn unknown_private_function_is_reported() {\n"
if anchor not in text:
    raise SystemExit("missing REGX test insertion anchor")
TESTS = r'''    #[test]
    fn regx_descriptors_cover_common_api_validation_without_raw_patterns() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("regx".into())]);
        let descriptor = Value::Object(HashMap::from([
            (
                "allow".into(),
                Value::Array(vec![
                    Value::String("letters".into()),
                    Value::String("numbers".into()),
                    Value::String("_-".into()),
                ]),
            ),
            (
                "require".into(),
                Value::Array(vec![
                    Value::String("uppercase".into()),
                    Value::String("numbers".into()),
                    Value::String("symbols".into()),
                ]),
            ),
            ("min".into(), Value::Number(8.0)),
            ("max".into(), Value::Number(64.0)),
            (
                "exclude".into(),
                Value::Array(vec![Value::String("Admin_123".into())]),
            ),
            ("ignoreCase".into(), Value::Bool(true)),
        ]));
        assert!(matches!(
            registry
                .call(
                    "regx",
                    "test",
                    &[Value::String("Good_123".into()), descriptor.clone()],
                )
                .unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            registry
                .call(
                    "regx",
                    "test",
                    &[Value::String("admin_123".into()), descriptor],
                )
                .unwrap(),
            Value::Bool(false)
        ));
    }

    #[test]
    fn regx_raw_supports_lookaheads_and_backreferences() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("regx".into())]);
        let password = r"^(?=.*[A-Z])(?=.*\d)(?=.*[!@#$%^&*])[A-Za-z\d!@#$%^&*]{8,64}$";
        assert!(matches!(
            registry
                .call(
                    "regx",
                    "raw",
                    &[
                        Value::String(password.into()),
                        Value::String("GoodPass1!".into()),
                    ],
                )
                .unwrap(),
            Value::Bool(true)
        ));

        let repeated = r"\b(\w+)\s+\1\b";
        assert!(matches!(
            registry
                .call(
                    "regx",
                    "raw",
                    &[
                        Value::String(repeated.into()),
                        Value::String("the the".into()),
                    ],
                )
                .unwrap(),
            Value::Bool(true)
        ));
    }

    #[test]
    fn regx_raw_keeps_pattern_and_input_bounds() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("regx".into())]);
        let oversized = "x".repeat(REGX_MAX_PATTERN_BYTES + 1);
        let error = registry
            .call("regx", "raw", &[Value::String(oversized)])
            .unwrap_err();
        assert!(error.message.contains("exceeds"));
    }

'''
text = text.replace(anchor, TESTS + anchor, 1)
modules.write_text(text, encoding="utf-8")

docs = Path("doc/field-manager.md")
text = docs.read_text(encoding="utf-8")
old = "The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` provides bounded deterministic regex matching and `regx.raw(pattern)` validates/returns a raw regex pattern."
new = "The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` keeps ordinary patterns on Rust's linear-time regex engine. `regx.test(value, descriptor)` provides readable validation descriptors (`allow`, `require`, `min`, `max`, `exclude`, `excludeContains`, `noConsecutive`, `ignoreCase`). `regx.raw(pattern)` validates advanced syntax, while `regx.raw(pattern, value)` uses a separately bounded backtracking engine for lookarounds and backreferences. `regx` is a shared REL builtin usable from `.route`, `.module`, `.service`, `server.server`, and `.field`; Field REL remains intentionally restricted to the pure `math` + `regx` capability set."
if old in text:
    text = text.replace(old, new, 1)
else:
    marker = "## Request-time runtime"
    if marker in text and new not in text:
        text = text.replace(marker, new + "\n\n" + marker, 1)
docs.write_text(text, encoding="utf-8")
