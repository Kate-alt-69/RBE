use anyhow::{bail, Context, Result};
use rpx::publisher_commands;
use std::path::PathBuf;
use std::process::ExitCode;

mod legacy_cli {
    include!("legacy_main.rs");

    pub fn dispatch() -> ExitCode {
        main()
    }

    pub fn build_package_for_publish(
        target: PathBuf,
        allow_host_toolchain: bool,
    ) -> Result<(PathBuf, String, String)> {
        let package = check_package(&target)?;
        let name = package.manifest.package.name.clone();
        let version = package.manifest.package.version.clone();
        let root = package.root.clone();
        compile_package(target, allow_host_toolchain)?;
        let archive = root.join("dist").join(format!("{name}-{version}.rbe.zip"));
        if !archive.is_file() {
            bail!(
                "RPX package build completed without expected artifact {}",
                archive.display()
            );
        }
        Ok((archive, name, version))
    }
}

fn main() -> ExitCode {
    let startup_args = std::env::args().skip(1).collect::<Vec<_>>();
    let show_publisher_help = publisher_help_requested(&startup_args);
    match run() {
        Ok(Some(code)) => code,
        Ok(None) => {
            let code = legacy_cli::dispatch();
            if show_publisher_help {
                print_publisher_help();
            }
            code
        }
        Err(error) => {
            eprintln!("ERROR : RPX publisher issue!\n\n{error:#}\n");
            eprintln!("HINT : use `rpx login` before publishing, and configure the registry with --registry or RPX_REGISTRY_URL.");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<Option<ExitCode>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command_index) = command_index(&args) else {
        return Ok(None);
    };
    let command = args[command_index].as_str();
    if !matches!(command, "login" | "whoami" | "logout" | "publish") {
        return Ok(None);
    }

    let registry = option_value(&args, "--registry")?;
    let allow_host_toolchain = args.iter().any(|arg| arg == "--allow-host-toolchain");
    let positional = publisher_positionals(&args, command_index)?;

    match command {
        "login" => {
            if !positional.is_empty() {
                bail!("usage: rpx login [--registry <url>]");
            }
            publisher_commands::login(registry.as_deref())?;
        }
        "whoami" => {
            if !positional.is_empty() {
                bail!("usage: rpx whoami [--registry <url>]");
            }
            publisher_commands::whoami(registry.as_deref())?;
        }
        "logout" => {
            if !positional.is_empty() {
                bail!("usage: rpx logout [--registry <url>]");
            }
            publisher_commands::logout(registry.as_deref())?;
        }
        "publish" => {
            if positional.len() > 1 {
                bail!("usage: rpx publish [path] [--registry <url>] [--allow-host-toolchain]");
            }
            let target = positional
                .first()
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?);
            let (archive, package, version) =
                legacy_cli::build_package_for_publish(target, allow_host_toolchain)
                    .context("failed to build package for publication")?;
            publisher_commands::publish_archive(registry.as_deref(), &package, &version, &archive)?;
        }
        _ => unreachable!(),
    }
    Ok(Some(ExitCode::SUCCESS))
}

fn command_index(args: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--registry" {
            index = index.saturating_add(2);
            continue;
        }
        if arg.starts_with("--registry=") || arg == "--allow-host-toolchain" {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn option_value(args: &[String], option: &str) -> Result<Option<String>> {
    let prefix = format!("{option}=");
    let mut value = None;
    let mut index = 0;
    while index < args.len() {
        if let Some(inline) = args[index].strip_prefix(&prefix) {
            if inline.is_empty() {
                bail!("{option} requires a value");
            }
            if value.replace(inline.to_owned()).is_some() {
                bail!("{option} may only be supplied once");
            }
            index += 1;
            continue;
        }
        if args[index] == option {
            let candidate = args
                .get(index + 1)
                .filter(|candidate| !candidate.starts_with('-'))
                .with_context(|| format!("{option} requires a value"))?;
            if value.replace(candidate.clone()).is_some() {
                bail!("{option} may only be supplied once");
            }
            index += 2;
            continue;
        }
        index += 1;
    }
    Ok(value)
}

fn publisher_positionals(args: &[String], command_index: usize) -> Result<Vec<String>> {
    let mut output = Vec::new();
    let mut index = command_index + 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--registry" {
            if args.get(index + 1).is_none() {
                bail!("--registry requires a value");
            }
            index += 2;
            continue;
        }
        if arg.starts_with("--registry=") || arg == "--allow-host-toolchain" {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            bail!("unsupported publisher option {arg:?}");
        }
        output.push(arg.clone());
        index += 1;
    }
    Ok(output)
}

fn publisher_help_requested(args: &[String]) -> bool {
    args.is_empty()
        || matches!(args.first().map(String::as_str), Some("help" | "-h" | "--help"))
}

fn print_publisher_help() {
    println!(
        "\nPublisher commands:\n\
  rpx login [--registry <url>]\n\
      Authorize RPX in the browser and store a scoped registry credential.\n\
  rpx whoami [--registry <url>]\n\
      Validate the active credential and list packages owned by the publisher.\n\
  rpx logout [--registry <url>]\n\
      Revoke the active credential and remove locally stored auth state.\n\
  rpx publish [path] [--registry <url>] [--allow-host-toolchain]\n\
      Build a canonical .rbe.zip, upload it through the signed publisher flow, and publish an immutable release.\n\n\
Publisher environment:\n\
  RPX_TOKEN             Optional scoped 64-hex token for CI; never persisted.\n\
  RPX_AUTH_FILE         Override ~/.rbe/rpx/auth.json.\n\
  RPX_REGISTRY_URL      Default registry base URL."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn publisher_command_can_follow_global_registry_option() {
        let args = strings(&["--registry", "https://registry.example", "login"]);
        assert_eq!(command_index(&args), Some(2));
        assert_eq!(
            option_value(&args, "--registry").unwrap().as_deref(),
            Some("https://registry.example")
        );
    }

    #[test]
    fn publisher_registry_option_can_follow_command() {
        let args = strings(&["publish", ".", "--registry=https://registry.example"]);
        assert_eq!(command_index(&args), Some(0));
        assert_eq!(
            publisher_positionals(&args, 0).unwrap(),
            strings(&["."])
        );
        assert_eq!(
            option_value(&args, "--registry").unwrap().as_deref(),
            Some("https://registry.example")
        );
    }

    #[test]
    fn duplicate_registry_options_are_rejected() {
        let args = strings(&[
            "login",
            "--registry=https://one.example",
            "--registry",
            "https://two.example",
        ]);
        assert!(option_value(&args, "--registry").is_err());
    }

    #[test]
    fn publisher_help_is_added_to_root_help() {
        assert!(publisher_help_requested(&[]));
        assert!(publisher_help_requested(&strings(&["help"])));
        assert!(publisher_help_requested(&strings(&["--help"])));
        assert!(!publisher_help_requested(&strings(&["install"])));
    }
}
