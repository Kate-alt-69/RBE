from pathlib import Path

path = Path('.github/patches/container-env-process.py')
text = path.read_text(encoding='utf-8')

old = '\"                    inspection_body(\\n'
new = '\"                    body: inspection_body(\\n'
count = text.count(old)
if count != 2:
    raise SystemExit(f'expected two inspection_body patch anchors, found {count}')
text = text.replace(old, new)

# The staged Environment module does not execute Wasmtime directly; it forwards
# work to the existing disposable --worker role, so do not keep a dead import.
unused = 'use execution_engine::WasmExecutor;\\n'
if unused not in text:
    raise SystemExit('missing staged WasmExecutor import')
text = text.replace(unused, '', 1)

path.write_text(text, encoding='utf-8')
