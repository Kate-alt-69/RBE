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

# The two supervised Controller replacement call sites have different outer
# indentation. Replace the brittle exact-block matcher in the staging patch
# with a tiny regex that preserves each call site's own argument indentation.
old_block = '''# Both crash-restart and rolling-refresh spawn calls share this exact shape.\nmain_path = ROOT / 'engine/crates/backend/src/main.rs'\nmain_text = main_path.read_text(encoding='utf-8')\nold_spawn = ''' + "'''" + '''                        match container_process::ContainerProcess::spawn(\\n                            &binary,\\n                            &settings,\\n                            &host_capability,\\n                        )''' + "'''" + '''\nif main_text.count(old_spawn) != 2:\n    raise SystemExit(f'main.rs: expected 2 supervisor spawn anchors, found {main_text.count(old_spawn)}')\nmain_text = main_text.replace(\n    old_spawn,\n    ''' + "'''" + '''                        match container_process::ContainerProcess::spawn(\\n                            &binary,\\n                            &settings,\\n                            &host_capability,\\n                            &project_root,\\n                        )''' + "'''" + ''',\n)\nmain_path.write_text(main_text, encoding='utf-8')'''
new_block = '''# Crash recovery and scheduled refresh must preserve the same backend startup\n# project root. Their outer indentation differs, so patch by Rust call shape.\nimport re\nmain_path = ROOT / 'engine/crates/backend/src/main.rs'\nmain_text = main_path.read_text(encoding='utf-8')\nspawn_pattern = re.compile(\n    r'(match container_process::ContainerProcess::spawn\\(\\n'\n    r'(?P<argindent>[ \\t]+)&binary,\\n'\n    r'(?P=argindent)&settings,\\n'\n    r'(?P=argindent)&host_capability,\\n)'\n    r'(?P<closeindent>[ \\t]+)\\)'\n)\ndef add_project_root(match):\n    return (\n        match.group(1)\n        + match.group('argindent')\n        + '&project_root,\\n'\n        + match.group('closeindent')\n        + ')'\n    )\nmain_text, spawn_count = spawn_pattern.subn(add_project_root, main_text)\nif spawn_count != 2:\n    raise SystemExit(f'main.rs: expected 2 supervised Container spawn calls, found {spawn_count}')\nmain_path.write_text(main_text, encoding='utf-8')'''
if text.count(old_block) != 1:
    raise SystemExit(f'expected supervised spawn staging block once, found {text.count(old_block)}')
text = text.replace(old_block, new_block, 1)

path.write_text(text, encoding='utf-8')
print('storage project-root staging preflight fixed')
