from pathlib import Path

path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = path.read_text(encoding="utf-8")
old = "fn test_storage_state(name: &str) -> (PathBuf, Arc<EnvironmentChildState>) {"
new = "fn test_storage_state(name: &str) -> (std::path::PathBuf, Arc<EnvironmentChildState>) {"
count = text.count(old)
if count != 1:
    raise SystemExit(f"CTR-008 PathBuf test fix: expected one anchor, found {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
