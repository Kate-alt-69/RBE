from pathlib import Path

path = Path(".github/patches/bug-wasm-013.py")
text = path.read_text()
label = '    "dynamic capability raw body executor input",'
pos = text.find(label)
if pos < 0:
    raise SystemExit("BUG-WASM-013 raw-body staging label missing")
start = text.rfind("text = replace_once(", 0, pos)
if start < 0:
    raise SystemExit("BUG-WASM-013 raw-body replace block start missing")
end = text.find("\n)\n", pos)
if end < 0:
    raise SystemExit("BUG-WASM-013 raw-body replace block end missing")
end += len("\n)\n")
path.write_text(text[:start] + text[end:])
