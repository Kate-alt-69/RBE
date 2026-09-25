//! Real pre-bootstrap `backend install` resolution path.
//!
//! The executable intentionally stops after trusted registry hydration and
//! deterministic resolution until verified artifact inspection can supply the
//! manifest hash required by `ProjectPackageLock`. It must never report an
//! install as successful before cache promotion/session activation completes.

use std::process::ExitCode;

use rbe_install_request::{InstallCommand, InstallTarget};
use rbe_install_runtime::RegistryClient;
use rbe_library_resolver::{resolve_scoped, ResolutionRequest};

const REGISTRY_ENV: &str = "RBE_PACKAGE_REGISTRY";

const HELP: &str = r#"RBE package installer

Usage:
  backend install <target> [options]
  backend install help

Named package examples:
  advancenet
  advancenet.4.0.1

Options:
  -version <version> | --version <version>
  -shared
  -force
  -no-cache
  -refresh-index
  -json
  -quiet

Registry configuration:
  RBE_PACKAGE_REGISTRY=https://<trusted-registry-base>/

Current execution boundary:
  Named-package registry hydration and deterministic dependency resolution are
  active. Verified artifact ingress exists in rbe-install-runtime. Final
  artifact inspection, durable cache promotion, install-session activation,
  and package.lock.rbe.yaml commit are not yet connected to backend.exe, so a
  successfully resolved package exits unavailable instead of claiming success."#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCliFailure {
    pub code: u8,
    pub message: String,
}

impl InstallCliFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: format!("error: {}\n\n{}", message.into(), usage()),
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self {
            code: 78,
            message: format!("error: {}", message.into()),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: 69,
            message: format!("error: {}", message.into()),
        }
    }

    fn software(message: impl Into<String>) -> Self {
        Self {
            code: 70,
            message: format!("error: {}", message.into()),
        }
    }
}

/// Intercept only a real `install` public subcommand. `help install`, internal
/// modes, and unrelated boot arguments continue through the existing router.
pub fn requested(args: &[String]) -> Option<Result<String, InstallCliFailure>> {
    let index = install_command_index(args)?;
    let command_args = &args[index + 1..];

    if command_args.first().is_some_and(|arg| arg == "help")
        || command_args.iter().any(|arg| is_help(arg))
    {
        return Some(Ok(HELP.to_string()));
    }

    let refs = command_args.iter().map(String::as_str).collect::<Vec<_>>();
    let command = match InstallCommand::parse(&refs) {
        Ok(command) => command,
        Err(error) => return Some(Err(InstallCliFailure::usage(error.to_string()))),
    };

    match &command.target {
        InstallTarget::Named { key, .. } if key != "sdk" && !key.starts_with("runtime.") => {
            Some(resolve_named(command))
        }
        // SDK/runtime and external/local archive execution remain owned by
        // their existing lanes. Let the legacy dispatcher preserve its current
        // explicit unavailable behavior rather than stealing those commands.
        InstallTarget::Named { .. }
        | InstallTarget::External { .. }
        | InstallTarget::LocalArchive(_) => None,
    }
}

pub fn exit_for(result: Result<String, InstallCliFailure>) -> ExitCode {
    match result {
        Ok(rendered) => {
            println!("{rendered}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn resolve_named(command: InstallCommand) -> Result<String, InstallCliFailure> {
    let InstallTarget::Named { key, version } = command.target else {
        return Err(InstallCliFailure::software(
            "internal installer dispatch selected a non-named target",
        ));
    };
    let requirement = version
        .map(|version| version.requirement)
        .unwrap_or_else(|| "*".to_string());
    let registry = std::env::var(REGISTRY_ENV).map_err(|_| {
        InstallCliFailure::config(format!(
            "named package installation requires {REGISTRY_ENV}=https://<registry-base>/"
        ))
    })?;
    if registry.trim().is_empty() {
        return Err(InstallCliFailure::config(format!(
            "{REGISTRY_ENV} must not be empty"
        )));
    }

    let json = command.flags.json;
    let quiet = command.flags.quiet;
    let key_for_worker = key.clone();
    let requirement_for_worker = requirement.clone();
    let registry_for_worker = registry.clone();

    let worker = std::thread::Builder::new()
        .name("rbe-package-install-resolver".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    InstallCliFailure::software(format!(
                        "initialize package install network runtime: {error}"
                    ))
                })?;
            runtime.block_on(resolve_named_async(
                &registry_for_worker,
                &key_for_worker,
                &requirement_for_worker,
                json,
                quiet,
            ))
        })
        .map_err(|error| {
            InstallCliFailure::software(format!("start package install worker: {error}"))
        })?;

    worker.join().map_err(|_| {
        InstallCliFailure::software("package install worker terminated unexpectedly")
    })?
}

