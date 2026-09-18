//! Supervisor for the standalone signed `container` executable.

use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::{sleep, timeout};

mod container_integrity {
    include!(concat!(env!("OUT_DIR"), "/container_integrity.rs"));
}

pub struct ContainerProcess {
    child: Child,
    _parent_liveness: ChildStdin,
    pub address: SocketAddr,
    token: String,
    pid: Option<u32>,
    started_at: Instant,
}

impl ContainerProcess {
    pub async fn spawn(
        binary: &Path,
        settings: &config::ContainersConfig,
        host_capability: &crate::host_capability::HostCapabilityEndpoint,
        project_root: &Path,
    ) -> anyhow::Result<Self> {
        if !project_root.is_absolute() || !project_root.is_dir() {
            anyhow::bail!(
                "frozen RBE project root must be an existing absolute directory: {}",
                project_root.display()
            );
        }
        verify_container(binary)?;
        const MAX_SPAWN_ATTEMPTS: u32 = 3;
        let mut last_err = None;

        for attempt in 1..=MAX_SPAWN_ATTEMPTS {
            let port = reserve_loopback_port()?;
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            let token = generate_token();

            let mut command = Command::new(binary);
            command
                .arg("--listen")
                .arg(address.to_string())
                // The browser dashboard belongs to backend.exe. The child keeps
                // its old standalone dashboard code only for direct debug runs.
                .arg("--no-dashboard")
                .arg("--parent-liveness-stdin")
                .arg("--application-root")
                .arg(project_root)
                .arg("--general-environments")
                .arg(settings.environments.to_string())
                .current_dir(project_root);
            if let Some(value) = settings.swamps_per_environment.fixed() {
                command
                    .arg("--swamps-per-environment")
                    .arg(value.to_string());
            }
            if let Some(value) = settings.workers_per_swamp.fixed() {
                command.arg("--workers-per-swamp").arg(value.to_string());
            }
            command
                .env("RBE_CONTAINER_TOKEN", &token)
                .env(
                    "RBE_HOST_CAPABILITY_ADDR",
                    host_capability.address().to_string(),
                )
                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())
                .stdin(std::process::Stdio::piped())
                .kill_on_drop(true);

            let mut child = command.spawn().map_err(|err| {
                anyhow::anyhow!(
                    "failed to spawn verified container process {}: {err}",
                    binary.display()
                )
            })?;
            let pid = child.id();
            let parent_liveness = child.stdin.take().ok_or_else(|| {
                anyhow::anyhow!("verified container parent liveness pipe was not created")
            })?;

            let mut process = Self {
                child,
                _parent_liveness: parent_liveness,
                address,
                token,
                pid,
                started_at: Instant::now(),
            };
            match process.wait_for_control_socket().await {
                Ok(()) => {
                    tracing::info!(
                        pid = process.child.id(), address = %address, attempt,
                        build_id = container_integrity::CONTAINER_BUILD_ID,
                        target = container_integrity::CONTAINER_TARGET,
                        "verified container process is ready"
                    );
                    return Ok(process);
                }
                Err(err) => {
                    tracing::warn!(attempt, address = %address, error = %err, "container process failed to become ready on this port, retrying with a fresh port");
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("failed to start container process after {MAX_SPAWN_ATTEMPTS} attempts")
        }))
    }

    pub fn packaged_path() -> anyhow::Result<PathBuf> {
        let executable = std::env::current_exe()
            .map_err(|err| anyhow::anyhow!("could not resolve backend executable path: {err}"))?;
        let root = executable
            .parent()
            .ok_or_else(|| anyhow::anyhow!("backend executable has no parent directory"))?;
        let name = if cfg!(windows) {
            "container.exe"
        } else {
            "container"
        };
        Ok(root.join("dep").join(name))
    }

    async fn wait_for_control_socket(&mut self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(10), async {
            loop {
                match tokio::net::TcpStream::connect(self.address).await {
                    Ok(_) => return Ok(()),
                    Err(_) => {
                        if let Some(status) = self.child.try_wait()? {
                            anyhow::bail!("verified container process exited before control socket became ready: {status}");
                        }
                        sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }).await.map_err(|_| anyhow::anyhow!("timed out waiting for container control socket at {}", self.address))??;
        Ok(())
    }

    pub fn endpoint(&self) -> (SocketAddr, String, Option<u32>) {
        (self.address, self.token.clone(), self.pid)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

fn container_dependency_missing(binary: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5001 Required packaged Container runtime is missing.\n\n  expected_path:\n    {}\n\n  action:\n    Rebuild/reinstall the complete RBE package for this target. Do not mix a Container binary from another build into this package.\n\n  help:\n    doc/error-codes/runtime.md#rbe5001",
        binary.display()
    )
}

fn container_binding_invalid(binary: &Path, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5002 Backend Container binding metadata is invalid.\n\n  container_path:\n    {}\n\n  reason:\n    {}\n\n  action:\n    Rebuild the complete RBE package for this target so backend and Container integrity metadata are generated together.\n\n  help:\n    doc/error-codes/runtime.md#rbe5002",
        binary.display(),
        reason
    )
}

fn container_integrity_failed(binary: &Path, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "RBE5003 Packaged Container runtime failed backend integrity verification.\n\n  container_path:\n    {}\n\n  reason:\n    {}\n\n  action:\n    Replace the package with a complete RBE build produced for this target. Do not copy Container binaries between backend builds.\n\n  help:\n    doc/error-codes/runtime.md#rbe5003",
        binary.display(),
        reason
    )
}

