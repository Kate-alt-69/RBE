//! User-facing RPX publisher workflows built on the frozen Kastrick contract.

use crate::auth_store::{CredentialSource, CredentialStore, ResolvedCredential};
use crate::publisher_client::{DevicePoll, PublishResponse, PublisherClient};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

pub fn login(registry_override: Option<&str>) -> Result<()> {
    let client = PublisherClient::from_override_or_env(registry_override)?;
    let store = CredentialStore::discover()?;
    let device = client.start_device()?;

    println!("RPX LOGIN");
    println!("  registry: {}", client.base_url());
    println!("  code: {}", device.user_code);
    println!("  authorize: {}", device.verification_uri);

    if open_browser(&device.verification_uri) {
        println!("  browser: opened authorization page");
    } else {
        println!("  browser: open the authorization URL above");
    }

    let interval = device.interval_seconds.clamp(1, 30);
    loop {
        if unix_now() >= device.expires_at {
            bail!("RPX device authorization expired before approval; run `rpx login` again");
        }
        match client.poll_device(&device.device_code)? {
            DevicePoll::Pending => thread::sleep(Duration::from_secs(interval)),
            DevicePoll::Authorized(token) => {
                store.save(&client.registry_key(), &token)?;
                println!("RPX LOGIN OK");
                println!("  credential: {}", store.path().display());
                println!("  expires: {}", token.expires_at);
                println!("  scopes: {}", scope_text(&token.scopes));
                return Ok(());
            }
        }
    }
}

pub fn whoami(registry_override: Option<&str>) -> Result<()> {
    let client = PublisherClient::from_override_or_env(registry_override)?;
    let store = CredentialStore::discover()?;
    let credential = require_credential(&store, &client.registry_key())?;
    let packages = client.publisher_packages(&credential.authorization)?;

    println!("RPX AUTHENTICATED");
    println!("  registry: {}", client.base_url());
    println!(
        "  credential: {}",
        match credential.source {
            CredentialSource::Environment => "RPX_TOKEN environment",
            CredentialSource::Store => "local RPX credential store",
        }
    );
    if let Some(expires_at) = credential.expires_at {
        println!("  expires: {expires_at}");
    }
    if !credential.scopes.is_empty() {
        println!("  scopes: {}", scope_text(&credential.scopes));
    }
    println!("  owned packages: {}", packages.packages.len());
    for package in packages.packages {
        println!(
            "    {}  latest={}  versions={}",
            package.name,
            package.latest_stable.as_deref().unwrap_or("none"),
            package.versions.len()
        );
    }
    Ok(())
}

pub fn logout(registry_override: Option<&str>) -> Result<()> {
    let client = PublisherClient::from_override_or_env(registry_override)?;
    let store = CredentialStore::discover()?;
    let Some(credential) = store.resolve(&client.registry_key())? else {
        println!("RPX LOGOUT OK");
        println!("  no active credential for {}", client.base_url());
        return Ok(());
    };

    let revoke = client.revoke(&credential.authorization);
    match credential.source {
        CredentialSource::Store => {
            store.remove(&client.registry_key())?;
            println!("RPX LOGOUT OK");
            println!("  local credential removed");
        }
        CredentialSource::Environment => {
            println!("RPX LOGOUT");
            println!("  credential came from RPX_TOKEN; unset that environment variable locally");
        }
    }
    revoke.context(
        "local logout completed, but the registry credential could not be revoked remotely",
    )?;
    Ok(())
}

pub fn publish_archive(
    registry_override: Option<&str>,
    package: &str,
    version: &str,
    archive: &Path,
) -> Result<PublishResponse> {
    let client = PublisherClient::from_override_or_env(registry_override)?;
    let store = CredentialStore::discover()?;
    let credential = require_credential(&store, &client.registry_key())?;
    require_publish_scope(&credential)?;

    println!("RPX PUBLISH");
    println!("  registry: {}", client.base_url());
    println!("  package: {package}@{version}");
    println!("  artifact: {}", archive.display());

    let prepared = client.prepare_upload(&credential.authorization, version)?;
    client.upload_archive(&prepared, archive)?;
    let published = client.finalize_publish(
        &credential.authorization,
        version,
        &prepared.upload_id,
    )?;
    if published.release.package != package || published.release.version != version {
        bail!(
            "RPX publisher returned a different package identity: expected {package}@{version}, got {}@{}",
            published.release.package,
            published.release.version
        );
    }

    println!("RPX PUBLISH OK");
    println!(
        "  name: {}",
        if published.claimed_name {
            "claimed on first publish"
        } else {
            "existing owned package"
        }
    );
    println!("  revision: {}", published.revision);
    println!("  sha256: {}", published.release.archive_sha256);
    println!("  manifest sha256: {}", published.release.manifest_sha256);
    println!("  bytes: {}", published.release.size_bytes);
    Ok(published)
}

fn require_credential(store: &CredentialStore, registry_key: &str) -> Result<ResolvedCredential> {
    store
        .resolve(registry_key)?
        .context("RPX is not logged in for this registry; run `rpx login`")
}

fn require_publish_scope(credential: &ResolvedCredential) -> Result<()> {
    if credential.source == CredentialSource::Environment || credential.scopes.is_empty() {
        return Ok(());
    }
    if !credential.scopes.iter().any(|scope| scope == "package.publish") {
        bail!("stored RPX credential does not grant package.publish; run `rpx login` again");
    }
    Ok(())
}

fn scope_text(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "server-managed".to_owned()
    } else {
        scopes.join(", ")
    }
}

fn open_browser(url: &str) -> bool {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(url);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    #[cfg(not(any(windows, unix)))]
    return false;

    command.spawn().is_ok()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
