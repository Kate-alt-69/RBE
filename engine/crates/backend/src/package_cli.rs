use std::path::Path;

use anyhow::{bail, Context};
use rbe_install_runtime::{read_verified_rpx_root_snapshots, VerifiedRpxRootSnapshot};

use crate::runtime_image_boot::package_links::approval;

const HELP: &str = r#"RBE package permissions

Usage:
  backend package permissions <package>
  backend package approve <package> [capability ...]
  backend package help

Examples:
  backend package permissions mail
  backend package approve mail net:http net:dns
  backend package approve mail

`approve` replaces the explicit RBE host privileges for the exact installed
package artifact. Passing no capabilities revokes all explicit privileges.
Package-scoped logging is implicit and does not require approval."#;

pub fn requested(args: &[String]) -> Option<anyhow::Result<String>> {
    let index = args.iter().position(|arg| arg == "package")?;
    let command = args.get(index + 1).map(String::as_str).unwrap_or("help");
    if matches!(command, "help" | "-h" | "--help") {
        return Some(Ok(HELP.to_string()));
    }

    let result = match command {
        "permissions" => permissions(&args[index + 2..]),
        "approve" => approve(&args[index + 2..]),
        other => Err(anyhow::anyhow!(
            "unknown package command {other:?}; expected permissions, approve, or help\n\n{HELP}"
        )),
    };
    Some(result)
}

fn permissions(args: &[String]) -> anyhow::Result<String> {
    if args.len() != 1 {
        bail!("package permissions requires exactly one installed package name\n\n{HELP}");
    }
    let project_root = std::env::current_dir().context("resolve package project directory")?;
    let snapshot = installed_snapshot(&project_root, &args[0])?;
    let requested = approval::requested_runtime_capabilities(&project_root, &snapshot)?;
    let approved = approval::approved_runtime_capabilities(&project_root, &snapshot)?;

    let mut output = String::new();
    output.push_str(&format!(
        "RBE PACKAGE PERMISSIONS\n  package: {} {}\n  artifact: {}\n  implicit: log (lib/{})\n",
        snapshot.package, snapshot.version, snapshot.artifact_sha256, snapshot.package
    ));

    let mut explicit = 0usize;
    for capability in &requested {
        if let Some(description) = approval::explicit_host_privilege_description(capability) {
            explicit += 1;
            let state = if approved.iter().any(|item| item == capability) {
                "APPROVED"
            } else {
                "NOT APPROVED"
            };
            output.push_str(&format!("  {capability}: {state}\n    {description}\n"));
        }
    }
    if explicit == 0 {
        output.push_str("  explicit host privileges: none requested\n");
    }

    let package_private = requested
        .iter()
        .filter(|capability| {
            *capability != "log"
                && approval::explicit_host_privilege_description(capability).is_none()
        })
        .collect::<Vec<_>>();
    if !package_private.is_empty() {
        output.push_str("  package-private/unimplemented requests:\n");
        for capability in package_private {
            output.push_str(&format!("    {capability}\n"));
        }
    }
    Ok(output.trim_end().to_string())
}

fn approve(args: &[String]) -> anyhow::Result<String> {
    let Some(package) = args.first() else {
        bail!("package approve requires an installed package name\n\n{HELP}");
    };
    let project_root = std::env::current_dir().context("resolve package project directory")?;
    let snapshot = installed_snapshot(&project_root, package)?;
    let approved = approval::replace_runtime_approval(&project_root, &snapshot, &args[1..])?;

    if approved.is_empty() {
        Ok(format!(
            "Revoked all explicit RBE host privileges for `{}` {}. Package-scoped `log` remains implicit.",
            snapshot.package, snapshot.version
        ))
    } else {
        Ok(format!(
            "Approved explicit RBE host privileges for `{}` {}: {}\nApproval is bound to artifact {} and the current project lock.",
            snapshot.package,
            snapshot.version,
            approved.join(", "),
            snapshot.artifact_sha256
        ))
    }
}

fn installed_snapshot(
    project_root: &Path,
    package: &str,
) -> anyhow::Result<VerifiedRpxRootSnapshot> {
    let snapshots = read_verified_rpx_root_snapshots(project_root)
        .context("load verified installed package roots")?;
    snapshots
        .into_iter()
        .find(|snapshot| snapshot.package == package)
        .ok_or_else(|| {
            anyhow::anyhow!("package {package:?} is not an installed explicit project root")
        })
}
