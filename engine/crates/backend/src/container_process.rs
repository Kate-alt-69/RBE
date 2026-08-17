//! Supervisor for the standalone `container` executable.
//!
//! The backend owns the process lifetime, but the container process owns all
//! execution state. The control endpoint is intentionally loopback-only and
//! the authentication token is generated per backend boot and never logged.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::time::Duration;

use rand::RngCore;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

pub struct ContainerProcess {
    child: Child,
    pub address: SocketAddr,
}

impl ContainerProcess {
    pub async fn spawn(binary: &Path) -> anyhow::Result<Self> {
        let port = reserve_loopback_port()?;
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let token = generate_token();

        let child = Command::new(binary)
            .arg("--listen")
            .arg(address.to_string())
            .env("RBE_CONTAINER_TOKEN", &token)
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| anyhow::anyhow!("failed to spawn container process {}: {err}", binary.display()))?;

        let mut process = Self { child, address };
        process.wait_for_control_socket().await?;
        tracing::info!(pid = process.child.id(), address = %address, "container process is ready");
        Ok(process)
    }

    async fn wait_for_control_socket(&mut self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(10), async {
            loop {
                match tokio::net::TcpStream::connect(self.address).await {
                    Ok(_) => return Ok(()),
                    Err(_) => {
                        if let Some(status) = self.child.try_wait()? {
                            anyhow::bail!("container process exited before control socket became ready: {status}");
                        }
                        sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for container control socket at {}", self.address))??;
        Ok(())
    }

    pub fn pid(&self) -> Option<u32> { self.child.id() }
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
        let port = reserve_loopback_port().unwrap();
        assert!(port > 0);
    }

    #[test]
    fn token_has_256_bit_length() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
    }
}
