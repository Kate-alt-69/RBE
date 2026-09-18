from pathlib import Path
import json


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one Container diagnostic anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


path = "engine/crates/backend/src/container_process.rs"
replace_once(
    path,
    '''fn verify_container(binary: &Path) -> anyhow::Result<()> {
''',
    '''fn container_dependency_missing(binary: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5001 Required packaged Container runtime is missing.\\n\\n  expected_path:\\n    {}\\n\\n  action:\\n    Rebuild/reinstall the complete RBE package for this target. Do not mix a Container binary from another build into this package.\\n\\n  help:\\n    doc/error-codes/runtime.md#rbe5001",
        binary.display()
    )
}

fn container_binding_invalid(binary: &Path, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5002 Backend Container binding metadata is invalid.\\n\\n  container_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild the complete RBE package for this target so backend and Container integrity metadata are generated together.\\n\\n  help:\\n    doc/error-codes/runtime.md#rbe5002",
        binary.display(),
        reason
    )
}

fn container_integrity_failed(binary: &Path, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5003 Packaged Container runtime failed backend integrity verification.\\n\\n  container_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Replace the package with a complete RBE build produced for this target. Do not copy Container binaries between backend builds.\\n\\n  help:\\n    doc/error-codes/runtime.md#rbe5003",
        binary.display(),
        reason
    )
}

fn verify_container(binary: &Path) -> anyhow::Result<()> {
''',
)
replace_once(
    path,
    '''    if container_integrity::EXPECTED_CONTAINER_SHA256.is_empty()
        || container_integrity::CONTAINER_PUBLIC_KEY_HEX.is_empty()
        || container_integrity::CONTAINER_SIGNATURE_HEX.is_empty()
    {
        anyhow::bail!("container dependency is not cryptographically bound to this backend build; refusing startup");
    }
    if !binary.is_file() {
        anyhow::bail!(
            "required container dependency is missing: {}",
            binary.display()
        );
    }

    let actual_hash = sha256_file(binary)?;
''',
    '''    if container_integrity::EXPECTED_CONTAINER_SHA256.is_empty()
        || container_integrity::CONTAINER_PUBLIC_KEY_HEX.is_empty()
        || container_integrity::CONTAINER_SIGNATURE_HEX.is_empty()
    {
        return Err(container_binding_invalid(
            binary,
            "required SHA-256/public-key/signature metadata was not embedded in this backend build",
        ));
    }
    if !binary.is_file() {
        return Err(container_dependency_missing(binary));
    }

    let actual_hash = sha256_file(binary).map_err(|error| {
        container_integrity_failed(binary, format!("could not read/hash Container binary: {error}"))
    })?;
''',
)
replace_once(
    path,
    '''        anyhow::bail!(
            "container integrity check failed: SHA-256 mismatch (expected {}, got {})",
            container_integrity::EXPECTED_CONTAINER_SHA256,
            actual_hash
        );
''',
    '''        return Err(container_integrity_failed(
            binary,
            format!(
                "SHA-256 mismatch (expected {}, got {})",
                container_integrity::EXPECTED_CONTAINER_SHA256,
                actual_hash
            ),
        ));
''',
)
replace_once(
    path,
    '''    let public_key_bytes = decode_exact::<32>(
        container_integrity::CONTAINER_PUBLIC_KEY_HEX,
        "container public key",
    )?;
    let signature_bytes = decode_exact::<64>(
        container_integrity::CONTAINER_SIGNATURE_HEX,
        "container signature",
    )?;
    let public_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|err| anyhow::anyhow!("invalid embedded container public key: {err}"))?;
''',
    '''    let public_key_bytes = decode_exact::<32>(
        container_integrity::CONTAINER_PUBLIC_KEY_HEX,
        "container public key",
    )
    .map_err(|error| container_binding_invalid(binary, error))?;
    let signature_bytes = decode_exact::<64>(
        container_integrity::CONTAINER_SIGNATURE_HEX,
        "container signature",
    )
    .map_err(|error| container_binding_invalid(binary, error))?;
    let public_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|error| container_binding_invalid(binary, format!("invalid embedded Container public key: {error}")))?;
''',
)
replace_once(
    path,
    '''    public_key
        .verify(statement.as_bytes(), &signature)
        .map_err(|err| anyhow::anyhow!("container signature verification failed: {err}"))?;
''',
    '''    public_key
        .verify(statement.as_bytes(), &signature)
        .map_err(|error| {
            container_integrity_failed(
                binary,
                format!("Container signature verification failed: {error}"),
            )
        })?;
''',
)

# Add classification tests without depending on build-generated integrity values.
p = Path(path)
text = p.read_text()
anchor = '''    #[test]
    fn constant_time_compare_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
'''
replacement = anchor + '''
    #[test]
    fn container_package_diagnostics_are_stable_and_actionable() {
        let path = Path::new("dep/container");

        let missing = container_dependency_missing(path).to_string();
        assert!(missing.starts_with("RBE5001 "));
        assert!(missing.contains("expected_path:"));
        assert!(missing.contains("doc/error-codes/runtime.md#rbe5001"));

        let binding = container_binding_invalid(path, "synthetic binding failure").to_string();
        assert!(binding.starts_with("RBE5002 "));
        assert!(binding.contains("synthetic binding failure"));
        assert!(binding.contains("doc/error-codes/runtime.md#rbe5002"));

        let integrity = container_integrity_failed(path, "synthetic integrity failure").to_string();
        assert!(integrity.starts_with("RBE5003 "));
        assert!(integrity.contains("synthetic integrity failure"));
        assert!(integrity.contains("doc/error-codes/runtime.md#rbe5003"));
    }
'''
if text.count(anchor) != 1:
    raise SystemExit("container_process.rs: test anchor drifted")
p.write_text(text.replace(anchor, replacement, 1))

# Insert the new package compatibility codes before the generic boot wrapper.
path = "doc/error-codes/runtime.md"
p = Path(path)
text = p.read_text()
marker = '<a id="rbe5099"></a>\n'
if text.count(marker) != 1:
    raise SystemExit("runtime.md: RBE5099 anchor drifted")
entries = '''<a id="rbe5002"></a>
### RBE5002 — backend Container binding metadata is invalid

**Status:** Emitted.

The backend build does not contain a complete, valid integrity binding for its packaged Container runtime. This is a package/build compatibility problem, not an instruction to disable verification.

**Action:** rebuild the complete RBE package for the same target so backend and Container metadata are generated together.

<a id="rbe5003"></a>
### RBE5003 — packaged Container failed backend integrity verification

**Status:** Emitted.

The packaged Container could not be read/hashed or its hash/signature does not match the artifact cryptographically bound to this backend build.

**Action:** replace the complete package with a coherent build for the same target. Do not copy `container`/`container.exe` between backend builds.

'''
p.write_text(text.replace(marker, entries + marker, 1))

path = "doc/error-codes/catalog.json"
p = Path(path)
data = json.loads(p.read_text())
items = data["entries"]
by_code = {item["code"]: item for item in items}
for code in ("RBE5002", "RBE5003"):
    if code in by_code:
        raise SystemExit(f"catalog already contains {code}")
index = next(i for i, item in enumerate(items) if item["code"] == "RBE5099")
items[index:index] = [
    {
        "code": "RBE5002",
        "status": "emitted",
        "title": "Backend Container binding metadata is invalid",
        "doc": "runtime.md#rbe5002",
    },
    {
        "code": "RBE5003",
        "status": "emitted",
        "title": "Packaged Container failed backend integrity verification",
        "doc": "runtime.md#rbe5003",
    },
]
p.write_text(json.dumps(data, indent=2) + "\n")
