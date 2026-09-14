//! Standalone RBE Service runtime process.
//!
//! This binary intentionally owns only the `.service` Mother/worker execution
//! entrypoints and operator restart controls. It does not contain the normal
//! backend boot path, HTTP/API server, Vault supervisor, container supervisor,
//! HostBootstrap package provisioning, or Error Reporter daemon entrypoint.

#[allow(dead_code)]
mod er_recovery;
mod service_boot;
mod service_control;
#[allow(dead_code, clippy::too_many_arguments)]
mod service_mother;

#[allow(dead_code)]
mod service_integrity {
    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));
}

// er_recovery is shared source with backend for now, but service.exe only needs
// the already-issued in-memory CONTROL key type. Keeping this tiny shim here
// prevents Linux HostBootstrap/keyring/package-manager code from entering the
// Service executable just to decode an inherited capability.
#[allow(dead_code)]
mod host_bootstrap {
    use std::sync::Arc;

    struct ErControlKeyMaterial([u8; 32]);

    impl Drop for ErControlKeyMaterial {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }

    #[derive(Clone)]
    pub struct ErControlKey(Arc<ErControlKeyMaterial>);

    impl ErControlKey {
        pub(crate) fn from_inherited_hex(value: &str) -> anyhow::Result<Self> {
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                anyhow::bail!("ER CONTROL key must be a 32-byte hexadecimal value");
            }
            let decoded = hex::decode(value)?;
            let bytes: [u8; 32] = decoded
                .try_into()
                .map_err(|_| anyhow::anyhow!("ER CONTROL key decoded to the wrong length"))?;
            Ok(Self(Arc::new(ErControlKeyMaterial(bytes))))
        }

        pub(crate) fn to_hex(&self) -> String {
            hex::encode(self.as_bytes())
        }

        pub(crate) fn as_bytes(&self) -> &[u8; 32] {
            &self.0.as_ref().0
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn emit_service_compat_probe() -> anyhow::Result<()> {
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
    stdout.write_all(b"\n")?;
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
            "{code} {headline}\n\n  reason:\n    {details}\n\n  action:\n    Review the reason above. If generated runtime files are stale, rebuild the complete RBE package.\n\n  help:\n    doc/error-codes/service.md#{}",
            code.to_ascii_lowercase(),
        )
    }
}

fn process_label(args: &[String]) -> String {
    if args.iter().any(|arg| arg == "--service-mother") {
        return "service - mother".into();
    }
    let name = flag_value(args, "--service-file")
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .or_else(|| flag_value(args, "--service-name"))
        .unwrap_or_else(|| "unknown.service".into());
    let clean = name
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    format!("service - {clean} | service.exe")
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--service-compat-probe") {
        if let Err(error) = emit_service_compat_probe() {
            eprintln!("service compatibility probe failed: {error:#}");
            std::process::exit(3);
        }
        return;
    }

    init_service_logging(&args);

    match service_control::command_from_args(&args) {
        Ok(Some(command)) => {
            match service_control::submit(&command) {
                Ok(path) => println!("Service restart request queued: {}", path.display()),
                Err(error) => {
                    logging::Logger::new("SERVICE").child("CONTROL").fatal(format!(
                        "SVC5101 Service restart request could not be queued.\n\n  reason:\n    {error:#}\n\n  action:\n    Verify the runtime data directory is writable and retry the request.\n\n  help:\n    doc/error-codes/service.md#svc5101"
                    ));
                    std::process::exit(1);
                }
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            logging::Logger::new("SERVICE").child("CONTROL").fatal(format!(
                "SVC5100 Invalid Service control command.\n\n  reason:\n    {error:#}\n\n  action:\n    Check the command syntax and retry.\n\n  help:\n    doc/error-codes/service.md#svc5100"
            ));
            std::process::exit(2);
        }
    }

    let mother = args.iter().any(|arg| arg == "--service-mother");
    let worker = args.iter().any(|arg| arg == "--service-host");
    if mother == worker {
        logging::Logger::new("SERVICE").fatal(
            "SVC5102 Service executable requires exactly one internal Mother or worker mode.\n\n  action:\n    Launch service.exe through backend.exe; these modes are internal runtime contracts.\n\n  help:\n    doc/error-codes/service.md#svc5102",
        );
        std::process::exit(2);
    }

    service_runtime::apply_service_process_label(&process_label(&args));
    let result = if mother {
        service_mother::run_child(&args).await
    } else {
        service_boot::run_host(&args).await
    };
    if let Err(error) = result {
        if mother {
            logging::Logger::new("SERVICE")
                .child("MOTHER")
                .fatal(render_fatal(
                    "SVC5099",
                    "Service Mother failed to start.",
                    &error,
                ));
        } else {
            logging::Logger::new("SERVICE")
                .child("WORKER")
                .fatal(render_fatal(
                    "SVC5199",
                    "Service worker failed to start.",
                    &error,
                ));
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn fallback_service_diagnostics_link_to_error_code_book() {
        let error = anyhow::anyhow!("synthetic startup failure");

        let mother = render_fatal("SVC5099", "Service Mother failed to start.", &error);
        assert!(mother.contains("doc/error-codes/service.md#svc5099"));

        let worker = render_fatal("SVC5199", "Service worker failed to start.", &error);
        assert!(worker.contains("doc/error-codes/service.md#svc5199"));
    }
}
