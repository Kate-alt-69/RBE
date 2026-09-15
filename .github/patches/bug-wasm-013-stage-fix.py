from pathlib import Path

path = Path(".github/patches/bug-wasm-013.py")
text = path.read_text()
old = '''text = replace_once(
    text,
    '                br#"[\\\\"users/kate.json\\\\"]"#,.decode()' if False else '                br#"[\\\\"users/kate.json\\\\"]"#,',
    '                br#"\\\\"users/kate.json\\\\""#,',
    "dynamic capability raw body executor input",
)'''
new = '''text = replace_once(
    text,
    r''' + "'''" + '''                br#"["users/kate.json"]"#,''' + "'''" + ''',
    r''' + "'''" + '''                br#""users/kate.json""#,''' + "'''" + ''',
    "dynamic capability raw body executor input",
)'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"BUG-WASM-013 raw-string staging anchor expected once, found {count}")
path.write_text(text.replace(old, new, 1))
