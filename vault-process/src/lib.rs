// existing vault-process source through this function is unchanged above
pub fn run_vault_daemon(
    service_name: String,
    data_dir: PathBuf,
    _force_dbus: bool,
) -> anyhow::Result<()> {
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &data_dir);
    error_client::install_panic_hook();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .try_init();

    #[cfg(target_os = "linux")]
    if !_force_dbus
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
            .map(|v| v.is_empty())
            .unwrap_or(true)
    {
        let message = serde_json::to_string(&NeedsDbus {
            kind: "needs_dbus".into(),
        })?;
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        writeln!(writer, "{message}")?;
        writer.flush()?;
        return Ok(());
    }

    let vault = vault::Vault::new(io, service_name, &data_dir)?;
    let token = generate_session_token();
    let ready = serde_json::to_string(&Ready {
        kind: "ready".into(),
        token: token.clone(),
    })?;
    {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        writeln!(writer, "{ready}")?;
        writer.flush()?;
    }
    vault.run(token)
}
