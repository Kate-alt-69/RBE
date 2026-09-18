from pathlib import Path

PUBLIC_HELP = "https://kastrick.vercel.app/project/rbe/doc/error-codes/runtime"


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    file.write_text(text.replace(old, new, 1))


# Dependencies: SHA-256/hex already exist. Add only the primitives CRY-001 needs.
replace_once(
    "engine/crates/route-engine/Cargo.toml",
    'sha2 = "0.10"\nsubtle = "2"\nhex = "0.4"\n',
    'sha2 = "0.10"\nhmac = "0.12"\nrand = "0.8"\nsubtle = "2"\nhex = "0.4"\n',
    "route-engine crypto dependencies",
)

modules = Path("engine/crates/route-engine/src/modules.rs")
text = modules.read_text()
text = text.replace(
    '        "crypto" => matches!(function, "hash"),\n',
    '        "crypto" => matches!(\n            function,\n            "hash"\n                | "sha256"\n                | "hmacSha256"\n                | "hmac_sha256"\n                | "randomBytes"\n                | "random_bytes"\n                | "randomToken"\n                | "random_token"\n        ),\n',
    1,
)
if '"hmacSha256"' not in text:
    raise SystemExit("crypto builtin export patch did not apply")

old_crypto = '''fn call_crypto(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "hash" => {
            let Some(Value::String(input)) = args.first() else {
                return Err(ModuleError {
                    message: "crypto.hash(value) requires a string argument".into(),
                });
            };
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            input.hash(&mut hasher);
            Ok(Value::String(format!("{:016x}", hasher.finish())))
        }
        other => Err(ModuleError {
            message: format!("crypto.{other}() does not exist"),
        }),
    }
}
'''
new_crypto = '''const CRYPTO_MAX_INPUT_BYTES: usize = 1024 * 1024;
const CRYPTO_MAX_HMAC_KEY_BYTES: usize = 4 * 1024;
const CRYPTO_MAX_RANDOM_BYTES: usize = 4 * 1024;
const CRYPTO_DEFAULT_TOKEN_BYTES: usize = 32;
const CRYPTO_MIN_TOKEN_BYTES: usize = 16;
const CRYPTO_MAX_TOKEN_BYTES: usize = 64;

fn crypto_error(code: &'static str, message: impl Into<String>) -> ModuleError {
    ModuleError {
        message: format!(
            "{code} {}\\nhelp: {PUBLIC_HELP}#{}",
            message.into(),
            code.to_ascii_lowercase()
        ),
    }
}

fn crypto_expect_arity(
    function: &str,
    args: &[Value],
    expected: usize,
) -> Result<(), ModuleError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(crypto_error(
            "CRY1001",
            format!(
                "crypto.{function}() expects {expected} argument(s), got {}",
                args.len()
            ),
        ))
    }
}

fn crypto_string<'a>(
    function: &str,
    args: &'a [Value],
    index: usize,
    label: &str,
    max_bytes: usize,
) -> Result<&'a str, ModuleError> {
    let Some(Value::String(value)) = args.get(index) else {
        return Err(crypto_error(
            "CRY1001",
            format!("crypto.{function}() {label} must be a string"),
        ));
    };
    if value.len() > max_bytes {
        return Err(crypto_error(
            "CRY1001",
            format!(
                "crypto.{function}() {label} exceeds the {max_bytes}-byte limit"
            ),
        ));
    }
    Ok(value)
}

fn crypto_length(
    function: &str,
    value: Option<&Value>,
    min: usize,
    max: usize,
) -> Result<usize, ModuleError> {
    let Some(Value::Number(value)) = value else {
        return Err(crypto_error(
            "CRY1001",
            format!("crypto.{function}() length must be a number"),
        ));
    };
    if !value.is_finite()
        || value.fract() != 0.0
        || *value < min as f64
        || *value > max as f64
    {
        return Err(crypto_error(
            "CRY1001",
            format!(
                "crypto.{function}() length must be an integer from {min} through {max}"
            ),
        ));
    }
    Ok(*value as usize)
}

fn secure_random_bytes(length: usize) -> Result<Vec<u8>, ModuleError> {
    use rand::RngCore;
    let mut bytes = vec![0_u8; length];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| {
            crypto_error(
                "CRY3001",
                format!("operating-system secure random generation failed: {error}"),
            )
        })?;
    Ok(bytes)
}

fn call_crypto(function_name: &str, args: &[Value]) -> Result<Value, ModuleError> {
    match function_name {
        "hash" | "sha256" => {
            crypto_expect_arity(function_name, args, 1)?;
            let input = crypto_string(
                function_name,
                args,
                0,
                "input",
                CRYPTO_MAX_INPUT_BYTES,
            )?;
            use sha2::{Digest, Sha256};
            Ok(Value::String(hex::encode(Sha256::digest(input.as_bytes()))))
        }
        "hmacSha256" | "hmac_sha256" => {
            crypto_expect_arity(function_name, args, 2)?;
            let key = crypto_string(
                function_name,
                args,
                0,
                "key",
                CRYPTO_MAX_HMAC_KEY_BYTES,
            )?;
            if key.is_empty() {
                return Err(crypto_error(
                    "CRY1001",
                    format!("crypto.{function_name}() key must not be empty"),
                ));
            }
            let input = crypto_string(
                function_name,
                args,
                1,
                "input",
                CRYPTO_MAX_INPUT_BYTES,
            )?;
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).map_err(|_| {
                crypto_error("CRY1001", "crypto.hmacSha256() key is invalid")
            })?;
            mac.update(input.as_bytes());
            Ok(Value::String(hex::encode(mac.finalize().into_bytes())))
        }
        "randomBytes" | "random_bytes" => {
            crypto_expect_arity(function_name, args, 1)?;
            let length = crypto_length(
                function_name,
                args.first(),
                1,
                CRYPTO_MAX_RANDOM_BYTES,
            )?;
            Ok(Value::Array(
                secure_random_bytes(length)?
                    .into_iter()
                    .map(|byte| Value::Number(f64::from(byte)))
                    .collect(),
            ))
        }
        "randomToken" | "random_token" => {
            if args.len() > 1 {
                return Err(crypto_error(
                    "CRY1001",
                    format!(
                        "crypto.{function_name}() expects zero or one argument, got {}",
                        args.len()
                    ),
                ));
            }
            let length = match args.first() {
                Some(value) => crypto_length(
                    function_name,
                    Some(value),
                    CRYPTO_MIN_TOKEN_BYTES,
                    CRYPTO_MAX_TOKEN_BYTES,
                )?,
                None => CRYPTO_DEFAULT_TOKEN_BYTES,
            };
            Ok(Value::String(hex::encode(secure_random_bytes(length)?)))
        }
        other => Err(crypto_error(
            "CRY1002",
            format!("crypto.{other}() does not exist"),
        )),
    }
}
'''
if text.count(old_crypto) != 1:
    raise SystemExit(f"call_crypto anchor drifted: found {text.count(old_crypto)}")
