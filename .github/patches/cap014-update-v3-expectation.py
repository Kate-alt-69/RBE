from pathlib import Path

path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text(encoding="utf-8")
old = '.contains("outside the native Route-WASM v2 subset"));'
new = '.contains("outside the native Route-WASM v3 subset"));'
if text.count(old) != 1:
    raise SystemExit(f"RELC fallback version expectation changed: {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
