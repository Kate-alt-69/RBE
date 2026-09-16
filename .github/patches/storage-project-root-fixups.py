from pathlib import Path

path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = path.read_text(encoding="utf-8")
old = '''        (
            root,
            Arc::new(EnvironmentChildState {
                storage,
                project_root: Arc::new(root.clone()),
'''
new = '''        (
            root.clone(),
            Arc::new(EnvironmentChildState {
                storage,
                project_root: Arc::new(root.clone()),
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"ProjectRoot test ownership fix expected one anchor, found {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("ProjectRoot test ownership fix applied")
