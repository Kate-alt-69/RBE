from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(
            f"{path}: expected exactly one anchor, found {count}: {old[:120]!r}"
        )
    target.write_text(text.replace(old, new, 1))


# BUG-BOOT-002: never use process::exit from the Tokio root task. Returning an
# ExitCode lets the runtime drop every spawned task/kill-on-drop child when boot
# fails before the normal graceful-shutdown path is installed.
main_path = "engine/crates/backend/src/main.rs"
replace_once(
    main_path,
    "use std::path::PathBuf;\nuse std::sync::Arc;",
    "use std::path::PathBuf;\nuse std::process::ExitCode;\nuse std::sync::Arc;",
)
replace_once(
    main_path,
    '''#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if has("--maintenance-notice") {
        let value = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
        };
        let host = value("--maintenance-host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = value("--maintenance-port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8080);
        if let Err(error) = maintenance_notice::run(host, port).await {
            eprintln!("fatal maintenance responder error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--er") {
        if !has("--launch") {
            eprintln!("backend.exe --er requires --launch as well");
            std::process::exit(2);
        }
        let separate = has("--separate-process") || has("--saperate-process");
        let bootstrap = if has("--er-bootstrap-stdin") {
            error_reporter_daemon::ErBootstrap::from_parent_stdin()
        } else {
            Ok(error_reporter_daemon::ErBootstrap::basic())
        };
        let bootstrap = match bootstrap {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                eprintln!("fatal error-reporter bootstrap error: {error:#}");
                std::process::exit(1);
            }
        };
        if let Err(error) = run_error_reporter_daemon(separate, bootstrap).await {
            eprintln!("fatal error-reporter-daemon error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--vault") {
        if let Err(error) = host_bootstrap::evaluate(&args).await {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            std::process::exit(1);
        }
        if !has("--separate-process") && !has("--saperate-process") {
            eprintln!("backend.exe --vault requires --separate-process");
            std::process::exit(2);
        }
        let value = |flag: &str, default: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| default.to_string())
        };
        let service_name = value("--service-name", "backend-rs");
        let data_dir = PathBuf::from(value(
            "--data-dir",
            &runtime_paths::default_admin_dir().to_string_lossy(),
        ));
        let force_dbus = has("--dbus");
        if let Err(error) = vault_process::run_vault_daemon(service_name, data_dir, force_dbus) {
            eprintln!("fatal Vault daemon error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    println!("Evaluating..");
    let host_ready = match host_bootstrap::evaluate(&args).await {
        Ok(ready) => ready,
        Err(error) => {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            std::process::exit(1);
        }
    };

    if let Err(error) = boot_and_run(host_ready).await {
        eprintln!("fatal boot error: {error:#}");
        std::process::exit(1);
    }
}
''',
    '''#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if has("--maintenance-notice") {
        let value = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
        };
        let host = value("--maintenance-host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = value("--maintenance-port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8080);
        if let Err(error) = maintenance_notice::run(host, port).await {
            eprintln!("fatal maintenance responder error: {error:#}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if has("--er") {
        if !has("--launch") {
            eprintln!("backend.exe --er requires --launch as well");
            return ExitCode::from(2);
        }
        let separate = has("--separate-process") || has("--saperate-process");
        let bootstrap = if has("--er-bootstrap-stdin") {
            error_reporter_daemon::ErBootstrap::from_parent_stdin()
        } else {
            Ok(error_reporter_daemon::ErBootstrap::basic())
        };
        let bootstrap = match bootstrap {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                eprintln!("fatal error-reporter bootstrap error: {error:#}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(error) = run_error_reporter_daemon(separate, bootstrap).await {
            eprintln!("fatal error-reporter-daemon error: {error:#}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if has("--vault") {
        if let Err(error) = host_bootstrap::evaluate(&args).await {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            return ExitCode::FAILURE;
        }
        if !has("--separate-process") && !has("--saperate-process") {
            eprintln!("backend.exe --vault requires --separate-process");
            return ExitCode::from(2);
        }
        let value = |flag: &str, default: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| default.to_string())
        };
        let service_name = value("--service-name", "backend-rs");
        let data_dir = PathBuf::from(value(
            "--data-dir",
            &runtime_paths::default_admin_dir().to_string_lossy(),
        ));
        let force_dbus = has("--dbus");
        if let Err(error) = vault_process::run_vault_daemon(service_name, data_dir, force_dbus) {
            eprintln!("fatal Vault daemon error: {error:#}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    println!("Evaluating..");
    let host_ready = match host_bootstrap::evaluate(&args).await {
        Ok(ready) => ready,
        Err(error) => {
            if host_bootstrap::verbose_debug(&args) {
                eprintln!("[HostBootstrap] {error:#}");
            } else {
                eprintln!("RBE initialization failed.");
            }
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = boot_and_run(host_ready).await {
        eprintln!("fatal boot error: {error:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
''',
)

