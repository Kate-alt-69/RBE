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
# RngCore trait must be imported at module scope (the old local import is not
# enough). Also repair the current standalone Service baseline: the package
# catalog needs only the shared approval reader, not the backend-only Library
# Host package module. Finally make the OID lock OpenOptions explicit: the lock
# file is persistent coordination state, so opening it must never truncate it.
rustfmt_marker = '\nrustfmt --edition 2021 '
if rustfmt_marker not in body:
    raise SystemExit('could not locate rustfmt boundary in v2 staging body')
preflight_repairs = r'''
python3 - <<'PY'
from pathlib import Path

p = Path('vault-process/src/lib.rs')
text = p.read_text()
anchor = 'use aes_gcm::{Aes256Gcm, Key, Nonce};\n'
module_import = anchor + 'use rand::RngCore;\n'
if module_import not in text:
    if anchor not in text:
        raise SystemExit('vault-process: AES-GCM import anchor missing')
    text = text.replace(anchor, module_import, 1)
p.write_text(text)

p = Path('engine/crates/backend/src/service_package_catalog.rs')
text = p.read_text()
module_anchor = 'use service_runtime::ServiceCatalog;\n\n'
module_decl = 'use service_runtime::ServiceCatalog;\n\n#[path = "package_links/approval.rs"]\nmod package_approval;\n\n'
if '#[path = "package_links/approval.rs"]' not in text:
    if module_anchor not in text:
        raise SystemExit('service_package_catalog: import anchor missing')
    text = text.replace(module_anchor, module_decl, 1)
text = text.replace(
    'crate::package_links::approval::approved_runtime_capabilities',
    'package_approval::approved_runtime_capabilities',
)
p.write_text(text)

p = Path('engine/crates/route-engine/src/oid_security.rs')
text = p.read_text()
lock_anchor = '''        let lease = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
'''
lock_fixed = '''        let lease = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
'''
if lock_fixed not in text:
    if lock_anchor not in text:
        raise SystemExit('oid_security: lock OpenOptions anchor missing')
    text = text.replace(lock_anchor, lock_fixed, 1)
p.write_text(text)
PY
'''
body = body.replace(rustfmt_marker, preflight_repairs + rustfmt_marker, 1)

# Ensure the baseline repair is formatted and included in the guarded commit.
body = body.replace(
    'engine/crates/backend/src/main.rs\n',
    'engine/crates/backend/src/main.rs \\\n    engine/crates/backend/src/service_package_catalog.rs\n',
    1,
)
body = body.replace(
    'engine/crates/backend/src/main.rs \\\n    engine/Cargo.lock',
    'engine/crates/backend/src/main.rs \\\n    engine/crates/backend/src/service_package_catalog.rs \\\n    engine/Cargo.lock',
    1,
)

Path('/tmp/run-oid-vault-v2.sh').write_text(body)
