use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const LOCK_FILE: &str = "sdk.lock.json";
const POWERSHELL_INSTALLER: &str = "https://kastrick-backend.onrender.com/api/sdk/install.ps1";
const SHELL_INSTALLER: &str = "https://kastrick-backend.onrender.com/api/sdk/install.sh";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SdkLock {
    format: u32,
    version: String,
    language: String,
    backend: String,
    rpx: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("RBE SDK bootstrap failed:\n{error:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        help();
        return Ok(());
    }

    if args.first().is_some_and(|arg| arg == "install") {
        let sdk = args
            .get(1)
            .context("expected `sdk.<version>` after install")?;
        let version = sdk
            .strip_prefix("sdk.")
            .context("SDK installs use `backend install sdk.<version>`")?;
        let path = option(&args, "path").unwrap_or_else(|| ".".to_string());
        let language = option(&args, "language").unwrap_or_else(|| "global".to_string());
        install(Path::new(&path), resolve_version(version), &language)?;
        return Ok(());
    }

    if args.first().is_some_and(|arg| arg == "sdk") {
        let action = args.get(1).map(String::as_str).unwrap_or("status");
        let path = option(&args, "path").unwrap_or_else(|| ".".to_string());
        match action {
            "repair" | "update" => bootstrap_instruction(Path::new(&path), action)?,
            "status" => status(Path::new(&path))?,
            other => bail!("unknown SDK action {other:?}; expected status, repair, or update"),
        }
        return Ok(());
    }

    bail!("unknown SDK backend command; run with --help")
}

