from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one diagnostics anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


# Shared, versioned compatibility response owned by service-runtime.
path = "engine/crates/service-runtime/src/lib.rs"
replace_once(
    path,
    "pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;\n",
    '''pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Parent/service compatibility protocol. This is deliberately separate from
/// REL syntax versions: it describes process ABI and catalog identity only.
pub const SERVICE_COMPAT_PROTOCOL: &str = "RBE-SERVICE-COMPAT/1";
pub const SERVICE_RUNTIME_ABI_VERSION: u32 = 1;
pub const SERVICE_CATALOG_FINGERPRINT_ABI_VERSION: u32 = 1;
pub const SERVICE_COMPAT_PROBE_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCompatibilityProbe {
    pub protocol: String,
    pub service_runtime_abi: u32,
    pub catalog_fingerprint_abi: u32,
    pub package_version: String,
    pub build_id: String,
    pub target: String,
}
''',
)

# Persist the exact build identity alongside the already-generated Service SHA.
path = "engine/crates/backend/build.rs"
replace_once(
    path,
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_SIGNING_PRIVATE_KEY");\n    println!("cargo:rerun-if-env-changed=RBE_BUILD_TRACE");\n',
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_SIGNING_PRIVATE_KEY");\n    println!("cargo:rerun-if-env-changed=RBE_BUILD_ID");\n    println!("cargo:rerun-if-env-changed=RBE_BUILD_TRACE");\n',
)
replace_once(
    path,
    '    let build_id = build_id();\n\n',
    '''    let build_id = build_id();
    if build_id.chars().any(char::is_control) {
        panic!("backend/build.rs: RBE build ID contains control characters");
    }

''',
)
replace_once(
    path,
    '''    let service_literal =
        format!("pub const EXPECTED_SERVICE_SHA256: &str = \\"{expected_service_hash}\\";\\n");
''',
    '''    let service_literal = format!(
        "pub const EXPECTED_SERVICE_SHA256: &str = {expected_service_hash:?};\\n\\
         pub const SERVICE_BUILD_ID: &str = {build_id:?};\\n\\
         pub const SERVICE_TARGET: &str = {target:?};\\n"
    );
''',
)

# service.exe owns the probe response and uses the existing Logger for fatal UX.
path = "engine/crates/backend/src/service_main.rs"
replace_once(
    path,
    '''#[allow(dead_code, clippy::too_many_arguments)]
mod service_mother;

''',
    '''#[allow(dead_code, clippy::too_many_arguments)]
mod service_mother;

mod service_integrity {
    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));
}

''',
)
replace_once(
    path,
    'fn process_label(args: &[String]) -> String {\n',
    '''fn emit_service_compat_probe() -> anyhow::Result<()> {
    use std::io::Write as _;

    let probe = service_runtime::ServiceCompatibilityProbe {
        protocol: service_runtime::SERVICE_COMPAT_PROTOCOL.to_string(),
        service_runtime_abi: service_runtime::SERVICE_RUNTIME_ABI_VERSION,
        catalog_fingerprint_abi: service_runtime::SERVICE_CATALOG_FINGERPRINT_ABI_VERSION,
        package_version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: service_integrity::SERVICE_BUILD_ID.to_string(),
        target: service_integrity::SERVICE_TARGET.to_string(),
    };
    let payload = serde_json::to_vec(&probe)?;
    if payload.len() > service_runtime::SERVICE_COMPAT_PROBE_MAX_BYTES {
        anyhow::bail!("Service compatibility probe exceeded its protocol bound");
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&payload)?;
    stdout.write_all(b"\\n")?;
    stdout.flush()?;
    Ok(())
}

fn init_service_logging(args: &[String]) {
    let settings_path = flag_value(args, "--settings").or_else(|| {
        std::env::var_os("RBE_TRUSTED_SETTINGS_PATH")
            .map(|value| value.to_string_lossy().into_owned())
    });
    let logging_config = settings_path
        .as_deref()
        .and_then(|path| config::Config::load(path).ok())
        .map(|config| config.logging)
        .unwrap_or_default();
    if let Err(error) = logging::terminal::init(&logging_config) {
        eprintln!("failed to initialize Service logging: {error:#}");
    }
}

fn known_service_diagnostic(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .lines()
        .any(|line| line.trim_start().starts_with("SVC"))
}

fn render_fatal(code: &str, headline: &str, error: &anyhow::Error) -> String {
    let details = format!("{error:#}");
    if known_service_diagnostic(error) {
        details
    } else {
        format!(
            "{code} {headline}\\n\\n  reason:\\n    {details}\\n\\n  action:\\n    Review the reason above. If generated runtime files are stale, rebuild the complete RBE package."
        )
    }
}

fn process_label(args: &[String]) -> String {
''',
)
replace_once(
    path,
    '''async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

''',
    '''async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--service-compat-probe") {
        if let Err(error) = emit_service_compat_probe() {
            eprintln!("service compatibility probe failed: {error:#}");
            std::process::exit(3);
        }
        return;
    }

    init_service_logging(&args);

''',
)
replace_once(
    path,
    '''                Err(error) => {
                    eprintln!("failed to queue Service restart request: {error:#}");
                    std::process::exit(1);
                }
''',
    '''                Err(error) => {
                    logging::Logger::new("SERVICE").child("CONTROL").fatal(format!(
                        "SVC5101 Service restart request could not be queued.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Verify the runtime data directory is writable and retry the request."
                    ));
                    std::process::exit(1);
                }
''',
)
replace_once(
    path,
    '''        Err(error) => {
            eprintln!("invalid Service restart command: {error:#}");
            std::process::exit(2);
        }
''',
    '''        Err(error) => {
            logging::Logger::new("SERVICE").child("CONTROL").fatal(format!(
                "SVC5100 Invalid Service control command.\\n\\n  reason:\\n    {error:#}\\n\\n  action:\\n    Check the command syntax and retry."
            ));
            std::process::exit(2);
        }
''',
)
replace_once(
    path,
    '''    if mother == worker {
        eprintln!("service executable requires exactly one internal Mother or worker mode");
        std::process::exit(2);
    }
''',
    '''    if mother == worker {
        logging::Logger::new("SERVICE").fatal(
            "SVC5102 Service executable requires exactly one internal Mother or worker mode.\\n\\n  action:\\n    Launch service.exe through backend.exe; these modes are internal runtime contracts.",
        );
        std::process::exit(2);
    }
''',
)
replace_once(
    path,
    '''    if let Err(error) = result {
        if mother {
            eprintln!("fatal Service Mother error: {error:#}");
        } else {
            eprintln!("fatal service worker error: {error:#}");
        }
        std::process::exit(1);
    }
''',
    '''    if let Err(error) = result {
        if mother {
            logging::Logger::new("SERVICE").child("MOTHER").fatal(render_fatal(
                "SVC5099",
                "Service Mother failed to start.",
                &error,
            ));
        } else {
            logging::Logger::new("SERVICE").child("WORKER").fatal(render_fatal(
                "SVC5199",
                "Service worker failed to start.",
                &error,
            ));
        }
        std::process::exit(1);
    }
''',
)
