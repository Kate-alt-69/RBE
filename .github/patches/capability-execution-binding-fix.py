from pathlib import Path

path = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
text = path.read_text(encoding="utf-8")
old = "        assert_eq!(decoded.generation, 7);\n"
if text.count(old) != 1:
    raise SystemExit(f"expected one stale decoded.generation assertion, found {text.count(old)}")
text = text.replace(old, "", 1)
path.write_text(text, encoding="utf-8")
