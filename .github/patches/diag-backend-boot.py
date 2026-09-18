from pathlib import Path
import json


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one backend diagnostic anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


path = "engine/crates/backend/src/main.rs"
replace_once(
    path,
    'use std::sync::Arc;\n',
    'use std::sync::atomic::{AtomicBool, Ordering};\nuse std::sync::Arc;\n',
)
replace_once(
    path,
    '''mod service_integrity {
    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));
}

''',
    '''mod service_integrity {
    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));
}

static BACKEND_LOGGING_READY: AtomicBool = AtomicBool::new(false);

fn has_rbe_error_code(details: &str) -> bool {
    details.lines().any(|line| {
        let Some(token) = line.split_whitespace().next() else {
            return false;
        };
        token.len() == 7
            && token.starts_with("RBE")
            && token.as_bytes()[3..].iter().all(u8::is_ascii_digit)
    })
}

fn render_backend_boot_fatal(error: &anyhow::Error) -> String {
    let details = format!("{error:#}");
    if has_rbe_error_code(&details) {
        details
    } else {
        format!(
            "RBE5099 Backend failed to start with an unclassified boot error.\\n\\n  reason:\\n    {details}\\n\\n  action:\\n    Review the reason above and the preceding startup logs. If the failure persists with an unchanged configuration/build, preserve the logs and report it.\\n\\n  help:\\n    doc/error-codes/runtime.md#rbe5099"
        )
    }
}

''',
)
replace_once(
    path,
    '''    if let Err(error) = boot_and_run(host_ready).await {
        eprintln!("fatal boot error: {error:#}");
        return ExitCode::FAILURE;
    }
''',
    '''    if let Err(error) = boot_and_run(host_ready).await {
        let rendered = render_backend_boot_fatal(&error);
        if BACKEND_LOGGING_READY.load(Ordering::Acquire) {
            logging::Logger::new("BACKEND").child("BOOT").fatal(rendered);
        } else {
            eprintln!("FATAL [BACKEND:BOOT]:\\n{rendered}");
        }
        return ExitCode::FAILURE;
    }
''',
)
replace_once(
    path,
    '''    logging::terminal::init(&config.logging)?;
    boot_trace("logging initialized");
''',
    '''    logging::terminal::init(&config.logging)?;
    BACKEND_LOGGING_READY.store(true, Ordering::Release);
    boot_trace("logging initialized");
''',
)
replace_once(
    path,
    '''    if !container_path.is_file() {
        anyhow::bail!(
            "required container dependency is missing: {}",
            container_path.display()
        );
    }
''',
    '''    if !container_path.is_file() {
        anyhow::bail!(
            "RBE5001 Required packaged Container runtime is missing.\\n\\n  expected_path:\\n    {}\\n\\n  action:\\n    Rebuild the complete RBE package for this target and keep the generated Container binary beside the backend package layout.\\n\\n  help:\\n    doc/error-codes/runtime.md#rbe5001",
            container_path.display()
        );
    }
''',
)

# Keep the tests independent from whatever test-module layout main.rs happens
# to use. A uniquely named module avoids brittle anchor assumptions.
p = Path(path)
text = p.read_text()
if "backend_boot_fatal_preserves_specific_rbe_codes" in text:
    raise SystemExit("backend boot diagnostic tests already exist")
text += r'''

#[cfg(test)]
mod backend_boot_diagnostic_tests {
    use super::{has_rbe_error_code, render_backend_boot_fatal};

    #[test]
    fn backend_boot_fatal_preserves_specific_rbe_codes() {
        let error = anyhow::anyhow!(
            "RBE5001 Required packaged Container runtime is missing.\nhelp: doc/error-codes/runtime.md#rbe5001"
        );
        let rendered = render_backend_boot_fatal(&error);
        assert!(rendered.starts_with("RBE5001 "));
        assert!(!rendered.contains("RBE5099"));
        assert!(has_rbe_error_code(&rendered));
    }

    #[test]
    fn backend_boot_fatal_wraps_unclassified_failures() {
        let error = anyhow::anyhow!("synthetic backend startup failure");
        let rendered = render_backend_boot_fatal(&error);
        assert!(rendered.starts_with("RBE5099 "));
        assert!(rendered.contains("synthetic backend startup failure"));
        assert!(rendered.contains("doc/error-codes/runtime.md#rbe5099"));
    }
}
'''
p.write_text(text)

# Error Code Book: replace only the RBE5001 section by stable anchor
# boundaries so wording improvements elsewhere do not break this migration.
path = "doc/error-codes/runtime.md"
p = Path(path)
text = p.read_text()
start_marker = '<a id="rbe5001"></a>\n'
end_marker = '<a id="rbe9001"></a>\n'
start = text.find(start_marker)
end = text.find(end_marker)
if start < 0 or end < 0 or end <= start:
    raise SystemExit("runtime.md RBE5001 anchor boundaries drifted")
replacement = '''<a id="rbe5001"></a>
### RBE5001 — required packaged dependency missing

**Status:** Emitted.

A required packaged runtime dependency is absent from the expected application-relative path. The normal backend boot path currently emits this code when the packaged Container runtime is missing.

**Action:** rebuild/reinstall the complete RBE package for the same target. Do not satisfy this error by copying an unrelated Container binary into place.

<a id="rbe5099"></a>
### RBE5099 — backend startup failed with an unclassified boot error

**Status:** Emitted.

Backend startup returned an error that does not yet own a narrower stable `RBExxxx` diagnostic code. If terminal logging was already initialized, RBE reports this through `FATAL [BACKEND:BOOT]`; otherwise it uses the same structured text through the early stderr fallback.

**Action:** follow the nested `reason` first. If the same immutable configuration/build repeatedly fails without a more specific code, preserve the startup logs and report it so the originating branch can receive a narrower code.

'''
text = text[:start] + replacement + text[end:]
p.write_text(text)

path = "doc/error-codes/catalog.json"
p = Path(path)
data = json.loads(p.read_text())
entries = data["entries"]
by_code = {entry["code"]: entry for entry in entries}
if "RBE5001" not in by_code:
    raise SystemExit("catalog is missing RBE5001")
by_code["RBE5001"]["status"] = "emitted"
if "RBE5099" in by_code:
    raise SystemExit("catalog already contains RBE5099")
index = next(i for i, entry in enumerate(entries) if entry["code"] == "RBE9001")
entries.insert(
    index,
    {
        "code": "RBE5099",
        "status": "emitted",
        "title": "Backend startup failed with an unclassified boot error",
        "doc": "runtime.md#rbe5099",
    },
)
p.write_text(json.dumps(data, indent=2) + "\n")
