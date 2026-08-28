//! Supervisor for the standalone signed `container` executable.

use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

mod container_integrity {
    include!(concat!(env!("OUT_DIR"), "/container_integrity.rs"));
}

pub struct ContainerProcess {
    child: Child,
    pub address: SocketAddr,
    token: String,
}

impl ContainerProcess {
    pub async fn spawn(binary: &Path, settings: &config::ContainersConfig) -> anyhow::Result<Self> {
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
                .arg("--general-environments")
                .arg(settings.environments.to_string());
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
                .kill_on_drop(true);

            let child = command.spawn().map_err(|err| {
                anyhow::anyhow!(
                    "failed to spawn verified container process {}: {err}",
                    binary.display()
                )
            })?;

            let mut process = Self {
                child,
                address,
                token,
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
        (self.address, self.token.clone(), self.child.id())
    }
}

fn verify_container(binary: &Path) -> anyhow::Result<()> {
    if container_integrity::EXPECTED_CONTAINER_SHA256.is_empty()
        || container_integrity::CONTAINER_PUBLIC_KEY_HEX.is_empty()
        || container_integrity::CONTAINER_SIGNATURE_HEX.is_empty()
    {
        anyhow::bail!("container dependency is not cryptographically bound to this backend build; refusing startup");
    }
    if !binary.is_file() {
        anyhow::bail!(
            "required container dependency is missing: {}",
            binary.display()
        );
    }

    let actual_hash = sha256_file(binary)?;
    if !constant_time_eq(
        actual_hash.as_bytes(),
        container_integrity::EXPECTED_CONTAINER_SHA256.as_bytes(),
    ) {
        anyhow::bail!(
            "container integrity check failed: SHA-256 mismatch (expected {}, got {})",
            container_integrity::EXPECTED_CONTAINER_SHA256,
            actual_hash
        );
    }

    let public_key_bytes = decode_exact::<32>(
        container_integrity::CONTAINER_PUBLIC_KEY_HEX,
        "container public key",
    )?;
    let signature_bytes = decode_exact::<64>(
        container_integrity::CONTAINER_SIGNATURE_HEX,
        "container signature",
    )?;
    let public_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|err| anyhow::anyhow!("invalid embedded container public key: {err}"))?;
    let signature = Signature::from_bytes(&signature_bytes);
    let statement = signing_statement(
        container_integrity::EXPECTED_CONTAINER_SHA256,
        container_integrity::CONTAINER_BUILD_ID,
        container_integrity::CONTAINER_TARGET,
    );
    public_key
        .verify(statement.as_bytes(), &signature)
        .map_err(|err| anyhow::anyhow!("container signature verification failed: {err}"))?;
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
}
