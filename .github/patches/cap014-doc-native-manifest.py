from pathlib import Path

path = Path("doc/runtime-image.md")
text = path.read_text(encoding="utf-8")
old = "The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission registers the immutable artifact and an exact capability manifest before execution; the current native subset has no imports, so its manifest is intentionally empty."
new = "The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission registers the immutable artifact and an exact capability manifest before execution. Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set."
if text.count(old) != 1:
    raise SystemExit("native manifest documentation anchor changed")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