text = text.replace(old_crypto, new_crypto, 1)

anchor = '''    #[test]\n    fn unknown_private_function_is_reported() {\n'''
tests = r'''    #[test]
    fn crypto_hash_and_sha256_use_sha256() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("crypto".into())]);
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        for function in ["hash", "sha256"] {
            let Value::String(digest) = registry
                .call("crypto", function, &[Value::String("abc".into())])
                .expect("SHA-256 call")
            else {
                panic!("expected digest string");
            };
            assert_eq!(digest, expected);
        }
    }

    #[test]
    fn crypto_hmac_sha256_matches_known_vector() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("crypto".into())]);
        let Value::String(mac) = registry
            .call(
                "crypto",
                "hmacSha256",
                &[
                    Value::String("key".into()),
                    Value::String("The quick brown fox jumps over the lazy dog".into()),
                ],
            )
            .expect("HMAC-SHA256 call")
        else {
            panic!("expected HMAC string");
        };
        assert_eq!(
            mac,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn crypto_random_generation_is_bounded_and_structured() {
        let registry = ModuleRegistry::from_imports(&[ImportTarget::Builtin("crypto".into())]);
        let Value::Array(bytes) = registry
            .call("crypto", "randomBytes", &[Value::Number(32.0)])
            .expect("random bytes")
        else {
            panic!("expected byte array");
        };
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|value| matches!(value, Value::Number(byte) if *byte >= 0.0 && *byte <= 255.0 && byte.fract() == 0.0)));

        let Value::String(token) = registry
            .call("crypto", "randomToken", &[])
            .expect("random token")
        else {
            panic!("expected token string");
        };
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let error = registry
            .call("crypto", "randomToken", &[Value::Number(8.0)])
            .expect_err("short tokens must fail");
        assert!(error.message.starts_with("CRY1001 "));
        assert!(error.message.contains("kastrick.vercel.app/project/rbe/doc/error-codes/runtime#cry1001"));
    }

'''
if text.count(anchor) != 1:
    raise SystemExit("modules test insertion anchor drifted")
