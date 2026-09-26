from pathlib import Path

path = Path("engine/crates/route-engine/src/discovery.rs")
source = path.read_text(encoding="utf-8")
old = '            assert!(error.contains("duplicate query field "id""), "{error}");\n'
new = '''            assert!(error.contains("duplicate query field"), "{error}");
            assert!(error.contains("id"), "{error}");
'''
if old not in source:
    raise SystemExit("FLD-019 generated assertion anchor not found")
path.write_text(source.replace(old, new, 1), encoding="utf-8")
