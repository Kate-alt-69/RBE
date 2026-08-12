//! Boot-time port reclaim.
//!
//! **Correction to an earlier assumption in the migration plan:** §4's
//! Port Safety row originally dismissed this as "mostly obsolete" once
//! the backend collapses to one process. That was wrong — the Node
//! backend's real purpose here is cleaning up a *crashed prior run's*
//! orphaned listener (the process died without releasing the port),
//! which is exactly as real a problem for a single Rust binary as it
//! was for the Node launcher. Reversing that call.
//!
//! **Scoped for safety, unlike a blind "kill whatever holds this
//! port":** only kills a process if its image name matches our own
//! binary's — never an arbitrary process that happens to be bound to
//! the configured port. Best-effort throughout: any failure here
//! (command not found, unexpected output format, permission denied)
//! logs a warning and falls through to the normal bind attempt, which
//! will then fail with its own clear error if the port is genuinely
//! still held by something else. This should never be the reason boot
//! fails outright.

use std::process::Command;

pub fn reclaim_port_if_needed(port: u16) {
    if let Err(err) = try_reclaim(port) {
        tracing::warn!(port, error = %err, "port reclaim check failed (non-fatal, continuing)");
    }
}

fn own_binary_name() -> Option<String> {
    std::env::current_exe()
        .ok()?
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

#[cfg(target_os = "windows")]
fn try_reclaim(port: u16) -> anyhow::Result<()> {
    let Some(own_name) = own_binary_name() else {
        return Ok(());
    };

    let netstat = Command::new("netstat").args(["-ano"]).output()?;
    let output = String::from_utf8_lossy(&netstat.stdout);

    let needle = format!(":{port} ");
    for line in output.lines() {
        if !line.contains(&needle) || !line.contains("LISTENING") {
            continue;
        }
        let Some(pid_str) = line.split_whitespace().last() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let tasklist = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()?;
        let tasklist_out = String::from_utf8_lossy(&tasklist.stdout);
        if !tasklist_out
            .to_lowercase()
            .contains(&own_name.to_lowercase())
        {
            tracing::warn!(
                port,
                pid,
                "port is held by a process that isn't this binary — not killing it, bind will likely fail"
            );
            continue;
        }

        tracing::info!(port, pid, "reclaiming port from a previous crashed run");
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }

    Ok(())
}

#[cfg(unix)]
fn try_reclaim(port: u16) -> anyhow::Result<()> {
    let Some(own_name) = own_binary_name() else {
        return Ok(());
    };

    let lsof = Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output();

    let Ok(lsof) = lsof else {
        // `lsof` not installed is common enough (minimal containers)
        // that this shouldn't even warn — just skip the check.
        return Ok(());
    };

    let output = String::from_utf8_lossy(&lsof.stdout);
    for pid_str in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let comm_path = format!("/proc/{pid}/comm");
        let process_name = std::fs::read_to_string(&comm_path)
            .ok()
            .map(|s| s.trim().to_string());

        let matches_own_binary = match &process_name {
            Some(name) => own_name.starts_with(name.as_str()) || name.as_str() == own_name,
            // Couldn't read /proc/{pid}/comm (e.g. non-Linux Unix) —
            // fall back to `ps` rather than assume a match.
            None => {
                let ps = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "comm="])
                    .output();
                match ps {
                    Ok(out) => {
                        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        !name.is_empty() && (own_name.starts_with(&name) || name == own_name)
                    }
                    Err(_) => false,
                }
            }
        };

        if !matches_own_binary {
            tracing::warn!(
                port,
                pid,
                "port is held by a process that isn't this binary — not killing it, bind will likely fail"
            );
            continue;
        }

        tracing::info!(port, pid, "reclaiming port from a previous crashed run");
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }

    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn try_reclaim(_port: u16) -> anyhow::Result<()> {
    Ok(())
}
