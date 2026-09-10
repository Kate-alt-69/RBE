from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing anchor: {label}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Windows does not use the Unix-sized process-name helper. Compile it only on
# platforms that actually need it (plus tests) instead of leaving dead code in
# release builds.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/lib.rs"
text = read(path)
text = replace_once(
    text,
    "\nfn short_service_process_label(label: &str, max_bytes: usize) -> String {\n",
    "\n#[cfg(any(target_os = \"linux\", target_os = \"macos\", test))]\nfn short_service_process_label(label: &str, max_bytes: usize) -> String {\n",
    "service process label target cfg",
)
write(path, text)


# ---------------------------------------------------------------------------
# The embedded shell runner is Linux-only. Keep its imports Linux-only too so
# Windows/MSVC builds stay warning-clean without suppressing useful warnings.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/host_bootstrap.rs"
text = read(path)
text = replace_once(
    text,
    "use std::process::Stdio;\nuse std::sync::Arc;\n\nuse rand::RngCore;\nuse tokio::io::AsyncWriteExt;\n",
    "#[cfg(target_os = \"linux\")]\nuse std::process::Stdio;\nuse std::sync::Arc;\n\nuse rand::RngCore;\n#[cfg(target_os = \"linux\")]\nuse tokio::io::AsyncWriteExt;\n",
    "HostBootstrap Linux-only imports",
)
write(path, text)


# ---------------------------------------------------------------------------
# Container integrity binding is expected build metadata, not a warning. Keep
# the detailed hash/build-id trace available behind RBE_BUILD_TRACE while making
# normal release builds quiet.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/build.rs"
text = read(path)
text = replace_once(
    text,
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_SIGNING_PRIVATE_KEY");\n',
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_SIGNING_PRIVATE_KEY");\n    println!("cargo:rerun-if-env-changed=RBE_BUILD_TRACE");\n',
    "build trace rerun marker",
)
text = replace_once(
    text,
    '            println!("cargo:warning=backend: binding container SHA-256 {hash}, build_id {build_id}, target {target}");\n',
    '            if std::env::var_os("RBE_BUILD_TRACE").is_some() {\n                println!("cargo:warning=backend: binding container SHA-256 {hash}, build_id {build_id}, target {target}");\n            }\n',
    "container binding informational warning",
)
write(path, text)


# ---------------------------------------------------------------------------
# ER self-supervision reports are a structured observation, not nine unrelated
# function arguments. This removes the Clippy failure while improving the
# boundary between observation data and recovery policy.
# This runs after er-self-supervision-wire.py has materialized the staged code.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
text = replace_once(
    text,
    '''fn report_er_supervisor_failure(\n    phase: &str,\n    pid: Option<u32>,\n    exit_success: Option<bool>,\n    exit_code: Option<i32>,\n    exit_signal: Option<i32>,\n    uptime: Duration,\n    consecutive_failures: u32,\n    authority: &str,\n    observation_error: Option<&str>,\n) {\n    let why = match observation_error {''',
    '''struct ErSupervisorFailure {\n    phase: &'static str,\n    pid: Option<u32>,\n    exit_success: Option<bool>,\n    exit_code: Option<i32>,\n    exit_signal: Option<i32>,\n    uptime: Duration,\n    observation_error: Option<String>,\n}\n\nfn report_er_supervisor_failure(\n    observation: ErSupervisorFailure,\n    consecutive_failures: u32,\n    authority: &str,\n) {\n    let ErSupervisorFailure {\n        phase,\n        pid,\n        exit_success,\n        exit_code,\n        exit_signal,\n        uptime,\n        observation_error,\n    } = observation;\n    let why = match observation_error.as_deref() {''',
    "ER self-supervision structured observation",
)

