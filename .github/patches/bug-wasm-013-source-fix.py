from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
old = '                br#"["users/kate.json"]"#,'
new = '                br#""users/kate.json""#,'
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-013 dynamic body test input expected once, found {count}")
path.write_text(text.replace(old, new, 1))
