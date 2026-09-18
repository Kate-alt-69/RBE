from pathlib import Path

path = Path("engine/crates/cloud-node/src/provider.rs")
text = path.read_text()
start_marker = "    pub(crate) async fn put_file(\n"
end_marker = "    pub(crate) async fn put_file_create_only(\n"
start = text.find(start_marker)
end = text.find(end_marker)
if start == -1:
    raise SystemExit("legacy put_file method not found")
if end == -1 or end <= start:
    raise SystemExit("put_file_create_only method not found after legacy put_file")
text = text[:start] + text[end:]
path.write_text(text)
