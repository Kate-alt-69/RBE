from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
replace_once(
    runtime,
    '''                let output = if cache.verified_artifact_available(&task.artifact_hash) {''',
    '''                let output = if cache.contains_artifact(&task.artifact_hash) {''',
    "Runtime dispatch durable artifact verification",
)

main = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    main,
    '''            } else if !runtime
                .cache()
                .verified_artifact_available(&request.artifact_hash)
            {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_NOT_FOUND".into(),
                    message: "execution artifact is not registered".into(),
                }''',
    '''            } else if !runtime.cache().contains_artifact(&request.artifact_hash) {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_NOT_FOUND".into(),
                    message: "execution artifact is not registered".into(),
                }''',
    "Execute durable artifact verification",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Controller protocol v8 supports a hash-only rebind for artifacts already SHA-256 verified in its cache, so repeated native calls do not retransmit WASM bytes; a cold/missing cache fails that fast path and requires full artifact registration. Backend never treats its own cache state as authority.''',
    '''Controller protocol v8 supports a hash-only rebind for artifacts already SHA-256 verified in its cache, so repeated native calls do not retransmit WASM bytes; a cold/missing cache fails that fast path and requires full artifact registration. Backend never treats its own cache state as authority. The rebind optimization does not replace execution-time durable integrity admission: Execute/runtime dispatch still re-open and SHA-256 verify the content-addressed artifact before worker launch, and the isolated worker verifies it again immediately before Wasmtime execution.''',
    "Runtime Image durable artifact verification docs",
)
