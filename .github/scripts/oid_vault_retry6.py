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

# The original v2 body used fill_bytes in vault-process but its import patch was
# lost during earlier staging retries. Ensure the generated source imports the
# trait before rustfmt/cargo are invoked.
rustfmt_marker = '\nrustfmt --edition 2021 '
if rustfmt_marker not in body:
    raise SystemExit('could not locate rustfmt boundary in v2 staging body')
ensure_rng = r'''
python3 - <<'PY'
from pathlib import Path
p = Path('vault-process/src/lib.rs')
text = p.read_text()
if 'use rand::RngCore;' not in text:
    anchor = 'use aes_gcm::{Aes256Gcm, Key, Nonce};\n'
    if anchor not in text:
        raise SystemExit('vault-process: AES-GCM import anchor missing')
    text = text.replace(anchor, anchor + 'use rand::RngCore;\n', 1)
p.write_text(text)
PY
'''
body = body.replace(rustfmt_marker, ensure_rng + rustfmt_marker, 1)
Path('/tmp/run-oid-vault-v2.sh').write_text(body)
