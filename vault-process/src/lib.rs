use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const MONITOR_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_DELAY: Duration = Duration::from_millis(200);
const MAX_RESTARTS: u32 = 5;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    seq: u64,
    op: String,
    name: String,
    caller: String,
    value: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    kind: String,
    seq: u64,
    ok: bool,
    value: Option<String>,
    error: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ready {
    kind: String,
    token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct NeedsDbus {
    kind: String,
}

enum ClientCommand {
    Get {
        name: String,
        caller: String,
        response: Sender<anyhow::Result<String>>,
    },
    Set {
        name: String,
        value: String,
        caller: String,
        response: Sender<anyhow::Result<()>>,
    },
}

#[derive(Clone)]
pub struct VaultClient {
    tx: Sender<ClientCommand>,
}

impl VaultClient {
    /// Starts `backend.exe --vault --separate-process` and waits for the
    /// child Vault to complete its startup handshake. The Vault process is
    /// part of the same executable image, but owns its own address space.
    /// On Linux, the child can request a private D-Bus bootstrap; the client
    /// transparently relaunches it with `--dbus`.
    pub fn spawn(service_name: impl Into<String>, data_dir: &Path) -> anyhow::Result<Self> {
        let service_name = service_name.into();
        let data_dir = data_dir.to_path_buf();
        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("could not resolve backend executable: {e}"))?;

        let (tx, rx) = mpsc::channel::<ClientCommand>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);

        thread::Builder::new()
            .name("vault-process-io".to_string())
            .spawn(move || {
                let mut worker = Worker::new(exe, service_name, data_dir, ready_tx);
                worker.run(rx);
            })
            .map_err(|e| anyhow::anyhow!("failed to start Vault client thread: {e}"))?;

        match ready_rx.recv_timeout(STARTUP_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(anyhow::anyhow!("Vault did not become ready within {:?}", STARTUP_TIMEOUT))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("Vault client worker exited during startup"))
            }
        }
    }

    pub fn credential(&self, name: &str, caller: &str) -> anyhow::Result<secrecy::SecretString> {
        let (response_tx, response_rx) = mpsc::channel();
        self.tx
            .send(ClientCommand::Get {
                name: name.to_owned(),
                caller: caller.to_owned(),
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("Vault client worker is unavailable"))?;
        let value = response_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Vault client worker stopped responding"))??;
        Ok(secrecy::SecretString::new(value))
    }

    pub fn set_credential(&self, name: &str, value: &str, caller: &str) -> anyhow::Result<()> {
        let (response_tx, response_rx) = mpsc::channel();
        self.tx
            .send(ClientCommand::Set {
                name: name.to_owned(),
                value: value.to_owned(),
                caller: caller.to_owned(),
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("Vault client worker is unavailable"))?;
        response_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Vault client worker stopped responding"))?
    }
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    token: String,
    seq: u64,
}

struct Worker {
    exe: PathBuf,
    service_name: String,
    data_dir: PathBuf,
    ready_tx: Option<Sender<anyhow::Result<()>>>,
    connection: Option<Connection>,
}

impl Worker {
    fn new(
        exe: PathBuf,
        service_name: String,
        data_dir: PathBuf,
        ready_tx: Sender<anyhow::Result<()>>,
    ) -> Self {
        Self {
            exe,
            service_name,
            data_dir,
            ready_tx: Some(ready_tx),
            connection: None,
        }
    }

    fn run(&mut self, rx: Receiver<ClientCommand>) {
        if let Err(err) = self.ensure_connection() {
            if let Some(tx) = self.ready_tx.take() {
                let _ = tx.send(Err(err));
            }
            return;
        }

        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        loop {
            match rx.recv_timeout(MONITOR_INTERVAL) {
                Ok(command) => {
                    self.handle(command);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.connection_is_dead() {
                        let _ = self.restart_connection();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown_child();
                    return;
                }
            }
        }
    }

    fn handle(&mut self, command: ClientCommand) {
        if self.connection_is_dead() {
            if let Err(err) = self.restart_connection() {
                match command {
                    ClientCommand::Get { response, .. } => {
                        let _ = response.send(Err(err));
                    }
                    ClientCommand::Set { response, .. } => {
                        let _ = response.send(Err(err));
                    }
                }
                return;
            }
        }

        match command {
            ClientCommand::Get { name, caller, response } => {
                let result = self.request("get", &name, &caller, None);
                let _ = response.send(result);
            }
            ClientCommand::Set { name, value, caller, response } => {
                let result = self.request("set", &name, &caller, Some(value)).map(|_| ());
                let _ = response.send(result);
            }
        }
    }

    fn request(
        &mut self,
        op: &str,
        name: &str,
        caller: &str,
        value: Option<String>,
    ) -> anyhow::Result<String> {
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Vault connection is unavailable"))?;

        let seq = connection.seq;
        connection.seq = connection.seq.saturating_add(1);
        let request = Request {
            token: connection.token.clone(),
            seq,
            op: op.to_string(),
            name: name.to_string(),
            caller: caller.to_string(),
            value,
        };

        let encoded = serde_json::to_string(&request)?;
        writeln!(connection.stdin, "{encoded}")?;
        connection.stdin.flush()?;

        let mut line = String::new();
        connection.reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(anyhow::anyhow!("Vault process closed the protocol pipe"));
        }

        let response: Response = serde_json::from_str(&line)?;
        if response.seq != seq {
            return Err(anyhow::anyhow!("Vault protocol sequence mismatch"));
        }
        if !response.ok {
            return Err(anyhow::anyhow!(
                "Vault request failed: {}",
                response.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }

        Ok(response.value.unwrap_or_default())
    }

    fn connection_is_dead(&mut self) -> bool {
        let Some(connection) = self.connection.as_mut() else {
            return true;
        };
        match connection.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn ensure_connection(&mut self) -> anyhow::Result<()> {
        if self.connection.is_some() && !self.connection_is_dead() {
            return Ok(());
        }
        self.connection = None;
        let mut last_error = None;

        for _attempt in 1..=MAX_RESTARTS {
            match self.spawn_connection(false) {
                Ok(connection) => {
                    self.connection = Some(connection);
                    return Ok(());
                }
                Err(err) if err.to_string() == "VAULT_NEEDS_DBUS" => {
                    match self.spawn_connection(true) {
                        Ok(connection) => {
                            self.connection = Some(connection);
                            return Ok(());
                        }
                        Err(force_err) => last_error = Some(force_err),
                    }
                }
                Err(err) => last_error = Some(err),
            }

            thread::sleep(RESTART_DELAY);
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Vault failed to start")))
    }

    fn restart_connection(&mut self) -> anyhow::Result<()> {
        self.connection = None;
        self.ensure_connection()
    }

    fn spawn_connection(&self, force_dbus: bool) -> anyhow::Result<Connection> {
        let mut command = Command::new(&self.exe);
        command
            .arg("--vault")
            .arg("--separate-process")
            .arg("--service-name")
            .arg(&self.service_name)
            .arg("--data-dir")
            .arg(&self.data_dir);

        if force_dbus {
            command.arg("--dbus");
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn Vault process: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Vault stdin pipe unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Vault stdout pipe unavailable"))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;

        if line.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("Vault exited before handshake"));
        }

        if let Ok(needs) = serde_json::from_str::<NeedsDbus>(&line) {
            if needs.kind == "needs_dbus" {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("VAULT_NEEDS_DBUS"));
            }
        }

        let ready: Ready = serde_json::from_str(&line)
            .map_err(|e| anyhow::anyhow!("invalid Vault handshake: {e}"))?;
        if ready.kind != "ready" || ready.token.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("Vault returned an invalid ready handshake"));
        }

        Ok(Connection {
            child,
            stdin,
            reader,
            token: ready.token,
            seq: 1,
        })
    }

    fn shutdown_child(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.child.kill();
            let _ = connection.child.wait();
        }
    }
}

