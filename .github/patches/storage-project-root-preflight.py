from pathlib import Path

path = Path('.github/patches/storage-project-root.py')
text = path.read_text(encoding='utf-8')

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

block = '''replace_once(\n    'engine/crates/route-engine/src/modules.rs',\n    '                        \\"storage\\" | \\"Storage\\" => ModuleKind::Builtin(BuiltinModule::Storage),',\n    '                        \\"storage\\" | \\"Storage\\" | \\"envStorage\\" => ModuleKind::Builtin(BuiltinModule::Storage),',\n)\nreplace_once(\n    'engine/crates/route-engine/src/modules.rs',\n    '                        \\"storage\\" | \\"Storage\\" => ModuleKind::Builtin(BuiltinModule::Storage),',\n    '                        \\"storage\\" | \\"Storage\\" | \\"envStorage\\" => ModuleKind::Builtin(BuiltinModule::Storage),',\n)'''
replacement = '''modules_path = ROOT / 'engine/crates/route-engine/src/modules.rs'\nmodules_text = modules_path.read_text(encoding='utf-8')\nmodules_old = '                        \\"storage\\" | \\"Storage\\" => ModuleKind::Builtin(BuiltinModule::Storage),'\nmodules_new = '                        \\"storage\\" | \\"Storage\\" | \\"envStorage\\" => ModuleKind::Builtin(BuiltinModule::Storage),'\nif modules_text.count(modules_old) != 2:\n    raise SystemExit(f'modules.rs: expected 2 Storage registry anchors, found {modules_text.count(modules_old)}')\nmodules_path.write_text(modules_text.replace(modules_old, modules_new), encoding='utf-8')'''
if text.count(block) != 1:
    raise SystemExit(f'expected duplicate modules patch block once, found {text.count(block)}')
text = text.replace(block, replacement, 1)

path.write_text(text, encoding='utf-8')
print('storage project-root staging preflight fixed')
