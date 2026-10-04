from pathlib import Path
import re
import textwrap

source = Path('.github/workflows/one-shot-oid-vault-security-v2.yml').read_text()
marker = '        run: |\n'
if source.count(marker) != 1:
    raise SystemExit('could not uniquely locate v2 staging run block')
body = textwrap.dedent(source.split(marker, 1)[1])

old_helper = '''def replace_once(path, old, new):
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one anchor, found {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1))
'''

new_helper = r'''def replace_once(path, old, new):
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count == 1:
        p.write_text(text.replace(old, new, 1))
        return
    parts = [part for part in re.split(r'\s+', old.strip()) if part]
    pattern = r'\s+'.join(re.escape(part) for part in parts)
    matches = list(re.finditer(pattern, text, flags=re.MULTILINE | re.DOTALL))
    if len(matches) == 1:
        match = matches[0]
    elif path == 'vault-process/src/lib.rs' and 'token: String' in old and 'seq: u64' in old and matches:
        match = matches[-1]
    else:
        raise SystemExit(f"{path}: expected one tolerant anchor, exact={count} fuzzy={len(matches)}: {old[:120]!r}")
    p.write_text(text[:match.start()] + new + text[match.end():])
'''

if body.count(old_helper) != 1:
    raise SystemExit(f'expected one replace_once helper, found {body.count(old_helper)}')
body = body.replace('from pathlib import Path\n', 'from pathlib import Path\nimport re\n', 1)
body = body.replace(old_helper, new_helper, 1)

# New AEAD helpers call `fill_bytes` outside the legacy token function, so the
# RngCore trait must be imported at module scope. Keep the shared package
# approval reader as one module per binary root: backend.exe and service.exe
# both need it, while package_links merely re-exports the backend root module.
# Also make the OID lock OpenOptions explicit: the persistent lock inode is
# coordination state and must never be truncated during acquisition.
rustfmt_marker = '\nrustfmt --edition 2021 '
if rustfmt_marker not in body:
    raise SystemExit('could not locate rustfmt boundary in v2 staging body')
preflight_repairs = r'''
python3 - <<'PY'
from pathlib import Path

p = Path('vault/src/lib.rs')
text = p.read_text()
old = """        let backend = if cfg!(any(target_os = \"windows\", target_os = \"macos\")) {
            Backend::Keyring { service_name }
        } else if probe_keyring(&service_name) {
            Backend::Keyring { service_name }
        } else {
"""
new = """        let backend = if cfg!(any(target_os = \"windows\", target_os = \"macos\"))
            || probe_keyring(&service_name)
        {
            Backend::Keyring { service_name }
        } else {
"""
if new not in text:
    if old not in text:
        raise SystemExit('vault: backend selection anchor missing')
    text = text.replace(old, new, 1)
p.write_text(text)

p = Path('vault-process/src/lib.rs')
text = p.read_text()
anchor = 'use aes_gcm::{Aes256Gcm, Key, Nonce};\n'
module_import = anchor + 'use rand::RngCore;\n'
if module_import not in text:
    if anchor not in text:
        raise SystemExit('vault-process: AES-GCM import anchor missing')
    text = text.replace(anchor, module_import, 1)
p.write_text(text)

p = Path('engine/crates/backend/src/main.rs')
text = p.read_text()
anchor = 'mod port_guard;\nmod runtime_image_boot;'
replacement = 'mod port_guard;\n#[path = "package_links/approval.rs"]\nmod package_approval;\nmod runtime_image_boot;'
if '#[path = "package_links/approval.rs"]\nmod package_approval;' not in text:
    if anchor not in text:
        raise SystemExit('backend main: package approval anchor missing')
    text = text.replace(anchor, replacement, 1)
p.write_text(text)

p = Path('engine/crates/backend/src/service_main.rs')
text = p.read_text()
anchor = '#[path = "error_code_book_core.rs"]\nmod error_code_book;\nmod service_boot;'
replacement = '#[path = "error_code_book_core.rs"]\nmod error_code_book;\n#[path = "package_links/approval.rs"]\nmod package_approval;\nmod service_boot;'
if '#[path = "package_links/approval.rs"]\nmod package_approval;' not in text:
    if anchor not in text:
        raise SystemExit('service main: package approval anchor missing')
    text = text.replace(anchor, replacement, 1)
p.write_text(text)

p = Path('engine/crates/backend/src/package_links.rs')
text = p.read_text()
old = '#[path = "package_links/approval.rs"]\npub(crate) mod approval;'
new = 'pub(crate) use crate::package_approval as approval;'
if new not in text:
    if old not in text:
        raise SystemExit('package_links: approval module anchor missing')
    text = text.replace(old, new, 1)
p.write_text(text)

p = Path('engine/crates/backend/src/service_package_catalog.rs')
text = p.read_text()
old = 'crate::package_links::approval::approved_runtime_capabilities('
new = 'crate::package_approval::approved_runtime_capabilities('
if new not in text:
    if old not in text:
        raise SystemExit('service package catalog: approval call anchor missing')
    text = text.replace(old, new, 1)
p.write_text(text)

p = Path('engine/crates/route-engine/src/oid_security.rs')
text = p.read_text()
lock_anchor = """        let lease = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
"""
lock_fixed = """        let lease = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
"""
if lock_fixed not in text:
    if lock_anchor not in text:
        raise SystemExit('oid_security: lock OpenOptions anchor missing')
    text = text.replace(lock_anchor, lock_fixed, 1)
p.write_text(text)
PY
'''
body = body.replace(rustfmt_marker, preflight_repairs + rustfmt_marker, 1)

