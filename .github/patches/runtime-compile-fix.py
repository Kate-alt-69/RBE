from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


# Runtime Image route registration borrows url_path for Router::route while the
# handler also needs an owned path for request.originalUrl/path matching.
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
old = "build_method_router(route_file.as_ref(), module_program.clone(), url_path),"
new = "build_method_router(route_file.as_ref(), module_program.clone(), url_path.clone()),"
if old not in text and new not in text:
    raise SystemExit("missing Runtime Image route path ownership anchor")
text = text.replace(old, new, 1)
write(path, text)


# Canonical service.exe removed all temporary alias filesystem paths from the
# manager, so Path/PathBuf are no longer used here.
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace("use std::path::{Path, PathBuf};\n", "", 1)
write(path, text)
