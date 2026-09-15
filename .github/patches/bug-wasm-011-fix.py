from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
old = '''    let host = host_imports.get(host_binding).ok_or_else(|| {
        format!(
            "native linked Module return target {host_binding:?} is not one exact HTTP, Video, Service, or Storage import"
        )
    })?;'''
new = '''    let host = host_imports.get(host_binding).cloned().ok_or_else(|| {
        format!(
            "native linked Module return target {host_binding:?} is not one exact HTTP, Video, Service, or Storage import"
        )
    })?;'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-011 selected host ownership anchor expected once, found {count}")
path.write_text(text.replace(old, new, 1))
