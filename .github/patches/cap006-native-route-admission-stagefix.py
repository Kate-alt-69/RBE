from pathlib import Path

path = Path('.github/patches/cap006-native-route-admission.py')
text = path.read_text(encoding='utf-8')
old = '''replace_once(
    discovery,
    '''use crate::server_rel::ServerProgram;\nuse crate::terminal::Terminal;''',
    '''use crate::server_rel::ServerProgram;\nuse crate::source_registry::SourceId;\nuse crate::terminal::Terminal;''',
    "route SourceId import",
)'''
new = '''replace_once(
    discovery,
    '''use crate::runtime_image::RuntimeImage;\nuse crate::terminal::Terminal;''',
    '''use crate::runtime_image::RuntimeImage;\nuse crate::source_registry::SourceId;\nuse crate::terminal::Terminal;''',
    "route SourceId import",
)'''
if text.count(old) != 1:
    raise SystemExit(f"CAP-006 SourceId staging anchor changed: {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding='utf-8')
