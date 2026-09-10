from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
path = ROOT / "engine/crates/backend/src/main.rs"
text = path.read_text(encoding="utf-8")
marker = "#[cfg(test)]\nmod container_supervision_tests {\n"
start = text.find(marker)
if start < 0:
    raise SystemExit("missing container supervision test module")
end = text.find("\nfn spawn_vault_refresh(", start)
if end < 0:
    raise SystemExit("missing spawn_vault_refresh after container supervision tests")
block = text[start:end].rstrip()
text = text[:start] + text[end + 1 :]
text = text.rstrip() + "\n\n" + block + "\n"
path.write_text(text, encoding="utf-8")