# BUG-BOOT-003: the CONTROL Error Reporter used stdin only as a one-shot
# bootstrap frame. Keep that same pipe open as a real parent-liveness lease so
# backend death cannot leave a second backend.exe process behind.
replace_once(
    main_path,
    '''            if let Some(frame) = frame {
                use tokio::io::AsyncWriteExt;
                let encoded = match serde_json::to_vec(&frame) {''',
    '''            let _parent_liveness = if let Some(frame) = frame {
                use tokio::io::AsyncWriteExt;
                let mut encoded = match serde_json::to_vec(&frame) {''',
)
replace_once(
    main_path,
    '''                let Some(mut stdin) = child.stdin.take() else {''',
    '''                encoded.push(b'\\n');
                let Some(mut stdin) = child.stdin.take() else {''',
)
replace_once(
    main_path,
    '''                drop(stdin);
            }

            tracing::info!(''',
    '''                Some(stdin)
            } else {
                None
            };

            tracing::info!(''',
)

# Make the ER bootstrap newline-framed instead of EOF-framed, then watch the
# still-open remainder of stdin. Only --er-bootstrap-stdin enables this guard;
# direct/manual ER runs keep their existing behavior.
er_path = "engine/crates/backend/src/error_reporter_daemon.rs"
replace_once(
    er_path,
    "use std::io::{Read, Seek, SeekFrom};",
    "use std::io::{BufRead, Read, Seek, SeekFrom};",
)
replace_once(
    er_path,
    '''pub struct ErBootstrap {
    authority: ErAuthority,
    signing_key: String,
    control_key: Option<String>,
}''',
    '''pub struct ErBootstrap {
    authority: ErAuthority,
    signing_key: String,
    control_key: Option<String>,
    parent_liveness: bool,
}''',
)
replace_once(
    er_path,
    '''            authority: ErAuthority::Basic,
            signing_key: random_key_hex(),
            control_key: None,
        }''',
    '''            authority: ErAuthority::Basic,
            signing_key: random_key_hex(),
            control_key: None,
            parent_liveness: false,
        }''',
)
replace_once(
    er_path,
    '''    pub fn from_parent_stdin() -> anyhow::Result<Self> {
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw)?;
        if raw.len() > 4096 {
            anyhow::bail!("ER bootstrap frame exceeded 4 KiB");
        }
        let frame: ParentBootstrapFrame = serde_json::from_str(raw.trim())?;''',
    '''    pub fn from_parent_stdin() -> anyhow::Result<Self> {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut raw = String::new();
        let read = stdin.read_line(&mut raw)?;
        if read == 0 {
            anyhow::bail!("ER bootstrap pipe closed before a frame was received");
        }
        if raw.len() > 4096 {
            anyhow::bail!("ER bootstrap frame exceeded 4 KiB");
        }
        let frame: ParentBootstrapFrame = serde_json::from_str(raw.trim())?;''',
)
replace_once(
    er_path,
    '''            authority: frame.authority,
            signing_key: frame.signing_key_hex,
            control_key: frame.control_key_hex,
        })''',
    '''            authority: frame.authority,
            signing_key: frame.signing_key_hex,
            control_key: frame.control_key_hex,
            parent_liveness: true,
        })''',
)
replace_once(
    er_path,
    '''    pub fn authority(&self) -> ErAuthority {
        self.authority
    }

    pub fn recovery_key''',
    '''    pub fn authority(&self) -> ErAuthority {
        self.authority
    }

    pub fn has_parent_liveness(&self) -> bool {
        self.parent_liveness
    }

    pub fn recovery_key''',
)
replace_once(
    er_path,
    '''    std::fs::create_dir_all(&admin_dir)?;
    let authority = bootstrap.authority();
    let restart_control = authority.can_control_restarts();''',
    '''    std::fs::create_dir_all(&admin_dir)?;
    let authority = bootstrap.authority();
    if bootstrap.has_parent_liveness() {
        spawn_parent_liveness_guard()?;
    }
    let restart_control = authority.can_control_restarts();''',
)
insert_anchor = '''fn random_key_hex() -> String {
'''
insert_text = '''fn spawn_parent_liveness_guard() -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rbe-er-parent-liveness".into())
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 64];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        tracing::info!(
                            "error-reporter parent liveness pipe closed; exiting with backend"
                        );
                        std::process::exit(0);
                    }
                    Ok(_) => {}
                }
            }
        })
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("spawn ER parent liveness watcher: {error}"))
}

fn random_key_hex() -> String {
'''
replace_once(er_path, insert_anchor, insert_text)