/// Entry point used by `backend.exe --vault --separate-process`.
/// Secrets never travel through the filesystem: the parent/backend and
/// Vault communicate only through inherited anonymous pipes, and every
/// request is bound to the fresh token generated by this Vault boot.
pub fn run_vault_daemon(service_name: String, data_dir: PathBuf, force_dbus: bool) -> anyhow::Result<()> {
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &data_dir);
    error_client::install_panic_hook();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();

    #[cfg(target_os = "linux")]
    if !force_dbus
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
            .map(|value| value.is_empty())
            .unwrap_or(true)
    {
        let message = serde_json::to_string(&NeedsDbus {
            kind: "needs_dbus".to_string(),
        })?;
        println!("{message}");
        std::io::stdout().flush()?;
        std::process::exit(10);
    }

    let vault = vault::Vault::new(io, service_name, &data_dir)?;
    let token = generate_session_token();

    let ready = serde_json::to_string(&Ready {
        kind: "ready".to_string(),
        token: token.clone(),
    })?;
    println!("{ready}");
    std::io::stdout().flush()?;

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();
    let mut expected_seq = 1u64;

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = Response {
                    kind: "response".to_string(),
                    seq: expected_seq,
                    ok: false,
                    value: None,
                    error: Some(format!("invalid request: {error}")),
                    token: None,
                };
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        };

        let response = if request.token != token {
            Response {
                kind: "response".to_string(),
                seq: request.seq,
                ok: false,
                value: None,
                error: Some("invalid Vault session token".to_string()),
                token: None,
            }
        } else if request.seq != expected_seq {
            Response {
                kind: "response".to_string(),
                seq: request.seq,
                ok: false,
                value: None,
                error: Some("invalid Vault request sequence".to_string()),
                token: None,
            }
        } else {
            expected_seq = expected_seq.saturating_add(1);
            match request.op.as_str() {
                "get" => match vault.credential(&request.name, &request.caller) {
                    Ok(value) => Response {
                        kind: "response".to_string(),
                        seq: request.seq,
                        ok: true,
                        value: Some(value.expose_secret().to_string()),
                        error: None,
                        token: None,
                    },
                    Err(error) => Response {
                        kind: "response".to_string(),
                        seq: request.seq,
                        ok: false,
                        value: None,
                        error: Some(format!("{error:#}")),
                        token: None,
                    },
                },
                "set" => match request.value.as_deref() {
                    Some(value) => match vault.set_credential(&request.name, value, &request.caller) {
                        Ok(()) => Response {
                            kind: "response".to_string(),
                            seq: request.seq,
                            ok: true,
                            value: None,
                            error: None,
                            token: None,
                        },
                        Err(error) => Response {
                            kind: "response".to_string(),
                            seq: request.seq,
                            ok: false,
                            value: None,
                            error: Some(format!("{error:#}")),
                            token: None,
                        },
                    },
                    None => Response {
                        kind: "response".to_string(),
                        seq: request.seq,
                        ok: false,
                        value: None,
                        error: Some("set request is missing value".to_string()),
                        token: None,
                    },
                },
                _ => Response {
                    kind: "response".to_string(),
                    seq: request.seq,
                    ok: false,
                    value: None,
                    error: Some(format!("unknown Vault operation {:?}", request.op)),
                    token: None,
                },
            }
        };

        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
        writer.flush()?;
    }

    Ok(())
}

fn generate_session_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
