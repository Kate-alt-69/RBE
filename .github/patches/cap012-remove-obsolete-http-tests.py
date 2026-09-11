from pathlib import Path

path = Path("engine/crates/route-engine/src/video_host.rs")
text = path.read_text(encoding="utf-8")
marker = "\n#[cfg(test)]\nmod tests {"
start = text.find(marker)
if start < 0:
    raise SystemExit("video_host test module anchor changed")
tail = text[start:]
required = [
    "fn rejects_non_public_http_destinations()",
    "fn http_parser_rejects_credentials_fragments_and_controlled_headers()",
]
for needle in required:
    if needle not in tail:
        raise SystemExit(f"expected obsolete HTTP test missing: {needle}")
# These tests exercised the old route-engine-local HTTP policy. CAP-012 moves
# that policy and equivalent tests into core_lib::network_broker; leaving these
# here would reference helpers intentionally removed by the primary patch.
path.write_text(text[:start].rstrip() + "\n", encoding="utf-8")
