from pathlib import Path

# Semantic guard: descriptor lowering must remain tied to the imported Storage
# capability, never to a user helper merely named `write`. This file is also
# intentionally modified while the first one-shot is running so that its final
# delete/rebase cannot land the over-broad identifier-based rewrite.
path = Path('.github/patches/storage-rel-write-v1.py')
text = path.read_text(encoding='utf-8')
for old, new in [
    ('args = normalize_storage_write_args(&expr, args)?;', 'args = Self::normalize_storage_write_args(&expr, args)?;'),
    ('let Some((kind, value)) = storage_descriptor(arg) else {', 'let Some((kind, value)) = Self::storage_descriptor(arg) else {'),
]:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f'expected one staging fix anchor for {old!r}, found {count}')
    text = text.replace(old, new, 1)
path.write_text(text, encoding='utf-8')
