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

    match service_control::command_from_args(&args) {
        Ok(Some(command)) => {
            match service_control::submit(&command) {
                Ok(path) => println!("Service restart request queued: {}", path.display()),
                Err(error) => {
                    eprintln!("failed to queue Service restart request: {error:#}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("invalid Service restart command: {error:#}");
            std::process::exit(2);
        }
    }

    let mother = args.iter().any(|arg| arg == "--service-mother");
    let worker = args.iter().any(|arg| arg == "--service-host");
    if mother == worker {
        eprintln!("service executable requires exactly one internal Mother or worker mode");
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
            eprintln!("fatal Service Mother error: {error:#}");
        } else {
            eprintln!("fatal service worker error: {error:#}");
        }
        std::process::exit(1);
    }
}
