from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


# Top-level backend: explicit --settings wins. Ambient SETTINGS_PATH is ignored
# unless the operator deliberately opts into the legacy development behavior.
path = "engine/crates/backend/src/main.rs"
text = read(path)
if "fn resolve_settings_path()" not in text:
    anchor = '''async fn boot_and_run() -> anyhow::Result<()> {
'''
    if anchor not in text:
        raise SystemExit("missing boot_and_run anchor")
    helper = r'''fn resolve_settings_path() -> String {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(value) = args
        .windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| pair[1].clone())
    {
        return value;
    }

    if args.iter().any(|arg| arg == "--allow-settings-env") {
        if let Ok(value) = std::env::var("SETTINGS_PATH") {
            eprintln!(
                "warning: --allow-settings-env enabled deprecated ambient SETTINGS_PATH support"
            );
            return value;
        }
    } else if std::env::var_os("SETTINGS_PATH").is_some() {
        eprintln!(
            "warning: ignoring ambient SETTINGS_PATH; use --settings <file> (or --allow-settings-env for legacy development compatibility)"
        );
    }

    "settings.json".to_string()
}

'''
    text = text.replace(anchor, helper + anchor, 1)

old = '''    let settings_path =
        std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".to_string());'''
if old in text:
    text = text.replace(old, '''    let settings_path = resolve_settings_path();''', 1)
elif "let settings_path = resolve_settings_path();" not in text:
    raise SystemExit("missing backend settings path anchor after Runtime Image patch")
write(path, text)


# Service Mother receives a trusted canonical settings path as an explicit arg.
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
old = '''    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".into());'''
if old in text:
    text = text.replace(
        old,
        '''    let settings_path = flag_value(args, "--settings").unwrap_or_else(|| "settings.json".into());''',
        1,
    )

# The child environment is cleared by the source-security patch. Keep only an
# RBE-private path for Manager->service-child propagation, never SETTINGS_PATH.
text = text.replace('        "SETTINGS_PATH",\n', '', 1)
text = text.replace(
    '''    command
        .env("SETTINGS_PATH", settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1");''',
    '''    command
        .env("RBE_TRUSTED_SETTINGS_PATH", settings_path)
        .env("RBE_PARENT_LIVENESS_PIPE", "1");''',
    1,
)
command_anchor = '''        .args(["--service-mother", "--launch-separate"])
        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
'''
if command_anchor not in text:
    raise SystemExit("missing hardened Service Mother command anchor")
text = text.replace(
    command_anchor,
    command_anchor + '''        .arg("--settings")
        .arg(&settings_path)
''',
    1,
)
write(path, text)


# Service host trusts only its explicit parent-supplied settings argument.
path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
old = '''    let settings_path = std::env::var("SETTINGS_PATH").unwrap_or_else(|_| "settings.json".into());'''
if old in text:
    text = text.replace(
        old,
        '''    let settings_path = value("--settings").unwrap_or_else(|| "settings.json".into());''',
        1,
    )
write(path, text)


# Service Manager runs inside the trusted Mother. It forwards the canonical
# path as argv to each child and never copies ambient SETTINGS_PATH.
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace('        "SETTINGS_PATH",\n', '', 1)
command_anchor = '''    command
        .args(["--service-host", "--service-file"])
        .arg(&file.path)
'''
if command_anchor not in text:
    raise SystemExit("missing hardened service child command anchor")
if '--settings")' not in text[text.index(command_anchor):text.index(command_anchor) + 1200]:
    replacement = command_anchor + '''        .arg("--settings")
        .arg(
            std::env::var_os("RBE_TRUSTED_SETTINGS_PATH")
                .unwrap_or_else(|| std::ffi::OsString::from("settings.json")),
        )
'''
    text = text.replace(command_anchor, replacement, 1)
write(path, text)
