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
    for (index, arg) in args.iter().enumerate() {
        if arg == "--explain" {
            let Some(code) = args.get(index + 1) else {
                return Some(Err(anyhow::anyhow!(
                    "--explain requires an error code, for example RELC3001"
                )));
            };
            return Some(explain(code));
        }
        if let Some(code) = arg.strip_prefix("--explain=") {
            return Some(explain(code));
        }
        if arg == "--list-error-codes" {
            let prefix = args
                .get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .map(String::as_str);
            return Some(list_codes(prefix));
        }
    }
    None
}

fn catalog() -> anyhow::Result<serde_json::Value> {
    serde_json::from_str(CATALOG)
        .map_err(|error| anyhow::anyhow!("embedded Error Code Book catalog is invalid: {error}"))
}

fn entries(catalog: &serde_json::Value) -> anyhow::Result<&Vec<serde_json::Value>> {
    catalog
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("embedded Error Code Book catalog has no entries"))
}

fn code_prefix(code: &str) -> String {
    code.chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn code_number(code: &str) -> Option<u32> {
    let digits = code
        .chars()
        .skip_while(|character| character.is_ascii_alphabetic())
        .collect::<String>();
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn code_matches_filter(code: &str, filter: &str) -> bool {
    let normalized = filter.trim().to_ascii_uppercase();
    if normalized.chars().all(|character| character.is_ascii_alphabetic()) {
        code_prefix(code) == normalized
    } else {
        code.starts_with(&normalized)
    }
}

fn render_entry(entry: &serde_json::Value) -> Option<String> {
    let code = entry.get("code")?.as_str()?;
    let status = entry
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("RBE diagnostic");
    Some(format!("{code:<10} [{status:<8}] {title}"))
}

fn render_known_codes(
    entries: &[serde_json::Value],
    prefix: Option<&str>,
    limit: Option<usize>,
) -> Vec<String> {
    let mut out = entries
        .iter()
        .filter(|entry| {
            let Some(code) = entry.get("code").and_then(serde_json::Value::as_str) else {
                return false;
            };
            prefix.is_none_or(|filter| code_matches_filter(code, filter))
        })
        .filter_map(render_entry)
        .collect::<Vec<_>>();
    out.sort();
    if let Some(limit) = limit {
        out.truncate(limit);
    }
    out
}

fn render_nearby_codes(
    entries: &[serde_json::Value],
    requested: &str,
    limit: usize,
) -> Vec<String> {
    let prefix = code_prefix(requested);
    let requested_number = code_number(requested);
    let mut ranked = entries
        .iter()
        .filter_map(|entry| {
            let code = entry.get("code")?.as_str()?;
            if code_prefix(code) != prefix {
                return None;
            }
            let distance = match (requested_number, code_number(code)) {
                (Some(requested), Some(candidate)) => requested.abs_diff(candidate),
                _ => u32::MAX,
            };
            Some((distance, code, render_entry(entry)?))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(right.1))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, rendered)| rendered)
        .collect()
}

pub fn list_codes(prefix: Option<&str>) -> anyhow::Result<String> {
    let catalog = catalog()?;
    let entries = entries(&catalog)?;
    let lines = render_known_codes(entries, prefix, None);
    if lines.is_empty() {
        let requested = prefix.unwrap_or("<all>").trim().to_ascii_uppercase();
        anyhow::bail!("no RBE error codes are registered for prefix {requested}");
    }
    let heading = prefix
        .map(|value| format!("RBE Error Code Book — {}", value.trim().to_ascii_uppercase()))
        .unwrap_or_else(|| "RBE Error Code Book".to_string());
    Ok(format!("{heading}\n\n{}", lines.join("\n")))
}

pub fn explain(code: &str) -> anyhow::Result<String> {
    let requested = code.trim().to_ascii_uppercase();
    if requested.is_empty() {
        anyhow::bail!("error code cannot be empty");
    }

    let catalog = catalog()?;
    let entries = entries(&catalog)?;
    let Some(entry) = entries.iter().find(|entry| {
        entry
            .get("code")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(&requested))
    }) else {
        let suggestions = render_nearby_codes(entries, &requested, 8);
        if suggestions.is_empty() {
            anyhow::bail!(
                "unknown RBE error code {requested}; use --list-error-codes to inspect registered codes"
            );
        }
        anyhow::bail!(
            "unknown RBE error code {requested}. Nearby registered codes:\n{}",
            suggestions.join("\n")
        );
    };

    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("RBE diagnostic");
    let status = entry
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
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
    let body = section
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            if index == 0 && line.starts_with("### ") {
                None
            } else if line.starts_with("**Status:**") {
                None
            } else {
                Some(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    Ok(format!(
        "{requested} — {title}\nStatus: {status}\n\n{body}\n\nReference: doc/error-codes/{doc}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_specific_relc_error_from_embedded_book() {
        let rendered = explain("relc3001").expect("RELC3001 must be explainable");
        assert!(rendered.contains("RELC3001"));
        assert!(rendered.contains("Native Container execution required but lowering failed"));
        assert!(rendered.contains("doc/error-codes/relc.md#relc3001"));
        assert_eq!(rendered.matches("RELC3001").count(), 1);
    }

    #[test]
    fn explains_service_error_from_embedded_book() {
        let rendered = explain("SVC5002").expect("SVC5002 must be explainable");
        assert!(rendered.contains("Service catalog changed"));
        assert!(rendered.contains("doc/error-codes/service.md#svc5002"));
    }

    #[test]
    fn unknown_code_suggests_nearest_same_subsystem_codes() {
        let error = explain("RELC3999").expect_err("unknown codes must be rejected");
        let message = error.to_string();
        assert!(message.contains("unknown RBE error code RELC3999"));
        assert!(message.contains("RELC4001"));
        assert!(message.contains("RELC3001"));
        assert!(!message.contains("REL1000"));
    }

    #[test]
    fn lists_codes_by_prefix() {
        let rendered = list_codes(Some("REL")).expect("REL code list must exist");
        assert!(rendered.contains("REL1000"));
        assert!(!rendered.contains("RELC1000"));
    }

    #[test]
    fn numeric_filter_can_narrow_a_family() {
        let rendered = list_codes(Some("SVC5")).expect("SVC5 range must exist");
        assert!(rendered.contains("SVC5001"));
        assert!(!rendered.contains("SVC1000"));
    }

    #[test]
    fn requested_accepts_equals_form() {
        let args = vec!["--explain=svc5002".to_string()];
        let rendered = requested(&args)
            .expect("lookup should be detected")
            .expect("lookup should succeed");
        assert!(rendered.contains("SVC5002"));
    }
}
'''
Path('engine/crates/backend/src/error_code_book.rs').write_text(module)

# backend.exe handles Error Code Book requests before HostBootstrap/settings/logging.
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
backend.exe --explain=RELC3001
backend.exe --list-error-codes RELC
service.exe --explain SVC5002
```

Unknown codes suggest numerically nearby registered codes from the same subsystem. Alphabetic filters select one exact subsystem (`REL` does not include `RELC`); filters containing digits can narrow a range such as `SVC5`. The lookup is compiled from this authoritative documentation tree, so it does not require network access or a mutable runtime docs directory.

For machine-readable tooling, see [`catalog.json`](catalog.json).
''',
)
