//! Local-only initialization before normal backend listeners are opened.
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Read};
use zeroize::Zeroizing;

pub fn initialize(explicit: bool) -> Result<()> {
    // Remove plaintext before any worker can inherit the process environment.
    let environment_password = std::env::var("RBE_ADMIN_PASSWORD").ok().map(Zeroizing::new);
    std::env::remove_var("RBE_ADMIN_PASSWORD");
    if api::admin_credentials::load()? {
        if explicit {
            println!("Administrator credentials are already configured; no password was replaced.");
        }
        return Ok(());
    }
    if !api::admin_credentials::deferred() {
        if explicit {
            bail!("rebuild with --deny-admin-setup to enable installation-owned credentials");
        }
        return Ok(());
    }
    let password = if let Some(password) = environment_password {
        password
    } else if let Some(path) = std::env::var_os("RBE_ADMIN_PASSWORD_FILE") {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .context("could not open the administrator secret file")?
            .take(1027)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1026 {
            bail!("administrator secret file exceeds its size limit");
        }
        let text = Zeroizing::new(
            String::from_utf8(bytes).context("administrator secret file must contain UTF-8")?,
        );
        Zeroizing::new(text.trim_end_matches(['\r', '\n']).to_string())
    } else if std::io::stdin().is_terminal() {
        let first = Zeroizing::new(rpassword::prompt_password(
            "Create RBE administrator password (12+ bytes): ",
        )?);
        let second = Zeroizing::new(rpassword::prompt_password(
            "Confirm administrator password: ",
        )?);
        if first != second {
            bail!("administrator passwords did not match");
        }
        first
    } else {
        if explicit {
            bail!(
                "set RBE_ADMIN_PASSWORD or RBE_ADMIN_PASSWORD_FILE for unattended initialization"
            );
        }
        eprintln!("Control Room locked: initialize locally with backend --init-admin, or set RBE_ADMIN_PASSWORD for the first unattended boot.");
        return Ok(());
    };
    api::admin_credentials::initialize(&password)?;
    println!("Administrator verifier stored in protected installation storage.");
    Ok(())
}
