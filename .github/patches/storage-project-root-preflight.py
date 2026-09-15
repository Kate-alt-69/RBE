from pathlib import Path

path = Path('.github/patches/storage-project-root.py')
text = path.read_text(encoding='utf-8')

# The module registry contains the same Storage mapping in the namespace-import
# and exact-function-import branches. Keep replace_once strict everywhere else,
# but allow these two intentional sequential replacements.
old_helper = '''    count = text.count(old)\n    if count != 1:\n        raise SystemExit(f'{path}: expected exactly one anchor, found {count}: {old[:120]!r}')\n    file.write_text(text.replace(old, new, 1), encoding='utf-8')'''
new_helper = '''    count = text.count(old)\n    duplicate_storage_registry = (\n        path == 'engine/crates/route-engine/src/modules.rs'\n        and '"storage" | "Storage" => ModuleKind::Builtin(BuiltinModule::Storage)' in old\n    )\n    if count != 1 and not (duplicate_storage_registry and count >= 1):\n        raise SystemExit(f'{path}: expected exactly one anchor, found {count}: {old[:120]!r}')\n    file.write_text(text.replace(old, new, 1), encoding='utf-8')'''
if text.count(old_helper) != 1:
    raise SystemExit(f'expected replace_once helper once, found {text.count(old_helper)}')
text = text.replace(old_helper, new_helper, 1)

# project_relative() intentionally parses the $$ marker into a safe *relative*
# path. The trusted Controller is the only layer that joins it to the canonical
# backend startup project root.
old = '''        let (relative, target) = project_relative(raw_path, false)?;\n        let parent = relative'''
new = '''        let (relative, _) = project_relative(raw_path, false)?;\n        let target = self.root.join(&relative);\n        let parent = relative'''
if text.count(old) != 1:
    raise SystemExit(f'expected prepare_write_target anchor once, found {text.count(old)}')
text = text.replace(old, new, 1)

old = '''        let (_, target) = project_relative(raw_path, allow_directory)?;\n        let metadata = match fs::symlink_metadata(&target) {'''
new = '''        let (relative, _) = project_relative(raw_path, allow_directory)?;\n        let target = self.root.join(relative);\n        let metadata = match fs::symlink_metadata(&target) {'''
if text.count(old) != 1:
    raise SystemExit(f'expected resolve_existing anchor once, found {text.count(old)}')
text = text.replace(old, new, 1)

path.write_text(text, encoding='utf-8')
print('storage project-root staging preflight fixed')
