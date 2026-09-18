from pathlib import Path

PUBLIC_BASE = "https://kastrick.vercel.app/project/rbe/doc/error-codes"


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    file.write_text(text.replace(old, new, 1))


def replace_all(path: str, old: str, new: str, minimum: int, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count < minimum:
        raise SystemExit(f"{label}: expected at least {minimum} matches, found {count}")
    file.write_text(text.replace(old, new))


# REL/RELC owns one dynamic help target builder. Keep code assignment typed at
# the originating error site, but send the human-facing help link to the public
# documentation website.
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    '''    /// Repository-relative long-form Error Code Book target.\n    pub fn help_path(&self) -> String {\n        let code = self.code();\n        let book = if code.starts_with("RELC") {\n            "relc.md"\n        } else {\n            "rel.md"\n        };\n        format!("doc/error-codes/{book}#{}", code.to_ascii_lowercase())\n    }\n''',
    '''    /// Public long-form Error Code Book target. The compiler stays usable\n    /// offline because backend/service also embed the same book for `--explain`.\n    pub fn help_url(&self) -> String {\n        let code = self.code();\n        let book = if code.starts_with("RELC") { "relc" } else { "rel" };\n        format!(\n            "https://kastrick.vercel.app/project/rbe/doc/error-codes/{book}#{}",\n            code.to_ascii_lowercase()\n        )\n    }\n''',
    "RELC public help builder",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    '        write!(formatter, "\\nhelp: {}", self.help_path())\n',
    '        write!(formatter, "\\nhelp: {}", self.help_url())\n',
    "RELC Display public help",
)
replace_all(
    "engine/crates/route-engine/src/relc.rs",
    "doc/error-codes/rel.md#",
    PUBLIC_BASE + "/rel#",
    1,
    "REL diagnostic tests",
)
replace_all(
    "engine/crates/route-engine/src/relc.rs",
    "doc/error-codes/relc.md#",
    PUBLIC_BASE + "/relc#",
    1,
    "RELC diagnostic tests",
)

# Existing backend/service diagnostics already carry stable codes. Redirect the
# short `help:` pointers to the public website without changing their meaning.
for path, page in [
    ("engine/crates/backend/src/service_main.rs", "service"),
    ("engine/crates/backend/src/service_mother.rs", "service"),
    ("engine/crates/backend/src/container_process.rs", "runtime"),
    ("engine/crates/backend/src/main.rs", "runtime"),
]:
    replace_all(
        path,
        f"doc/error-codes/{page}.md#",
        f"{PUBLIC_BASE}/{page}#",
        1,
        f"{path} public help links",
    )

# Offline lookup remains embedded and network-independent, but tells users where
# the continuously updated website copy lives as well.
error_book = "engine/crates/backend/src/error_code_book.rs"
replace_once(
    error_book,
    'const RUNTIME: &str = include_str!("../../../../doc/error-codes/runtime.md");\n',
    'const RUNTIME: &str = include_str!("../../../../doc/error-codes/runtime.md");\nconst PUBLIC_BASE_URL: &str = "https://kastrick.vercel.app/project/rbe/doc/error-codes";\n',
    "embedded Error Code Book public base",
)
replace_once(
    error_book,
    '''    Ok(format!(\n        "{requested} — {title}\\nStatus: {status}\\n\\n{body}\\n\\nReference: doc/error-codes/{doc}"\n    ))\n''',
    '''    let online_page = page_name.strip_suffix(".md").unwrap_or(page_name);\n    let online = format!("{PUBLIC_BASE_URL}/{online_page}#{anchor}");\n    Ok(format!(\n        "{requested} — {title}\\nStatus: {status}\\n\\n{body}\\n\\nReference: doc/error-codes/{doc}\\nOnline: {online}"\n    ))\n''',
    "offline explain online reference",
)
replace_once(
    error_book,
    '        assert!(rendered.contains("doc/error-codes/relc.md#relc3001"));\n',
    '        assert!(rendered.contains("doc/error-codes/relc.md#relc3001"));\n        assert!(rendered.contains("https://kastrick.vercel.app/project/rbe/doc/error-codes/relc#relc3001"));\n',
    "offline RELC online URL regression",
)
replace_once(
    error_book,
    '        assert!(rendered.contains("doc/error-codes/service.md#svc5002"));\n',
    '        assert!(rendered.contains("doc/error-codes/service.md#svc5002"));\n        assert!(rendered.contains("https://kastrick.vercel.app/project/rbe/doc/error-codes/service#svc5002"));\n',
    "offline Service online URL regression",
)

# The catalog remains page-relative so the website can render/move pages, but it
# advertises the canonical public root for tooling and other RBE components.
catalog = Path("doc/error-codes/catalog.json")
text = catalog.read_text()
needle = '  "description": "Machine-readable index for the authoritative RBE Error Code Book. Markdown pages contain the full explanations.",\n'
if text.count(needle) != 1:
    raise SystemExit("catalog publicBaseUrl anchor drifted")
text = text.replace(
    needle,
    needle + f'  "publicBaseUrl": "{PUBLIC_BASE}",\\n'.replace('\\n', '\n'),
    1,
)
catalog.write_text(text)

# Human docs should demonstrate the URL users actually see in terminals.
replace_all(
    "doc/error-codes/README.md",
    "doc/error-codes/relc.md#relc3001",
    PUBLIC_BASE + "/relc#relc3001",
    1,
    "Error Code Book example URL",
)
replace_all(
    "doc/error-codes/README.md",
    "doc/error-codes/<book>.md#<lowercase-code>",
    PUBLIC_BASE + "/<book>#<lowercase-code>",
    1,
    "Error Code Book UX contract URL",
)
replace_once(
    "doc/error-codes/README.md",
    "The built backend package also supports offline long-form lookup before runtime bootstrap:\n",
    "Runtime diagnostics use the public documentation website as their canonical `help:` target. The built backend package also supports offline long-form lookup before runtime bootstrap, using the same book embedded at build time:\n",
    "offline/public Error Code Book explanation",
)
replace_all(
    "doc/error-codes/service.md",
    "doc/error-codes/service.md#",
    PUBLIC_BASE + "/service#",
    1,
    "Service Error Code Book example URL",
)
