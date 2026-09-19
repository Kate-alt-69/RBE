from pathlib import Path

path = Path('.github/patches/field-manager-foundation.py')
text = path.read_text(encoding='utf-8')

helper = '''def replace_first(path: str, old: str, new: str, label: str) -> None:\n    file = Path(path)\n    text = file.read_text(encoding="utf-8")\n    if old not in text:\n        raise SystemExit(f"{label}: anchor not found")\n    file.write_text(text.replace(old, new, 1), encoding="utf-8")\n\n\n'''
anchor = 'from pathlib import Path\n\n\n'
if helper not in text:
    if anchor not in text:
        raise SystemExit('patch helper insertion anchor not found')
    text = text.replace(anchor, anchor + helper, 1)

label = '"namespace math/regx registry"'
pos = text.find(label)
if pos < 0:
    raise SystemExit('namespace math/regx label not found')
start = text.rfind('replace_once(', 0, pos)
if start < 0:
    raise SystemExit('namespace math/regx replace_once call not found')
text = text[:start] + 'replace_first(' + text[start + len('replace_once('):]

path.write_text(text, encoding='utf-8')