replacements = [
    (
        '''                    report_er_supervisor_failure(\n                        "spawn-failed",\n                        None,\n                        None,\n                        None,\n                        None,\n                        Duration::ZERO,\n                        consecutive_failures,\n                        authority,\n                        Some(&error.to_string()),\n                    );''',
        '''                    report_er_supervisor_failure(\n                        ErSupervisorFailure {\n                            phase: "spawn-failed",\n                            pid: None,\n                            exit_success: None,\n                            exit_code: None,\n                            exit_signal: None,\n                            uptime: Duration::ZERO,\n                            observation_error: Some(error.to_string()),\n                        },\n                        consecutive_failures,\n                        authority,\n                    );''',
        "ER spawn failure observation",
    ),
    (
        '''                        report_er_supervisor_failure(\n                            "bootstrap-serialize-failed",\n                            pid,\n                            None,\n                            None,\n                            None,\n                            started_at.elapsed(),\n                            consecutive_failures,\n                            authority,\n                            Some(&error.to_string()),\n                        );''',
        '''                        report_er_supervisor_failure(\n                            ErSupervisorFailure {\n                                phase: "bootstrap-serialize-failed",\n                                pid,\n                                exit_success: None,\n                                exit_code: None,\n                                exit_signal: None,\n                                uptime: started_at.elapsed(),\n                                observation_error: Some(error.to_string()),\n                            },\n                            consecutive_failures,\n                            authority,\n                        );''',
        "ER bootstrap serialization observation",
    ),
    (
        '''                    report_er_supervisor_failure(\n                        "bootstrap-pipe-unavailable",\n                        pid,\n                        None,\n                        None,\n                        None,\n                        started_at.elapsed(),\n                        consecutive_failures,\n                        authority,\n                        Some("CONTROL ER child did not expose inherited bootstrap stdin"),\n                    );''',
        '''                    report_er_supervisor_failure(\n                        ErSupervisorFailure {\n                            phase: "bootstrap-pipe-unavailable",\n                            pid,\n                            exit_success: None,\n                            exit_code: None,\n                            exit_signal: None,\n                            uptime: started_at.elapsed(),\n                            observation_error: Some(\n                                "CONTROL ER child did not expose inherited bootstrap stdin".into(),\n                            ),\n                        },\n                        consecutive_failures,\n                        authority,\n                    );''',
        "ER bootstrap pipe observation",
    ),
    (
        '''                    report_er_supervisor_failure(\n                        "bootstrap-write-failed",\n                        pid,\n                        None,\n                        None,\n                        None,\n                        started_at.elapsed(),\n                        consecutive_failures,\n                        authority,\n                        Some(&error.to_string()),\n                    );''',
        '''                    report_er_supervisor_failure(\n                        ErSupervisorFailure {\n                            phase: "bootstrap-write-failed",\n                            pid,\n                            exit_success: None,\n                            exit_code: None,\n                            exit_signal: None,\n                            uptime: started_at.elapsed(),\n                            observation_error: Some(error.to_string()),\n                        },\n                        consecutive_failures,\n                        authority,\n                    );''',
        "ER bootstrap write observation",
    ),
    (
        '''                            report_er_supervisor_failure(\n                                "unexpected-exit",\n                                pid,\n                                Some(status.success()),\n                                status.code(),\n                                er_exit_signal(&status),\n                                uptime,\n                                consecutive_failures,\n                                authority,\n                                None,\n                            );''',
        '''                            report_er_supervisor_failure(\n                                ErSupervisorFailure {\n                                    phase: "unexpected-exit",\n                                    pid,\n                                    exit_success: Some(status.success()),\n                                    exit_code: status.code(),\n                                    exit_signal: er_exit_signal(&status),\n                                    uptime,\n                                    observation_error: None,\n                                },\n                                consecutive_failures,\n                                authority,\n                            );''',
        "ER unexpected exit observation",
    ),
    (
        '''                            report_er_supervisor_failure(\n                                "wait-failed",\n                                pid,\n                                None,\n                                None,\n                                None,\n                                uptime,\n                                consecutive_failures,\n                                authority,\n                                Some(&error.to_string()),\n                            );''',
        '''                            report_er_supervisor_failure(\n                                ErSupervisorFailure {\n                                    phase: "wait-failed",\n                                    pid,\n                                    exit_success: None,\n                                    exit_code: None,\n                                    exit_signal: None,\n                                    uptime,\n                                    observation_error: Some(error.to_string()),\n                                },\n                                consecutive_failures,\n                                authority,\n                            );''',
        "ER wait failure observation",
    ),
]

for old, new, label in replacements:
    text = replace_once(text, old, new, label)

write(path, text)
