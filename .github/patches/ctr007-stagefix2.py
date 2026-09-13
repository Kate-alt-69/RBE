from pathlib import Path

path = Path('.github/patches/ctr007-worker-crash-circuit.py')
text = path.read_text(encoding='utf-8')

marker = '''main = Path("container-runtime/crates/container-bin/src/main.rs")
m = main.read_text(encoding="utf-8")
'''
insert = '''main = Path("container-runtime/crates/container-bin/src/main.rs")
m = main.read_text(encoding="utf-8")
m = one(
    m,
    ''' + "'''" + '''    let environment_processes = environment_processes
        .snapshots()''' + "'''" + ''',
    ''' + "'''" + '''    let environment_process_snapshots = environment_processes
        .snapshots()''' + "'''" + ''',
    "inspection Environment snapshot variable",
)
m = one(
    m,
    ''' + "'''" + '''        "environment_processes": environment_processes,
        "security": {''' + "'''" + ''',
    ''' + "'''" + '''        "environment_processes": environment_process_snapshots,
        "security": {''' + "'''" + ''',
    "inspection Environment snapshot output",
)
'''
if text.count(marker) != 1:
    raise SystemExit(f'main patch marker count={text.count(marker)}')
text = text.replace(marker, insert, 1)
path.write_text(text, encoding='utf-8')