async fn resolve_named_async(
    registry: &str,
    key: &str,
    requirement: &str,
    json: bool,
    quiet: bool,
) -> Result<String, InstallCliFailure> {
    let client = RegistryClient::new(registry)
        .map_err(|error| InstallCliFailure::config(format!("invalid {REGISTRY_ENV}: {error}")))?;
    let (catalog, indexes) = client.hydrate_catalog(key).await.map_err(|error| {
        InstallCliFailure::unavailable(format!(
            "package registry resolution for `{key}` failed: {error}"
        ))
    })?;
    let request = ResolutionRequest::new(key, requirement).map_err(|error| {
        InstallCliFailure::usage(format!("invalid package requirement for `{key}`: {error}"))
    })?;
    let scoped = resolve_scoped(&catalog, [request]).map_err(|error| {
        InstallCliFailure::unavailable(format!("could not resolve `{key}`: {error}"))
    })?;
    let resolution = scoped.root(key).ok_or_else(|| {
        InstallCliFailure::software(format!(
            "resolver returned no root graph for requested package `{key}`"
        ))
    })?;
    let root = resolution.release(key).ok_or_else(|| {
        InstallCliFailure::software(format!(
            "resolver returned no selected root release for `{key}`"
        ))
    })?;
    let selected_version = root.version.to_string();
    let index = indexes.get(key).ok_or_else(|| {
        InstallCliFailure::software(format!(
            "registry hydration returned no root index for `{key}`"
        ))
    })?;
    let metadata = index
        .releases
        .iter()
        .find(|release| release.version == selected_version)
        .ok_or_else(|| {
            InstallCliFailure::software(format!(
                "resolved `{key}` {selected_version} is missing from the validated registry index"
            ))
        })?;
    let dependencies = resolution.selected.len().saturating_sub(1);

    let message = if json {
        serde_json::json!({
            "status": "resolved_not_activated",
            "package": key,
            "version": selected_version,
            "dependencies": dependencies,
            "artifact": {
                "source": metadata.artifact.source,
                "sha256": metadata.artifact.sha256,
                "size_bytes": metadata.artifact.size_bytes,
            },
            "registry": registry,
            "server_started": false,
            "next_boundary": "verified artifact inspection and durable activation"
        })
        .to_string()
    } else if quiet {
        format!(
            "`{key}` {selected_version} resolved, but durable package activation is not connected yet"
        )
    } else {
        format!(
            "`backend install` resolved `{key}` {selected_version} with {dependencies} dependenc{} from the configured registry.\nartifact: {}\nsha256: {}\nsize: {} bytes\n\nVerified artifact ingress is available, but artifact manifest inspection and durable cache/session activation are not connected to backend.exe yet. No RBE server was started and no project lockfile was changed.",
            if dependencies == 1 { "y" } else { "ies" },
            metadata.artifact.source,
            metadata.artifact.sha256,
            metadata.artifact.size_bytes,
        )
    };

    Err(InstallCliFailure::unavailable(message))
}

fn install_command_index(args: &[String]) -> Option<usize> {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        if is_help(arg) || is_internal_mode(arg) {
            return None;
        }
        if !arg.starts_with('-') {
            return (arg == "install").then_some(index);
        }
        if arg == "--settings" {
            if args.get(index + 1).is_none() {
                return None;
            }
            index += 2;
            continue;
        }
        if arg.starts_with("--settings=")
            || matches!(arg, "--allow-settings-env" | "--debug" | "--debug-boot")
            || arg.starts_with("--debug-boot=")
            || arg.starts_with("-debug=")
        {
            index += 1;
            continue;
        }
        if arg == "-debug" {
            if args
                .get(index + 1)
                .is_some_and(|value| is_boolean_literal(value))
            {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        return None;
    }
    None
}

fn is_help(value: &str) -> bool {
    matches!(value, "-h" | "--help" | "-help")
}

fn is_internal_mode(value: &str) -> bool {
    matches!(
        value,
        "--maintenance-notice" | "--er" | "--vault" | "--explain" | "--list-error-codes"
    ) || value.starts_with("--explain=")
}

fn is_boolean_literal(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "0" | "true" | "false" | "yes" | "no" | "on" | "off"
    )
}

fn usage() -> &'static str {
    "Usage: backend install <target> [options]\nRun `backend install help` for details."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn scanner_finds_install_after_known_boot_flags() {
        assert_eq!(
            install_command_index(&args(&[
                "--settings",
                "settings.json",
                "-debug",
                "install",
                "advancenet"
            ])),
            Some(3)
        );
    }

    #[test]
    fn help_and_other_commands_are_not_stolen() {
        assert_eq!(install_command_index(&args(&["help", "install"])), None);
        assert_eq!(install_command_index(&args(&["check"])), None);
        assert_eq!(install_command_index(&args(&["--er", "install"])), None);
    }

    #[test]
    fn malformed_install_uses_canonical_request_parser() {
        let result = requested(&args(&["install", "demo", "--wat"]))
            .expect("install must be intercepted")
            .expect_err("unknown flag must fail");
        assert_eq!(result.code, 2);
        assert!(result.message.contains("unknown install flag"));
    }

    #[test]
    fn reserved_and_external_targets_remain_outside_this_lane() {
        assert!(requested(&args(&["install", "sdk.0.1.0"])).is_none());
        assert!(requested(&args(&["install", "runtime.python.3.10"])).is_none());
        assert!(requested(&args(&["install", "https://example.com/package.rbe-pkg"])).is_none());
    }
}
