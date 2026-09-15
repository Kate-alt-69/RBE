from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
old = '                br#"["users/kate.json"]"#,'
new = '                br#""users/kate.json""#,'
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-013 dynamic body test input expected once, found {count}")
text = text.replace(old, new, 1)

old = "data.active(0, &ConstExpr::i32_const(payload_offset as i32), [b'[']);"
new = 'data.active(0, &ConstExpr::i32_const(payload_offset as i32), *b"[");'
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-013 byte-slice clippy anchor expected once, found {count}")
path.write_text(text.replace(old, new, 1))

path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()
old = "crate::wasm_compiler::RouteWasmInput::JsonBodyCapabilityArgument"
new = "crate::wasm_compiler::RouteWasmInput::JsonBodyCapabilityValue"
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-013 stale RELC input variant expected once, found {count}")
path.write_text(text.replace(old, new, 1))
