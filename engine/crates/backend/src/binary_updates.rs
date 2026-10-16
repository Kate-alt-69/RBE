//! Opt-in standalone updates. Downloads never overwrite active files. A copied
//! helper takes ownership only after the old backend drains and closes its
//! lifetime pipe; a journal retains the complete previous bundle until readiness.
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const MAX_FILE: u64 = 1024 * 1024 * 1024;
const MAX_PART: u64 = 80 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binary {
    pub size: u64,
    pub sha256: String,
    pub parts: Vec<Part>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    pub tag: String,
    pub source: String,
    pub run: u64,
    pub attempt: u64,
    pub update_protocol: u32,
    pub credential_format: u32,
    pub targets: BTreeMap<String, BTreeMap<String, Binary>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    root: PathBuf,
    stage: PathBuf,
    cwd: PathBuf,
    args: Vec<String>,
    manifest: Manifest,
    host: String,
    port: u16,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    prepared: Prepared,
    previous: BTreeMap<String, String>,
}

pub fn identity() -> serde_json::Value {
    serde_json::json!({"version": env!("RBE_VERSION"), "base_version": env!("RBE_BASE_VERSION"),
		"source": env!("RBE_SOURCE_COMMIT"), "target": env!("RBE_TARGET"),
		"update_protocol": 1, "credential_format": 1,
		"deferred_admin_setup": api::admin_credentials::deferred()})
}
fn safe_tag(tag: &str) -> bool {
    tag == "rbe-binaries-latest"
        || (tag.starts_with("rbe-v")
            && tag.len() < 100
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            && !tag.contains(".."))
}
fn hash_valid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn names() -> Vec<String> {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    ["backend", "cloud_node", "dep/container", "dep/service"]
        .iter()
        .map(|v| format!("{v}{ext}"))
        .collect()
}
fn state(root: &Path) -> PathBuf {
    root.join(".rbe-updates")
}
fn journal(root: &Path) -> PathBuf {
    state(root).join("transaction.json")
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .user_agent("RBE-binary-updater/1")
        .build()?)
}
fn url(reference: &str, path: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{reference}/{path}",
        env!("RBE_BINARY_REPOSITORY")
    )
}
fn fetch_manifest(reference: &str) -> Result<Manifest> {
    if !safe_tag(reference) {
        bail!("invalid binary tag");
    }
    let mut bytes = Vec::new();
    client()?
        .get(url(reference, "manifest.json"))
        .send()?
        .error_for_status()?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        bail!("binary manifest is too large");
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    validate(&manifest)?;
    Ok(manifest)
}
fn version_matches_sequence(m: &Manifest) -> bool {
    let Some((base, build)) = m.version.split_once("-dev.") else {
        return false;
    };
    let parts = base.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && (*p == "0" || !p.starts_with('0'))
                && p.bytes().all(|b| b.is_ascii_digit())
        })
        && m.run > 0
        && m.attempt > 0
        && build == format!("{}.{}", m.run, m.attempt)
}
fn validate(m: &Manifest) -> Result<()> {
    if !version_matches_sequence(m)
        || m.format != 1
        || m.update_protocol != 1
        || m.credential_format != 1
        || m.tag != format!("rbe-v{}", m.version)
        || !safe_tag(&m.tag)
        || m.source.len() != 40
        || !m.source.bytes().all(|b| b.is_ascii_hexdigit())
        || m.version.split('.').next() != env!("RBE_BASE_VERSION").split('.').next()
    {
        bail!("incompatible binary snapshot; major/protocol/credential changes require manual installation");
    }
    let files = m
        .targets
        .get(env!("RBE_TARGET"))
        .context("snapshot has no bundle for this platform")?;
    if files.keys().cloned().collect::<Vec<_>>() != {
        let mut required = names();
        required.sort();
        required
    } {
        bail!("snapshot must contain exactly the complete runtime bundle");
    }
    for (name, file) in files {
        if file.size == 0
            || file.size > MAX_FILE
            || !hash_valid(&file.sha256)
            || file.parts.is_empty()
            || file.parts.len() > 128
        {
            bail!("invalid binary file bounds");
        }
        let mut total = 0u64;
        for (index, part) in file.parts.iter().enumerate() {
            let expected = if file.parts.len() == 1 {
                format!("binaries/{}/{name}", env!("RBE_TARGET"))
            } else {
                format!("binaries/{}/{name}.part{index:04}", env!("RBE_TARGET"))
            };
            if part.path != expected
                || part.size == 0
                || part.size > MAX_PART
                || !hash_valid(&part.sha256)
            {
                bail!("invalid binary part");
            }
            total = total
                .checked_add(part.size)
                .context("binary size overflow")?;
        }
        if total != file.size {
            bail!("binary part sizes disagree with the file size");
        }
    }
    Ok(())
}
fn digest(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("next");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    if path.exists() {
        bail!("refusing to overwrite an existing update journal");
    }
    fs::rename(temporary, path)?;
    #[cfg(unix)]
    File::open(path.parent().context("missing journal directory")?)?.sync_all()?;
    Ok(())
}
fn guard(root: &Path) -> Result<File> {
    api::admin_credentials::private_directory(&state(root))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state(root).join("update.lock"))?;
    file.try_lock_exclusive()
        .context("another updater owns this installation")?;
    Ok(file)
}
fn managed_host() -> bool {
    ["RENDER", "INVOCATION_ID", "KUBERNETES_SERVICE_HOST"]
        .iter()
        .any(|v| std::env::var_os(v).is_some())
}
pub fn check(tag: &str) -> Result<Manifest> {
    let first = fetch_manifest(tag)?;
    // Resolve the moving alias once, then use only the immutable snapshot tag.
    let pinned = fetch_manifest(&first.tag)?;
    if first != pinned {
        bail!("binary tag changed during resolution; retry later");
    }
    Ok(pinned)
}
/// Capture compiler diagnostics without pipe deadlock or an unbounded wait.
/// Private temporary files also cap output from a defective candidate process.
fn probe(
    command: &mut Command,
    directory: &Path,
    timeout: Duration,
) -> Result<std::process::Output> {
    let id = token();
    let stdout = directory.join(format!("probe-{id}.out"));
    let stderr = directory.join(format!("probe-{id}.err"));
    let result = (|| -> Result<std::process::Output> {
        let out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout)?;
        let err = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr)?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()?;
        let until = Instant::now() + timeout;
        loop {
            let too_large = [&stdout, &stderr]
                .iter()
                .any(|p| fs::metadata(p).is_ok_and(|m| m.len() > 2 * 1024 * 1024));
            if too_large || Instant::now() >= until {
                child.kill()?;
                child.wait()?;
                bail!("candidate probe exceeded its time or diagnostic output limit");
            }
            if let Some(status) = child.try_wait()? {
                let read = |path: &Path| -> Result<Vec<u8>> {
                    let mut bytes = Vec::new();
                    File::open(path)?
                        .take(2 * 1024 * 1024 + 1)
                        .read_to_end(&mut bytes)?;
                    if bytes.len() > 2 * 1024 * 1024 {
                        bail!("candidate diagnostic output is too large");
                    }
                    Ok(bytes)
                };
                return Ok(std::process::Output {
                    status,
                    stdout: read(&stdout)?,
                    stderr: read(&stderr)?,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })();
    let _ = fs::remove_file(stdout);
    let _ = fs::remove_file(stderr);
    result
}
pub fn stage(root: &Path, tag: &str, args: Vec<String>) -> Result<Option<Prepared>> {
    if managed_host() {
        bail!("use the hosting platform's deployment workflow; in-place updates are disabled here");
    }
    let _guard = guard(root)?;
    if journal(root).exists() {
        bail!("an interrupted update requires backend --update-recover");
    }
    let manifest = check(tag)?;
    if manifest.version == env!("RBE_VERSION") && manifest.source == env!("RBE_SOURCE_COMMIT") {
        return Ok(None);
    }
    let stage = state(root).join("staged").join(&manifest.tag);
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    api::admin_credentials::private_directory(&stage)?;
    let files = &manifest.targets[env!("RBE_TARGET")];
    let http = client()?;
    let download_deadline = Instant::now() + Duration::from_secs(600);
    for (name, binary) in files {
        let path = stage.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut full_hash = Sha256::new();
        for part in &binary.parts {
            let remaining = download_deadline
                .checked_duration_since(Instant::now())
                .context("binary staging exceeded its download time limit")?;
            let response = http
                .get(url(&manifest.tag, &part.path))
                .timeout(remaining.min(Duration::from_secs(180)))
                .send()?
                .error_for_status()?;
            let mut bounded = response.take(part.size + 1);
            let mut part_hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = bounded.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > part.size {
                    bail!("binary download exceeded its declared size");
                }
                part_hash.update(&buffer[..n]);
                full_hash.update(&buffer[..n]);
                out.write_all(&buffer[..n])?;
            }
            if size != part.size || hex::encode(part_hash.finalize()) != part.sha256 {
                bail!("binary part checksum mismatch");
            }
        }
        out.sync_all()?;
        if hex::encode(full_hash.finalize()) != binary.sha256 {
            bail!("binary checksum mismatch");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut version = Command::new(stage.join(&names()[0]));
    let output = probe(
        version.arg("--version-json"),
        &state(root),
        Duration::from_secs(30),
    )?;
    let reported: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if !output.status.success()
        || reported["version"] != manifest.version
        || reported["source"] != manifest.source
        || reported["target"] != env!("RBE_TARGET")
        || reported["deferred_admin_setup"] != true
    {
        bail!("candidate executable identity does not match the verified manifest");
    }
    let settings = args
        .windows(2)
        .find(|p| p[0] == "--settings")
        .map(|p| PathBuf::from(&p[1]))
        .or_else(|| {
            args.iter()
                .any(|a| a == "--allow-settings-env")
                .then(|| std::env::var_os("SETTINGS_PATH").map(PathBuf::from))
                .flatten()
        })
        .unwrap_or_else(|| PathBuf::from("settings.json"))
        .canonicalize()?;
    let mut candidate = Command::new(stage.join(&names()[0]));
    candidate
        .arg("--update-preflight")
        .arg(root)
        .arg("--settings")
        .arg(settings);
    let output = probe(&mut candidate, &state(root), Duration::from_secs(180))?;
    if !output.status.success() {
        bail!(
            "candidate project preflight failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let host = result["host"]
        .as_str()
        .context("candidate preflight has no host")?
        .to_string();
    if host.parse::<std::net::IpAddr>().is_err() && host != "localhost" {
        bail!("automatic update readiness requires a local IP listener or localhost");
    }
    let port = result["port"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= u16::MAX as u64)
        .context("invalid candidate API port")? as u16;
    Ok(Some(Prepared {
        root: root.to_path_buf(),
        stage,
        cwd: std::env::current_dir()?,
        args: clean_args(&args),
        manifest,
        host,
        port,
    }))
}

/// Spawn from a copy, never from a file that the worker will replace. Forget the
/// pipe writer deliberately: OS process exit closes it after Tokio/child cleanup.
pub fn handoff(prepared: Prepared, recover: bool) -> Result<()> {
    if !recover {
        api::admin_credentials::prepare_for_update()?;
    }
    let directory = state(&prepared.root);
    api::admin_credentials::private_directory(&directory)?;
    let helper = directory.join(format!(
        "worker-{}{}",
        token(),
        if cfg!(windows) { ".exe" } else { "" }
    ));
    fs::copy(std::env::current_exe()?, &helper)?;
    let mut child = Command::new(helper)
        .arg(if recover {
            "--update-rollback-worker"
        } else {
            "--update-worker"
        })
        .stdin(Stdio::piped())
        .spawn()?;
    let mut pipe = child
        .stdin
        .take()
        .context("updater lease was not created")?;
    serde_json::to_writer(&mut pipe, &prepared)?;
    pipe.write_all(b"\n")?;
    pipe.flush()?;
    std::mem::forget(pipe);
    Ok(())
}
/// If helper preparation fails after draining, bring the unchanged installation
/// back without polling the same update again. No journal/swap has started yet.
pub fn handoff_after_drain(prepared: Prepared) -> Result<()> {
    match handoff(prepared.clone(), false) {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("Could not start updater; restarting the unchanged backend: {error:#}");
            Command::new(std::env::current_exe()?)
                .args(rollback_args(&prepared.args))
                .current_dir(&prepared.cwd)
                .spawn()
                .context(
                    "could not restart the unchanged backend after updater preparation failed",
                )?;
            Ok(())
        }
    }
}
fn token() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
fn restore(root: &Path, previous: &BTreeMap<String, String>) -> Result<()> {
    for name in names() {
        let saved = state(root).join("previous").join(&name);
        let installed = root.join(&name);
        if saved.exists() {
            if digest(&saved)? != previous[&name] {
                bail!("rollback bundle checksum mismatch");
            }
            if installed.exists() {
                fs::remove_file(&installed)?;
            }
            fs::rename(saved, &installed)?;
        }
        if digest(&installed)? != previous[&name] {
            bail!("previous runtime could not be fully restored");
        }
    }
    Ok(())
}
/// Keep the journal until all four saved hashes have been restored. A partial
/// recovery remains recoverable if a Windows executable is still locked.
fn rollback_transaction(p: &Prepared, previous: &BTreeMap<String, String>) -> Result<()> {
    restore(&p.root, previous)?;
    fs::remove_file(journal(&p.root))?;
    Ok(())
}
fn install(p: &Prepared) -> Result<BTreeMap<String, String>> {
    validate(&p.manifest)?;
    let mut previous = BTreeMap::new();
    for name in names() {
        if digest(&p.stage.join(&name))? != p.manifest.targets[env!("RBE_TARGET")][&name].sha256 {
            bail!("staged binary changed after verification");
        }
        previous.insert(name.clone(), digest(&p.root.join(&name))?);
    }
    let backup = state(&p.root).join("previous");
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    api::admin_credentials::private_directory(&backup)?;
    write_json(
        &journal(&p.root),
        &Journal {
            prepared: p.clone(),
            previous: previous.clone(),
        },
    )?;
    let result = (|| -> Result<()> {
        for name in names() {
            let saved = backup.join(&name);
            fs::create_dir_all(saved.parent().unwrap())?;
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match fs::rename(p.root.join(&name), &saved) {
                    Ok(()) => break,
                    Err(error)
                        if Instant::now() < deadline
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::PermissionDenied
                                    | std::io::ErrorKind::WouldBlock
                            ) =>
                    {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            fs::rename(p.stage.join(&name), p.root.join(&name))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        rollback_transaction(p, &previous)?;
        return Err(error.context("update swap failed; previous bundle restored"));
    }
    Ok(previous)
}
fn spawn_backend(p: &Prepared, nonce: &str) -> Result<(Child, ChildStdin)> {
    let mut child = Command::new(p.root.join(&names()[0]))
        .args(&p.args)
        .arg("--update-parent-lease")
        .arg("--update-ready")
        .arg(nonce)
        .current_dir(&p.cwd)
        .stdin(Stdio::piped())
        .spawn()?;
    let lease = child
        .stdin
        .take()
        .context("new backend lease was not created")?;
    Ok((child, lease))
}
fn stop_backend(child: &mut Child, lease: ChildStdin) -> Result<()> {
    drop(lease);
    let until = Instant::now() + Duration::from_secs(120);
    while child.try_wait()?.is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    child.wait()?;
    Ok(())
}
fn wait_ready(p: &Prepared, child: &mut Child, nonce: &str) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(120);
    let host = match p.host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            if ip.is_unspecified() {
                "127.0.0.1".into()
            } else {
                ip.to_string()
            }
        }
        Ok(std::net::IpAddr::V6(ip)) => format!(
            "[{}]",
            if ip.is_unspecified() {
                std::net::Ipv6Addr::LOCALHOST
            } else {
                ip
            }
        ),
        Err(_) if p.host == "localhost" => "127.0.0.1".into(),
        Err(_) => bail!("automatic update readiness requires a local IP listener or localhost"),
    };
    let http = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    while Instant::now() < until {
        if child.try_wait()?.is_some() {
            bail!("candidate exited before readiness");
        }
        if let Ok(bytes) = fs::read(state(&p.root).join(format!("ready-{nonce}.json"))) {
            let ready: serde_json::Value = serde_json::from_slice(&bytes)?;
            if ready["version"] == p.manifest.version
                && ready["source"] == p.manifest.source
                && ready["pid"] == child.id()
                && ready["nonce"] == nonce
            {
                if let Ok(response) = http.get(format!("http://{host}:{}/health", p.port)).send() {
                    if response.status().is_success()
                        && response
                            .json::<serde_json::Value>()
                            .ok()
                            .is_some_and(|v| v["ok"] == true)
                    {
                        return Ok(());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!("candidate readiness timed out")
}

pub async fn worker(recover: bool) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(180),
        tokio::io::stdin()
            .take(2 * 1024 * 1024)
            .read_to_end(&mut bytes),
    )
    .await??;
    let p: Prepared = serde_json::from_slice(&bytes)?;
    let _guard = guard(&p.root)?;
    let notice =
        crate::maintenance_notice::MaintenanceNoticeProcess::spawn(&p.host, p.port).await?;
    let fallback = p.clone();
    let result =
        tokio::task::spawn_blocking(move || -> Result<(Prepared, BTreeMap<String, String>)> {
            let previous = if recover {
                let j: Journal = serde_json::from_slice(&fs::read(journal(&p.root))?)?;
                validate_journal(&p.root, &j)?;
                restore(&p.root, &j.previous)?;
                fs::remove_file(journal(&p.root))?;
                BTreeMap::new()
            } else {
                install(&p)?
            };
            Ok((p, previous))
        })
        .await?;
    // Both the old maintenance listener and the new boot listener use this port.
    // Release it explicitly before launching; the candidate owns boot maintenance.
    notice.stop().await;
    let (p, previous) = match result {
        Ok(value) => value,
        Err(error) => {
            if journal(&fallback.root).exists() {
                return Err(error.context("update journal retained; run backend --update-recover"));
            }
            eprintln!("Update swap was rejected; restarting the unchanged bundle: {error:#}");
            drop(_guard);
            return restart_previous(&fallback).await;
        }
    };
    if recover {
        drop(_guard);
        return restart_previous(&p).await;
    }
    let nonce = token();
    let (mut child, lease) = match spawn_backend(&p, &nonce) {
        Ok(value) => value,
        Err(error) => {
            rollback_transaction(&p, &previous)?;
            eprintln!("Candidate could not launch; restarting the previous bundle: {error:#}");
            drop(_guard);
            return restart_previous(&p).await;
        }
    };
    let ready_path = state(&p.root).join(format!("ready-{nonce}.json"));
    let checked = tokio::task::spawn_blocking(move || {
        let ready = wait_ready(&p, &mut child, &nonce);
        (p, previous, child, lease, ready)
    })
    .await?;
    let (p, previous, mut child, lease, ready) = checked;
    let _ = fs::remove_file(ready_path);
    if let Err(error) = ready {
        let p = tokio::task::spawn_blocking(move || -> Result<Prepared> {
            stop_backend(&mut child, lease)?;
            rollback_transaction(&p, &previous)?;
            eprintln!("Update failed readiness; restarting the previous bundle: {error:#}");
            Ok(p)
        })
        .await??;
        drop(_guard);
        restart_previous(&p).await?;
    } else {
        fs::remove_file(journal(&p.root))?;
        // The committed candidate must be able to stage its next update. Only
        // the file transaction, not lifetime supervision, owns the update lock.
        drop(_guard);
        supervise(child, lease).await?;
    }
    Ok(())
}

async fn supervise(mut child: Child, lease: ChildStdin) -> Result<()> {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if child.try_wait()?.is_some() { return Ok(()); }
            },
            _ = crate::shutdown_signal() => {
                return tokio::task::spawn_blocking(move || stop_backend(&mut child, lease)).await?;
            },
        }
    }
}
async fn restart_previous(p: &Prepared) -> Result<()> {
    let mut previous = p.clone();
    previous.args = rollback_args(&p.args);
    // A rollback is a regular boot, not a candidate readiness transaction.
    let mut child = Command::new(p.root.join(&names()[0]))
        .args(&previous.args)
        .arg("--update-parent-lease")
        .current_dir(&p.cwd)
        .stdin(Stdio::piped())
        .spawn()?;
    let lease = child
        .stdin
        .take()
        .context("rollback lease was not created")?;
    supervise(child, lease).await
}

pub fn mark_ready(args: &[String]) -> Result<()> {
    if let Some(nonce) = args
        .windows(2)
        .find(|p| p[0] == "--update-ready")
        .map(|p| &p[1])
    {
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid updater readiness token");
        }
        let root = runtime_paths::binary_dir();
        api::admin_credentials::private_directory(&state(&root))?;
        let mut value = identity();
        value["nonce"] = nonce.clone().into();
        value["pid"] = std::process::id().into();
        write_json(&state(&root).join(format!("ready-{nonce}.json")), &value)?;
    }
    Ok(())
}
pub fn interrupted() -> bool {
    journal(&runtime_paths::binary_dir()).exists()
}
pub fn recovery() -> Result<Prepared> {
    recovery_at(&runtime_paths::binary_dir())
}
/// The retained copied helper can restore even if interruption left backend
/// itself absent. The journal is private, bounded, and tied to this root.
pub fn recovery_at(root: &Path) -> Result<Prepared> {
    let root = root.canonicalize()?;
    api::admin_credentials::private_directory(&state(&root))?;
    let mut bytes = Vec::new();
    File::open(journal(&root))?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        bail!("update journal exceeds its size limit");
    }
    let j: Journal = serde_json::from_slice(&bytes)?;
    validate_journal(&root, &j)?;
    Ok(j.prepared)
}
fn validate_journal(root: &Path, j: &Journal) -> Result<()> {
    let mut required = names();
    required.sort();
    if j.prepared.root != root
        || j.previous.keys().cloned().collect::<Vec<_>>() != required
        || !j.previous.values().all(|v| hash_valid(v))
    {
        bail!("invalid or misplaced update recovery journal");
    }
    Ok(())
}

fn clean_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--update-ready" => at += 2,
            "--update-parent-lease" => at += 1,
            _ => {
                out.push(args[at].clone());
                at += 1;
            }
        }
    }
    out
}
fn rollback_args(args: &[String]) -> Vec<String> {
    clean_args(args)
        .into_iter()
        .filter(|arg| arg != "--auto-update")
        .collect()
}

