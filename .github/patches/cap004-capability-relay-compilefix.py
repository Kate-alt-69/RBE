from pathlib import Path

path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = path.read_text(encoding="utf-8")
old = '''        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session,
            runtime_image: provenance.runtime_image.clone(),'''
new = '''        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session.clone(),
            runtime_image: provenance.runtime_image.clone(),'''
if text.count(old) != 1:
    raise SystemExit(f"expected one CAP-004 Execute session move, found {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
