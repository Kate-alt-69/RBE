from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
old = 'entries.sort_by(|(left, _), (right, _)| left.cmp(right));'
new = 'entries.sort_by_key(|(left, _)| *left);'
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-006 clippy anchor count={count}")
path.write_text(text.replace(old, new, 1))
