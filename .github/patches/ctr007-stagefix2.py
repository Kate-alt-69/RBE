from pathlib import Path

path = Path('container-runtime/crates/container-bin/src/main.rs')
text = path.read_text(encoding='utf-8')

old = '''    let environment_processes = environment_processes
        .snapshots()'''
new = '''    let environment_process_snapshots = environment_processes
        .snapshots()'''
if text.count(old) != 1:
    raise SystemExit(f'inspection snapshot variable anchor count={text.count(old)}')
text = text.replace(old, new, 1)

old = '''        "environment_processes": environment_processes,
        "security": {'''
new = '''        "environment_processes": environment_process_snapshots,
        "security": {'''
if text.count(old) != 1:
    raise SystemExit(f'inspection snapshot output anchor count={text.count(old)}')
text = text.replace(old, new, 1)

path.write_text(text, encoding='utf-8')
