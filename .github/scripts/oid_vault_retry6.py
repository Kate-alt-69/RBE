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
Path('/tmp/run-oid-vault-v2.sh').write_text(body)
