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
text = text.replace(old, new, 1)

# CAP-004 intentionally has no host bridge yet, but the request type must still
# be exercised instead of hidden behind a dead-code allow. Consume metadata
# locally while returning one generic fail-closed error; do not echo payload or
# execution identity back to the sandbox.
old = '''pub fn unavailable_capability_dispatcher() -> CapabilityDispatcher {
    Arc::new(|_| {
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}'''
new = '''pub fn unavailable_capability_dispatcher() -> CapabilityDispatcher {
    Arc::new(|request| {
        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.kind,
            request.target.as_str(),
            request.operation.as_str(),
            request.payload.len(),
            request.max_response_bytes,
        );
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}'''
if text.count(old) != 1:
    raise SystemExit(f"expected one unavailable CAP-004 dispatcher, found {text.count(old)}")
text = text.replace(old, new, 1)

path.write_text(text, encoding="utf-8")