# Backend -> Container Controller gets the same lifetime lease that Controller
# already uses for its Environment children.
container_process_path = "engine/crates/backend/src/container_process.rs"
replace_once(
    container_process_path,
    "use tokio::process::{Child, Command};",
    "use tokio::process::{Child, ChildStdin, Command};",
)
replace_once(
    container_process_path,
    '''pub struct ContainerProcess {
    child: Child,
    pub address: SocketAddr,''',
    '''pub struct ContainerProcess {
    child: Child,
    _parent_liveness: ChildStdin,
    pub address: SocketAddr,''',
)
replace_once(
    container_process_path,
    '''                .arg("--no-dashboard")
                .arg("--general-environments")''',
    '''                .arg("--no-dashboard")
                .arg("--parent-liveness-stdin")
                .arg("--general-environments")''',
)
replace_once(
    container_process_path,
    '''                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())
                .kill_on_drop(true);

            let child = command.spawn().map_err(|err| {''',
    '''                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())
                .stdin(std::process::Stdio::piped())
                .kill_on_drop(true);

            let mut child = command.spawn().map_err(|err| {''',
)
replace_once(
    container_process_path,
    '''            let pid = child.id();

            let mut process = Self {
                child,
                address,''',
    '''            let pid = child.id();
            let parent_liveness = child.stdin.take().ok_or_else(|| {
                anyhow::anyhow!("verified container parent liveness pipe was not created")
            })?;

            let mut process = Self {
                child,
                _parent_liveness: parent_liveness,
                address,''',
)

container_main_path = "container-runtime/crates/container-bin/src/main.rs"
replace_once(
    container_main_path,
    '''    if args.iter().any(|arg| arg == "--worker") {
        return run_worker(&args);
    }

    let debug = args.iter().any(|arg| arg == "--debug");''',
    '''    if args.iter().any(|arg| arg == "--worker") {
        return run_worker(&args);
    }

    if args.iter().any(|arg| arg == "--parent-liveness-stdin") {
        spawn_parent_liveness_guard()?;
    }

    let debug = args.iter().any(|arg| arg == "--debug");''',
)
replace_once(
    container_main_path,
    '''fn spawn_monitor_supervisor() -> anyhow::Result<()> {
''',
    '''fn spawn_parent_liveness_guard() -> anyhow::Result<()> {
    thread::Builder::new()
        .name("container-parent-liveness".into())
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 64];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        emit_event(
                            "parent_liveness_closed",
                            "backend parent pipe closed; stopping Controller",
                        );
                        std::process::exit(0);
                    }
                    Ok(_) => {}
                }
            }
        })
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("spawn Controller parent liveness watcher: {error}"))
}

fn spawn_monitor_supervisor() -> anyhow::Result<()> {
''',
)
