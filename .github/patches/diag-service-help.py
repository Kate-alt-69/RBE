from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one help-link anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


# Service Mother compatibility diagnostics own SVC5001-SVC5003.
path = "engine/crates/backend/src/service_mother.rs"
replace_once(
    path,
    '''        "SVC5001 Service binary is not compatible with the current backend.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE for this target and replace the generated Service binary at:\\n    {}",
''',
    '''        "SVC5001 Service binary is not compatible with the current backend.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE for this target and replace the generated Service binary at:\\n    {}\\n\\n  help:\\n    doc/error-codes/service.md#svc5001",
''',
)
replace_once(
    path,
    '''        "SVC5003 Service binary does not support the required compatibility protocol.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE and replace the generated Service binary at:\\n    {}",
''',
    '''        "SVC5003 Service binary does not support the required compatibility protocol.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE and replace the generated Service binary at:\\n    {}\\n\\n  help:\\n    doc/error-codes/service.md#svc5003",
''',
)
replace_once(
    path,
    '''                "SVC5002 Service Mother compiled a different Service catalog than the backend validated.\\n\\n  service_root:\\n    {}\\n\\n  service_binary:\\n    {}\\n\\n  expected_fingerprint:\\n    {}\\n\\n  compiled_fingerprint:\\n    {}\\n\\n  action:\\n    Ensure .service files and Service settings are not changing during startup and restart RBE.\\n\\n    If the mismatch persists, rebuild RBE and replace the generated Service binary at:\\n    {}",
''',
    '''                "SVC5002 Service Mother compiled a different Service catalog than the backend validated.\\n\\n  service_root:\\n    {}\\n\\n  service_binary:\\n    {}\\n\\n  expected_fingerprint:\\n    {}\\n\\n  compiled_fingerprint:\\n    {}\\n\\n  action:\\n    Ensure .service files and Service settings are not changing during startup and restart RBE.\\n\\n    If the mismatch persists, rebuild RBE and replace the generated Service binary at:\\n    {}\\n\\n  help:\\n    doc/error-codes/service.md#svc5002",
''',
)
replace_once(
    path,
    '''        assert!(error.to_string().contains("SVC5003"));
''',
    '''        let rendered = error.to_string();
        assert!(rendered.contains("SVC5003"));
        assert!(rendered.contains("doc/error-codes/service.md#svc5003"));
''',
)
replace_once(
    path,
    '''        assert!(error.to_string().contains("SVC5001"));
        assert!(error.to_string().contains("runtime ABI"));
''',
    '''        let rendered = error.to_string();
        assert!(rendered.contains("SVC5001"));
        assert!(rendered.contains("runtime ABI"));
        assert!(rendered.contains("doc/error-codes/service.md#svc5001"));
''',
)

# service.exe owns control/fallback fatal rendering.
path = "engine/crates/backend/src/service_main.rs"
replace_once(
    path,
    '''        format!(
            "{code} {headline}\\n\\n  reason:\\n    {details}\\n\\n  action:\\n    Review the reason above. If generated runtime files are stale, rebuild the complete RBE package."
        )
''',
    '''        format!(
            "{code} {headline}\\n\\n  reason:\\n    {details}\\n\\n  action:\\n    Review the reason above. If generated runtime files are stale, rebuild the complete RBE package.\\n\\n  help:\\n    doc/error-codes/service.md#{}",
            code.to_ascii_lowercase(),
        )
''',
)
replace_once(
    path,
    '''                        "SVC5101 Service restart request could not be queued.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Verify the runtime data directory is writable and retry the request."
''',
    '''                        "SVC5101 Service restart request could not be queued.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Verify the runtime data directory is writable and retry the request.\\n\\n  help:\\n    doc/error-codes/service.md#svc5101"
''',
)
replace_once(
    path,
    '''                "SVC5100 Invalid Service control command.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Check the command syntax and retry."
''',
    '''                "SVC5100 Invalid Service control command.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Check the command syntax and retry.\\n\\n  help:\\n    doc/error-codes/service.md#svc5100"
''',
)
replace_once(
    path,
    '''            "SVC5102 Service executable requires exactly one internal Mother or worker mode.\\n\\n  action:\\n    Launch service.exe through backend.exe; these modes are internal runtime contracts.",
''',
    '''            "SVC5102 Service executable requires exactly one internal Mother or worker mode.\\n\\n  action:\\n    Launch service.exe through backend.exe; these modes are internal runtime contracts.\\n\\n  help:\\n    doc/error-codes/service.md#svc5102",
''',
)