/// Local preflight is deliberately independent of HostBootstrap, listeners, and
/// admin initialization. Only the candidate's compiler examines the live project.
pub fn preflight(root: &Path, settings: &Path) -> Result<serde_json::Value> {
    let root = root.canonicalize()?;
    let mut config = config::Config::load(settings)?;
    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile_from_root(&config.services, &io, &root)?;
    let compiled = crate::runtime_image_boot::compile_at_root(&config, catalog.as_ref(), &root)?;
    crate::runtime_image_boot::apply_server_policy(&mut config, &compiled.image.server_policy)?;
    crate::runtime_image_boot::apply_middleware_plan(&mut config, &compiled.image.middleware_plan)?;
    Ok(
        serde_json::json!({"host": config.api.host, "port": config.api.port, "image": compiled.image.image_id}),
    )
}

pub async fn parent_lease_closed() {
    if std::env::args().any(|a| a == "--update-parent-lease") {
        let mut input = [0u8; 1];
        use tokio::io::AsyncReadExt;
        let _ = tokio::io::stdin().read(&mut input).await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        prepared: Prepared,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("rbe-update-test-{}", token()));
            let stage = root.join(".rbe-updates/staged/test");
            let mut files = BTreeMap::new();
            for name in names() {
                for directory in [&root, &stage] {
                    fs::create_dir_all(directory.join(&name).parent().unwrap()).unwrap();
                }
                fs::write(root.join(&name), format!("old {name}")).unwrap();
                fs::write(stage.join(&name), format!("new {name}")).unwrap();
                let size = fs::metadata(stage.join(&name)).unwrap().len();
                let hash = digest(&stage.join(&name)).unwrap();
                files.insert(
                    name.clone(),
                    Binary {
                        size,
                        sha256: hash.clone(),
                        parts: vec![Part {
                            path: format!("binaries/{}/{name}", env!("RBE_TARGET")),
                            size,
                            sha256: hash,
                        }],
                    },
                );
            }
            let manifest = Manifest {
                format: 1,
                version: format!("{}-dev.1.1", env!("RBE_BASE_VERSION")),
                tag: format!("rbe-v{}-dev.1.1", env!("RBE_BASE_VERSION")),
                source: "a".repeat(40),
                run: 1,
                attempt: 1,
                update_protocol: 1,
                credential_format: 1,
                targets: [(env!("RBE_TARGET").to_string(), files)].into(),
            };
            let prepared = Prepared {
                root: root.clone(),
                stage,
                cwd: root.clone(),
                args: vec![],
                manifest,
                host: "127.0.0.1".into(),
                port: 8080,
            };
            Self { root, prepared }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn rejects_incomplete_foreign_and_traversing_manifests() {
        let fixture = Fixture::new();
        let m = &fixture.prepared.manifest;
        validate(m).unwrap();
        let mut missing = m.clone();
        missing
            .targets
            .get_mut(env!("RBE_TARGET"))
            .unwrap()
            .remove(&names()[0]);
        assert!(validate(&missing).is_err());
        for bad_path in ["../../backend", "/backend", "binaries/other/backend"] {
            let mut bad = m.clone();
            bad.targets
                .get_mut(env!("RBE_TARGET"))
                .unwrap()
                .get_mut(&names()[0])
                .unwrap()
                .parts[0]
                .path = bad_path.into();
            assert!(validate(&bad).is_err());
        }
        let mut future = m.clone();
        future.update_protocol += 1;
        assert!(validate(&future).is_err());
        let mut oversized = m.clone();
        oversized
            .targets
            .get_mut(env!("RBE_TARGET"))
            .unwrap()
            .get_mut(&names()[0])
            .unwrap()
            .size = MAX_FILE + 1;
        assert!(validate(&oversized).is_err());
    }
    #[test]
    fn complete_swap_and_rollback_preserve_application_files() {
        let f = Fixture::new();
        fs::write(f.root.join("settings.json"), "operator settings").unwrap();
        fs::create_dir_all(f.root.join("data/admin/control-room")).unwrap();
        fs::write(
            f.root.join("data/admin/control-room/credentials.json"),
            "operator verifier",
        )
        .unwrap();
        let previous = install(&f.prepared).unwrap();
        for name in names() {
            assert_eq!(
                fs::read(f.root.join(&name)).unwrap(),
                format!("new {name}").as_bytes()
            );
        }
        assert!(journal(&f.root).exists());
        restore(&f.root, &previous).unwrap();
        for name in names() {
            assert_eq!(
                fs::read(f.root.join(&name)).unwrap(),
                format!("old {name}").as_bytes()
            );
        }
        assert_eq!(
            fs::read_to_string(f.root.join("settings.json")).unwrap(),
            "operator settings"
        );
        assert_eq!(
            fs::read_to_string(f.root.join("data/admin/control-room/credentials.json")).unwrap(),
            "operator verifier"
        );
    }
    #[cfg(unix)]
    #[test]
    fn candidate_probes_are_bounded_and_clean_up_diagnostics() {
        let f = Fixture::new();
        let directory = state(&f.root);
        let output = probe(
            Command::new("sh").args(["-c", "printf ok; printf diagnostic >&2"]),
            &directory,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
        assert_eq!(output.stderr, b"diagnostic");
        assert!(probe(
            Command::new("sh").args(["-c", "while :; do :; done"]),
            &directory,
            Duration::from_millis(50)
        )
        .is_err());
        assert!(!fs::read_dir(&directory).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("probe-")));
    }
    #[test]
    fn failed_candidate_launch_restores_and_clears_transaction() {
        let f = Fixture::new();
        let previous = install(&f.prepared).unwrap();
        assert!(spawn_backend(&f.prepared, &token()).is_err());
        rollback_transaction(&f.prepared, &previous).unwrap();
        assert!(!journal(&f.root).exists());
        for name in names() {
            assert_eq!(
                fs::read(f.root.join(&name)).unwrap(),
                format!("old {name}").as_bytes()
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn exited_candidate_fails_readiness_without_accepting_a_health_response() {
        let f = Fixture::new();
        let mut child = Command::new("sh").args(["-c", "exit 1"]).spawn().unwrap();
        let failure = wait_ready(&f.prepared, &mut child, &token());
        assert!(failure
            .unwrap_err()
            .to_string()
            .contains("exited before readiness"));
    }
    #[test]
    fn changed_stage_is_rejected_before_any_active_file_moves() {
        let f = Fixture::new();
        fs::write(f.prepared.stage.join(&names()[2]), "tampered").unwrap();
        assert!(install(&f.prepared).is_err());
        assert!(!journal(&f.root).exists());
        for name in names() {
            assert_eq!(
                fs::read(f.root.join(&name)).unwrap(),
                format!("old {name}").as_bytes()
            );
        }
    }
    #[test]
    fn recovery_restores_a_crash_between_individual_renames() {
        let f = Fixture::new();
        let previous: BTreeMap<String, String> = names()
            .into_iter()
            .map(|n| {
                let hash = digest(&f.root.join(&n)).unwrap();
                (n, hash)
            })
            .collect();
        for name in names().iter().take(2) {
            let backup = state(&f.root).join("previous").join(name);
            fs::create_dir_all(backup.parent().unwrap()).unwrap();
            fs::rename(f.root.join(name), backup).unwrap();
            if name == &names()[0] {
                fs::rename(f.prepared.stage.join(name), f.root.join(name)).unwrap();
            }
        }
        restore(&f.root, &previous).unwrap();
        for name in names() {
            assert_eq!(
                fs::read(f.root.join(&name)).unwrap(),
                format!("old {name}").as_bytes()
            );
        }
    }
    #[test]
    fn next_update_can_take_lock_after_transaction_commits() {
        let f = Fixture::new();
        let first = guard(&f.root).unwrap();
        assert!(guard(&f.root).is_err());
        drop(first);
        assert!(guard(&f.root).is_ok());
        let args = vec![
            "--settings".into(),
            "my settings.json".into(),
            "--auto-update".into(),
            "--update-ready".into(),
            "nonce".into(),
            "--update-parent-lease".into(),
        ];
        assert_eq!(rollback_args(&args), vec!["--settings", "my settings.json"]);
    }
}
