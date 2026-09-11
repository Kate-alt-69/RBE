from pathlib import Path

path = Path('.github/patches/container-cancel-lifecycle.py')
text = path.read_text(encoding='utf-8')
needle = '''# The isolated-worker loop has the remaining cancellation probe.\nrep(runtime, '        if is_cancelled(cancelled, task) {', '        if is_cancelled(lifecycle, task) {')\n'''
if text.count(needle) != 1:
    raise SystemExit(f'expected one redundant cancellation-probe rewrite, found {text.count(needle)}')
path.write_text(text.replace(needle, '', 1), encoding='utf-8')
