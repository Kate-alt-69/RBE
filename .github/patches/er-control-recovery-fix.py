from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


path = "engine/crates/backend/src/host_bootstrap.rs"
text = read(path)
old = '''    pub fn issue_er_control_key(self) -> Option<ErControlKey> {\n        self.secure_credentials.then(ErControlKey::random)\n    }'''
new = '''    pub fn issue_er_control_key(self) -> Option<ErControlKey> {\n        self.er_control_enabled().then(ErControlKey::random)\n    }'''
if old not in text:
    raise SystemExit("missing HostBootstrap ER key issuance anchor")
text = text.replace(old, new, 1)
write(path, text)

path = "engine/crates/backend/src/er_recovery.rs"
text = read(path)
old = '''        let Some(raw) = read_limited(&path, REQUEST_MAX_BYTES)? else {\n            continue;\n        };'''
new = '''        let raw = match read_limited(&path, REQUEST_MAX_BYTES) {\n            Ok(Some(raw)) => raw,\n            Ok(None) => continue,\n            Err(_) => {\n                let _ = std::fs::remove_file(&path);\n                continue;\n            }\n        };'''
if old not in text:
    raise SystemExit("missing ER bounded request read anchor")
text = text.replace(old, new, 1)
old = '''fn explain_activity(report: &ServiceExitReport) -> String {\n    let operation = report.last_operation.as_deref().unwrap_or("idle-or-unknown");\n    format!(\n        "phase={} operation={} active_calls={} uptime_ms={} idle_for_ms={}",\n        report.phase, report.active_calls, report.active_calls, report.uptime_ms, report.idle_for_ms\n    )\n    .replace(\n        &format!("operation={}", report.active_calls),\n        &format!("operation={operation}"),\n    )\n}'''
new = '''fn explain_activity(report: &ServiceExitReport) -> String {\n    let operation = report.last_operation.as_deref().unwrap_or("idle-or-unknown");\n    format!(\n        "phase={} operation={} active_calls={} uptime_ms={} idle_for_ms={}",\n        report.phase, operation, report.active_calls, report.uptime_ms, report.idle_for_ms\n    )\n}'''
if old not in text:
    raise SystemExit("missing ER activity explanation anchor")
text = text.replace(old, new, 1)
write(path, text)
