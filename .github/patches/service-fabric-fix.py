from pathlib import Path
import re

manager = Path('engine/crates/service-runtime/src/manager.rs')
text = manager.read_text(encoding='utf-8')
text, count = re.subn(
    r'\n    fn running\(file: ServiceFile, process: ServiceProcess\) -> Self \{.*?\n    \}\n(?=\n    fn wakeable)',
    '',
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit('failed to remove obsolete Managed::running constructor')
manager.write_text(text, encoding='utf-8')

mother = Path('engine/crates/backend/src/service_mother.rs')
text = mother.read_text(encoding='utf-8')
old = '''            Ok(())
        }
        None => server_task.await??,
'''
new = '''        }
        None => server_task.await??,
'''
if old not in text:
    raise SystemExit('failed to find Service Mother match result fixup')
mother.write_text(text.replace(old, new, 1), encoding='utf-8')
