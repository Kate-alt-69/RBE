from pathlib import Path

path = Path('.github/patches/diag-rel-errors.py')
text = path.read_text()

old = '''if capability_count != 8:
    raise SystemExit(f"relc.rs: expected 8 capability constructors, found {capability_count}")
'''
new = '''if capability_count != 11:
    raise SystemExit(f"relc.rs: expected 11 capability constructors, found {capability_count}")
'''
if text.count(old) != 1:
    raise SystemExit('DIAG constructor-count anchor drifted')
text = text.replace(old, new, 1)

anchor = '''specialize_capability("unsupported Environment Storage operation", "RELC2102")
'''
replacement = '''specialize_capability("unsupported Environment Storage operation", "RELC2102")
specialize_capability("cannot be used as an Environment Storage namespace", "RELC2102")
'''
if text.count(anchor) != 1:
    raise SystemExit('DIAG Storage-owner classification anchor drifted')
text = text.replace(anchor, replacement, 1)

path.write_text(text)