text = text.replace(anchor, tests + anchor, 1)
modules.write_text(text)

# Error Code Book: CRY is a runtime-language crypto namespace and lives in the
# runtime book so the offline lookup needs no new embedded Markdown page.
replace_once(
    "doc/error-codes/README.md",
    '| `VID` | Video Manager | [Runtime](runtime.md#video-manager-codes) |\n',
    '| `VID` | Video Manager | [Runtime](runtime.md#video-manager-codes) |\n| `CRY` | REL cryptography/authentication primitives | [Runtime](runtime.md#crypto-codes) |\n',
    "Error Code Book CRY prefix table",
)
replace_once(
    "doc/error-codes/runtime.md",
    'This page covers backend/runtime (`RBE`), Error Reporter (`ER`), Vault (`VLT`) and Video Manager (`VID`) diagnostic namespaces.\n',
    'This page covers backend/runtime (`RBE`), Error Reporter (`ER`), Vault (`VLT`), Video Manager (`VID`) and REL cryptography (`CRY`) diagnostic namespaces.\n',
    "runtime book namespace summary",
)
replace_once(
    "doc/error-codes/runtime.md",
    '<a id="video-manager-codes"></a>\n## Video Manager codes\n',
    '''<a id="crypto-codes"></a>
## REL cryptography codes

| Range | Meaning |
| --- | --- |
| `CRY1000-1999` | crypto API input/operation validation |
| `CRY3000-3999` | secure entropy/runtime preparation failures |
| `CRY4000-4999` | password/authentication crypto execution |
| `CRY9000-9099` | crypto invariants / probable RBE bugs |

<a id="cry1001"></a>
### CRY1001 — invalid cryptography argument

**Status:** Emitted.

A REL cryptography helper received the wrong number/type of arguments or a value outside its bounded input/length policy.

**Action:** use the function signature and limits shown in the diagnostic. Do not remove the limits to accept attacker-controlled unbounded crypto work.

<a id="cry1002"></a>
### CRY1002 — unknown cryptography operation

**Status:** Emitted.

REL attempted to call a function that the `crypto` builtin does not export.

**Action:** use an explicitly supported crypto operation; do not infer undocumented host cryptography APIs.

<a id="cry3001"></a>
### CRY3001 — secure random generation failed

**Status:** Emitted.

The operating-system cryptographically secure random generator could not supply the requested entropy. RBE fails closed rather than substituting a predictable PRNG.

**Action:** inspect the host entropy/platform failure. Do not replace this failure with timestamps, counters or non-cryptographic randomness.

<a id="video-manager-codes"></a>
## Video Manager codes
''',
    "runtime book crypto section",
)

catalog = Path("doc/error-codes/catalog.json")
text = catalog.read_text()
text = text.replace(
    '    "VID": "Video Manager"\n',
    '    "VID": "Video Manager",\n    "CRY": "REL cryptography/authentication primitives"\n',
    1,
)
if '"CRY": "REL cryptography/authentication primitives"' not in text:
    raise SystemExit("catalog CRY prefix patch failed")
needle = '''    {
      "code": "VID5001",
      "status": "reserved",
      "title": "Required media tool/capability unavailable",
      "doc": "runtime.md#vid5001"
    },
'''
entries = '''    {
      "code": "CRY1001",
      "status": "emitted",
      "title": "Invalid cryptography argument",
      "doc": "runtime.md#cry1001"
    },
    {
      "code": "CRY1002",
      "status": "emitted",
      "title": "Unknown cryptography operation",
      "doc": "runtime.md#cry1002"
    },
    {
      "code": "CRY3001",
      "status": "emitted",
      "title": "Secure random generation failed",
      "doc": "runtime.md#cry3001"
    },
'''
if text.count(needle) != 1:
    raise SystemExit("catalog CRY entry insertion anchor drifted")
text = text.replace(needle, entries + needle, 1)
catalog.write_text(text)