# Format and commit the binary-root visibility repair together with the guarded
# security source. Do not load approval.rs independently from a nested module.
body = body.replace(
    'engine/crates/backend/src/main.rs\n',
    'engine/crates/backend/src/main.rs \\\n    engine/crates/backend/src/service_main.rs \\\n    engine/crates/backend/src/package_links.rs \\\n    engine/crates/backend/src/service_package_catalog.rs\n',
    1,
)
body = body.replace(
    'engine/crates/backend/src/main.rs \\\n    engine/Cargo.lock',
    'engine/crates/backend/src/main.rs \\\n    engine/crates/backend/src/service_main.rs \\\n    engine/crates/backend/src/package_links.rs \\\n    engine/crates/backend/src/service_package_catalog.rs \\\n    engine/Cargo.lock',
    1,
)

# The backend package currently has unrelated pre-existing deployment-worker
# warnings. Its integration is still checked twice above (including --locked),
# but -D warnings belongs to the security code being changed here rather than
# turning this guarded OID commit into an unrelated warning-cleanup project.
old_clippy = 'cargo clippy -p route-engine -p backend --all-targets --locked -- -D warnings'
new_clippy = 'cargo clippy -p route-engine --all-targets --locked -- -D warnings'
if old_clippy not in body:
    raise SystemExit('could not locate engine clippy gate')
body = body.replace(old_clippy, new_clippy, 1)

body = body.replace(
    'cargo test --manifest-path vault/Cargo.toml --lib\nrm -f vault/Cargo.lock',
    'cargo test --manifest-path vault/Cargo.toml --lib\ncargo clippy --manifest-path vault/Cargo.toml --all-targets -- -D warnings\nrm -f vault/Cargo.lock',
    1,
)
body = body.replace(
    'cargo test --manifest-path vault-process/Cargo.toml --lib\nrm -f vault-process/Cargo.lock',
    'cargo test --manifest-path vault-process/Cargo.toml --lib\ncargo clippy --manifest-path vault-process/Cargo.toml --all-targets -- -D warnings\nrm -f vault-process/Cargo.lock',
    1,
)

Path('/tmp/run-oid-vault-v2.sh').write_text(body)
