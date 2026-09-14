from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one explain anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


module = r'''//! Embedded user-facing RBE Error Code Book lookup.
//!
//! The CLI must remain usable before settings/logging/runtime bootstrap, so the
//! authoritative docs are compiled into backend/service rather than read from
//! a mutable runtime path or fetched from the network.

const CATALOG: &str = include_str!("../../../../doc/error-codes/catalog.json");
const REL: &str = include_str!("../../../../doc/error-codes/rel.md");
const RELC: &str = include_str!("../../../../doc/error-codes/relc.md");
const SERVICE: &str = include_str!("../../../../doc/error-codes/service.md");
const CONTAINER: &str = include_str!("../../../../doc/error-codes/container.md");
const RUNTIME: &str = include_str!("../../../../doc/error-codes/runtime.md");

pub fn requested(args: &[String]) -> Option<anyhow::Result<String>> {
    let index = args.iter().position(|arg| arg == "--explain")?;
    let Some(code) = args.get(index + 1) else {
        return Some(Err(anyhow::anyhow!(
            "--explain requires an error code, for example RELC3001"
        )));
    };
    Some(explain(code))
}

pub fn explain(code: &str) -> anyhow::Result<String> {
    let requested = code.trim().to_ascii_uppercase();
    if requested.is_empty() {
        anyhow::bail!("error code cannot be empty");
    }

    let catalog: serde_json::Value = serde_json::from_str(CATALOG)
        .map_err(|error| anyhow::anyhow!("embedded Error Code Book catalog is invalid: {error}"))?;
    let entries = catalog
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("embedded Error Code Book catalog has no entries"))?;
    let entry = entries
        .iter()
        .find(|entry| {
            entry
                .get("code")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(&requested))
        })
        .ok_or_else(|| anyhow::anyhow!("unknown RBE error code {requested}"))?;

    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("RBE diagnostic");
    let doc = entry
        .get("doc")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Error Code Book entry {requested} has no doc target"))?;
    let (page_name, anchor) = doc
        .split_once('#')
        .ok_or_else(|| anyhow::anyhow!("Error Code Book entry {requested} has an invalid doc target"))?;
    let page = match page_name {
        "rel.md" => REL,
        "relc.md" => RELC,
        "service.md" => SERVICE,
        "container.md" => CONTAINER,
        "runtime.md" => RUNTIME,
        other => anyhow::bail!("Error Code Book entry {requested} references unsupported page {other}"),
    };

    let marker = format!("<a id=\"{anchor}\"></a>");
    let start = page.find(&marker).ok_or_else(|| {
        anyhow::anyhow!("Error Code Book entry {requested} points to missing anchor {anchor}")
    })? + marker.len();
    let tail = &page[start..];
    let end = tail.find("\n<a id=\"").unwrap_or(tail.len());
    let section = tail[..end].trim();

    Ok(format!(
        "{requested} — {title}\n\n{section}\n\nReference: doc/error-codes/{doc}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_specific_relc_error_from_embedded_book() {
        let rendered = explain("relc3001").expect("RELC3001 must be explainable");
        assert!(rendered.contains("RELC3001"));
        assert!(rendered.contains("native Container execution required"));
        assert!(rendered.contains("doc/error-codes/relc.md#relc3001"));
    }

    #[test]
    fn explains_service_error_from_embedded_book() {
        let rendered = explain("SVC5002").expect("SVC5002 must be explainable");
        assert!(rendered.contains("Service catalog changed"));
        assert!(rendered.contains("doc/error-codes/service.md#svc5002"));
    }

    #[test]
    fn rejects_unknown_error_code() {
        let error = explain("NOPE9999").expect_err("unknown codes must be rejected");
        assert!(error.to_string().contains("unknown RBE error code"));
    }
}
'''
Path('engine/crates/backend/src/error_code_book.rs').write_text(module)

# backend.exe handles --explain before HostBootstrap/settings/logging.
path = 'engine/crates/backend/src/main.rs'
replace_once(
    path,
    'mod error_reporter_daemon;\n',
    'mod error_code_book;\nmod error_reporter_daemon;\n',
)
replace_once(
    path,
    '''    let has = |flag: &str| args.iter().any(|arg| arg == flag);

''',
    '''    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if let Some(explanation) = error_code_book::requested(&args) {
        match explanation {
            Ok(explanation) => {
                println!("{explanation}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("Error Code Book lookup failed: {error}");
                return ExitCode::from(2);
            }
        }
    }

''',
)

# service.exe gets the same pre-bootstrap/offline lookup.
path = 'engine/crates/backend/src/service_main.rs'
replace_once(
    path,
    'mod er_recovery;\n',
    'mod er_recovery;\nmod error_code_book;\n',
)
replace_once(
    path,
    '''    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--service-compat-probe") {
''',
    '''    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(explanation) = error_code_book::requested(&args) {
        match explanation {
            Ok(explanation) => println!("{explanation}"),
            Err(error) => {
                eprintln!("Error Code Book lookup failed: {error}");
                std::process::exit(2);
            }
        }
        return;
    }

    if args.iter().any(|arg| arg == "--service-compat-probe") {
''',
)

# Document the offline CLI as part of the public diagnostics contract.
path = 'doc/error-codes/README.md'
replace_once(
    path,
    '''For tooling and future `--explain <CODE>` support, see [`catalog.json`](catalog.json).
''',
    '''The built backend package also supports offline long-form lookup before runtime bootstrap:

```text
backend.exe --explain RELC3001
service.exe --explain SVC5002
```

The lookup is compiled from this authoritative documentation tree, so it does not require network access or a mutable runtime docs directory.

For machine-readable tooling, see [`catalog.json`](catalog.json).
''',
)
