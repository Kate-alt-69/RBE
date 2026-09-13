from pathlib import Path

path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text(encoding="utf-8")
old = 'r#":import[module&learning/catalog]\\n                   class Route { get(req) { return true; } }"#'
new = 'r#":import[\\"./module/learning/catalog\\"]\\n                   class Route { get(req) { return true; } }"#'
if text.count(old) != 1:
    raise SystemExit(f"CAP-020 nested import fixture: expected one anchor, found {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