fn install(project: &Path, version: String, language: &str) -> Result<()> {
    validate_language(language)?;
    let project = absolute(project)?;
    fs::create_dir_all(&project)?;
    let rbe = project.join(".rbe");
    let bin = rbe.join("bin");
    let sdk = rbe.join("sdk");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&sdk)?;
    fs::create_dir_all(project.join(".cache").join("library"))?;
    fs::create_dir_all(project.join("components"))?;

    let current = std::env::current_exe()?;
    let bundle_root = current.parent().unwrap_or(Path::new("."));
    let backend_name = executable_name("backend");
    let backend_dest = bin.join(&backend_name);
    copy_if_different(&current, &backend_dest)?;

    let rpx_source = bundle_root.join(executable_name("rpx"));
    if !rpx_source.is_file() {
        bail!(
            "RPX is missing beside the SDK backend at {}. A complete SDK bundle must ship backend and rpx together.\n{}",
            rpx_source.display(),
            installer_hint(&project)
        );
    }
    let rpx_dest = bin.join(executable_name("rpx"));
    copy_if_different(&rpx_source, &rpx_dest)?;

    let binding_root = bundle_root.join("bindings");
    let languages = requested_languages(language);
    for item in &languages {
        let source = binding_root.join(item);
        if !source.is_dir() {
            bail!(
                "SDK binding payload {item:?} is missing from {}. A complete SDK bundle must contain bindings/rust, bindings/javascript, bindings/typescript, and bindings/python.\n{}",
                binding_root.display(),
                installer_hint(&project)
            );
        }
        let destination = sdk.join(item);
        replace_tree(&source, &destination)?;
        let manifest = serde_json::json!({
            "format": 1,
            "sdk_version": version,
            "language": item,
            "package_manifest": "package.rbe.toml",
            "components_root": "components",
            "dependency_scope": "package-private",
            "binding_root": format!("sdk/{item}")
        });
        fs::write(
            destination.join("sdk.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
    }

    let lock = SdkLock {
        format: 1,
        version: version.clone(),
        language: language.to_string(),
        backend: format!("bin/{backend_name}"),
        rpx: format!("bin/{}", executable_name("rpx")),
    };
    fs::write(rbe.join(LOCK_FILE), serde_json::to_vec_pretty(&lock)?)?;

    println!("RBE SDK installed");
    println!("  project: {}", project.display());
    println!("  version: {version}");
    println!("  language: {language}");
    println!("  bindings: {}", languages.join(", "));
    println!("  RPX: {}", rpx_dest.display());
    println!("  scope: project-local only");
    Ok(())
}

fn bootstrap_instruction(project: &Path, action: &str) -> Result<()> {
    let project = absolute(project)?;
    bail!(
        "SDK {action} requires a fresh verified bootstrap bundle. The project-local backend intentionally does not keep a second complete SDK payload or replace itself in place.\n{}",
        installer_hint(&project)
    )
}

fn status(project: &Path) -> Result<()> {
    let project = absolute(project)?;
    let lock_path = project.join(".rbe").join(LOCK_FILE);
    let lock: SdkLock = serde_json::from_slice(
        &fs::read(&lock_path)
            .with_context(|| format!("SDK lock not found: {}", lock_path.display()))?,
    )?;
    let backend_ok = project.join(".rbe").join(&lock.backend).is_file();
    let rpx_ok = project.join(".rbe").join(&lock.rpx).is_file();
    let bindings = requested_languages(&lock.language);
    let missing_bindings = bindings
        .iter()
        .filter(|language| !project.join(".rbe").join("sdk").join(language).is_dir())
        .copied()
        .collect::<Vec<_>>();

    println!("RBE SDK STATUS");
    println!("  project: {}", project.display());
    println!("  version: {}", lock.version);
    println!("  language: {}", lock.language);
    println!("  backend: {}", if backend_ok { "OK" } else { "MISSING" });
    println!("  rpx: {}", if rpx_ok { "OK" } else { "MISSING" });
    for language in &bindings {
        let present = !missing_bindings.contains(language);
        println!("  sdk/{language}: {}", if present { "OK" } else { "MISSING" });
    }
    if !backend_ok || !rpx_ok || !missing_bindings.is_empty() {
        bail!("SDK installation is incomplete.\n{}", installer_hint(&project));
    }
    Ok(())
}

fn requested_languages(language: &str) -> Vec<&str> {
    if language == "global" {
        vec!["rust", "javascript", "typescript", "python"]
    } else {
        vec![language]
    }
}

fn installer_hint(project: &Path) -> String {
    if cfg!(windows) {
        format!(
            "Update/repair command (PowerShell):\n  $p = Join-Path $env:TEMP 'rbe-sdk-install.ps1'; iwr {POWERSHELL_INSTALLER} -OutFile $p; & $p -Path '{}'",
            project.display()
        )
    } else {
        format!(
            "Update/repair command:\n  curl -fsSL {SHELL_INSTALLER} -o /tmp/rbe-sdk-install.sh && sh /tmp/rbe-sdk-install.sh --path '{}'",
            project.display()
        )
    }
}

fn option(args: &[String], name: &str) -> Option<String> {
    let single = format!("-{name}=");
    let double = format!("--{name}=");
    args.iter().find_map(|argument| {
        argument
            .strip_prefix(&single)
            .or_else(|| argument.strip_prefix(&double))
            .map(ToOwned::to_owned)
    })
}

fn resolve_version(value: &str) -> String {
    if value.eq_ignore_ascii_case("latest") {
        option_env!("RBE_SDK_VERSION")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_string()
    } else {
        value.to_string()
    }
}

fn validate_language(value: &str) -> Result<()> {
    match value {
        "rust" | "javascript" | "typescript" | "python" | "global" => Ok(()),
        other => bail!(
            "unsupported SDK language {other:?}; expected rust, javascript, typescript, python, or global"
        ),
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn copy_if_different(source: &Path, destination: &Path) -> Result<()> {
    let same = source
        .canonicalize()
        .ok()
        .zip(destination.canonicalize().ok())
        .is_some_and(|(left, right)| left == right);
    if !same {
        fs::copy(source, destination).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn replace_tree(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination).with_context(|| {
            format!("failed to replace old SDK binding at {}", destination.display())
        })?;
    }
    copy_tree(source, destination)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy SDK binding {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

fn help() {
    println!(
        "RBE SDK backend (project-local)\n\n\
Install from a freshly verified complete SDK bundle:\n\
  backend install sdk.<version> -path=<project> [-language=typescript]\n\n\
Status:\n\
  backend sdk status -path=<project>\n\n\
Update/repair:\n\
  Re-run the official Kastrick SDK installer so backend, RPX, and language bindings are restored from a fresh verified bundle.\n\
  `backend sdk update -path=<project>` and `backend sdk repair -path=<project>` print that bootstrap command.\n\n\
The SDK backend never performs a machine-wide install."
    );
}