fn verify_container(binary: &Path) -> anyhow::Result<()> {
    if container_integrity::EXPECTED_CONTAINER_SHA256.is_empty()
        || container_integrity::CONTAINER_PUBLIC_KEY_HEX.is_empty()
        || container_integrity::CONTAINER_SIGNATURE_HEX.is_empty()
    {
        return Err(container_binding_invalid(
            binary,
            "required SHA-256/public-key/signature metadata was not embedded in this backend build",
        ));
    }
    if !binary.is_file() {
        return Err(container_dependency_missing(binary));
    }

    let actual_hash = sha256_file(binary).map_err(|error| {
        container_integrity_failed(
            binary,
            format!("could not read/hash Container binary: {error}"),
        )
    })?;
    if !constant_time_eq(
        actual_hash.as_bytes(),
        container_integrity::EXPECTED_CONTAINER_SHA256.as_bytes(),
    ) {
        return Err(container_integrity_failed(
            binary,
            format!(
                "SHA-256 mismatch (expected {}, got {})",
                container_integrity::EXPECTED_CONTAINER_SHA256,
                actual_hash
            ),
        ));
    }

    let public_key_bytes = decode_exact::<32>(
        container_integrity::CONTAINER_PUBLIC_KEY_HEX,
        "container public key",
    )
    .map_err(|error| container_binding_invalid(binary, error))?;
    let signature_bytes = decode_exact::<64>(
        container_integrity::CONTAINER_SIGNATURE_HEX,
        "container signature",
    )
    .map_err(|error| container_binding_invalid(binary, error))?;
    let public_key = VerifyingKey::from_bytes(&public_key_bytes).map_err(|error| {
        container_binding_invalid(
            binary,
            format!("invalid embedded Container public key: {error}"),
        )
    })?;
    let signature = Signature::from_bytes(&signature_bytes);
    let statement = signing_statement(
        container_integrity::EXPECTED_CONTAINER_SHA256,
        container_integrity::CONTAINER_BUILD_ID,
        container_integrity::CONTAINER_TARGET,
    );
    public_key
        .verify(statement.as_bytes(), &signature)
        .map_err(|error| {
            container_integrity_failed(
                binary,
                format!("Container signature verification failed: {error}"),
            )
        })?;
    Ok(())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn decode_exact<const N: usize>(hex_value: &str, label: &str) -> anyhow::Result<[u8; N]> {
    let bytes = hex::decode(hex_value).map_err(|err| anyhow::anyhow!("invalid {label}: {err}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid {label}: expected {} bytes", N))
}
fn signing_statement(hash: &str, build_id: &str, target: &str) -> String {
    format!("RBE-CONTAINER-INTEGRITY-V1\nsha256={hash}\nbuild_id={build_id}\ntarget={target}\n")
}
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
fn reserve_loopback_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserves_loopback_port() {
        assert!(reserve_loopback_port().unwrap() > 0);
    }
    #[test]
    fn token_has_256_bit_length() {
        assert_eq!(generate_token().len(), 64);
    }
    #[test]
    fn constant_time_compare_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn container_package_diagnostics_are_stable_and_actionable() {
        let path = Path::new("dep/container");

        let missing = container_dependency_missing(path).to_string();
        assert!(missing.starts_with("RBE5001 "));
        assert!(missing.contains("expected_path:"));
        assert!(missing.contains("doc/error-codes/runtime.md#rbe5001"));

        let binding = container_binding_invalid(path, "synthetic binding failure").to_string();
        assert!(binding.starts_with("RBE5002 "));
        assert!(binding.contains("synthetic binding failure"));
        assert!(binding.contains("doc/error-codes/runtime.md#rbe5002"));

        let integrity = container_integrity_failed(path, "synthetic integrity failure").to_string();
        assert!(integrity.starts_with("RBE5003 "));
        assert!(integrity.contains("synthetic integrity failure"));
        assert!(integrity.contains("doc/error-codes/runtime.md#rbe5003"));
    }
}
