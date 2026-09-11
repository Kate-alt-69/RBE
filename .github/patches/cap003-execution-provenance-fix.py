from pathlib import Path

path = Path("container-runtime/crates/container-runtime-core/src/swamp.rs")
text = path.read_text(encoding="utf-8")
old = '''            id: ExecutionId::from_parts(1, sequence),
            environment: "general-1".into(),
            artifact_hash: "test".into(),'''
new = '''            id: ExecutionId::from_parts(1, sequence),
            environment: "general-1".into(),
            provenance: None,
            artifact_hash: "test".into(),'''
if text.count(old) != 1:
    raise SystemExit(f"expected one Swamp test ExecutionTask initializer, found {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
