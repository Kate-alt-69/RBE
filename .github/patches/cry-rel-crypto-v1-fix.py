from pathlib import Path

path = Path('.github/patches/cry-rel-crypto-v1.py')
text = path.read_text()
old = '            "{code} {}\\\\nhelp: {PUBLIC_HELP}#{}",\n'
new = '            "{code} {}\\\\nhelp: https://kastrick.vercel.app/project/rbe/doc/error-codes/runtime#{}",\n'
count = text.count(old)
if count != 1:
    raise SystemExit(f'CRY-001 PUBLIC_HELP generator anchor: expected 1, found {count}')
path.write_text(text.replace(old, new, 1))
