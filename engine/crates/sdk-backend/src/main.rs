mod toolchain_install;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use toolchain_install::{
    install_verified_toolchain, project_toolchain_exists, verify_project_toolchain,
    PROJECT_TOOLCHAIN_FILE,
};

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
    #[serde(default)]
    managed_toolchain: bool,
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
        let toolchain = option(&args, "toolchain").map(PathBuf::from);
        install(
            Path::new(&path),
            resolve_version(version),
            &language,
            toolchain.as_deref(),
        )?;
        return Ok(());
    }

    if args.first().is_some_and(|arg| arg == "sdk") {
        let action = args.get(1).map(String::as_str).unwrap_or("status");
        let path = option(&args, "path").unwrap_or_else(|| ".".to_string());
        match action {
            "repair" | "update" => bootstrap_instruction(Path::new(&path), action)?,
            "status" => status(Path::new(&path))?,
            "toolchain" => {
                let file = option(&args, "file")
                    .context("`backend sdk toolchain` requires -file=<verified-rpx-toolchain.json>")?;
                install_toolchain(Path::new(&path), Path::new(&file))?;
            }
            other => bail!(
                "unknown SDK action {other:?}; expected status, repair, update, or toolchain"
            ),
        }
        return Ok(());
    }

    bail!("unknown SDK backend command; run with --help")
}

fn install(
    project: &Path,
    version: String,
    language: &str,
    toolchain_source: Option<&Path>,
) -> Result<()> {
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

    let toolchain_count = if let Some(source) = toolchain_source {
        Some(install_verified_toolchain(&project, source)?)
    } else if project_toolchain_exists(&project) {
        // Reinstalls preserve an already-admitted toolchain only after checking
        // that every currently installed compiler still matches its pin.
        Some(verify_project_toolchain(&project)?)
    } else {
        None
    };

    let lock = SdkLock {
        format: 2,
        version: version.clone(),
        language: language.to_string(),
        backend: format!("bin/{backend_name}"),
        rpx: format!("bin/{}", executable_name("rpx")),
        managed_toolchain: toolchain_count.is_some(),
    };
    write_lock(&project, &lock)?;

    println!("RBE SDK installed");
    println!("  project: {}", project.display());
    println!("  version: {version}");
    println!("  language: {language}");
    println!("  bindings: {}", languages.join(", "));
    println!("  RPX: {}", rpx_dest.display());
    match toolchain_count {
        Some(count) => println!(
            "  managed toolchain: VERIFIED ({count} pinned tool{})",
            if count == 1 { "" } else { "s" }
        ),
        None => println!(
            "  managed toolchain: NOT CONFIGURED (RPX compile remains fail-closed unless explicit local --allow-host-toolchain is used)"
        ),
    }
    println!("  scope: project-local only");
    Ok(())
}

fn install_toolchain(project: &Path, source: &Path) -> Result<()> {
    let project = absolute(project)?;
    let mut lock = load_lock(&project)?;
    let count = install_verified_toolchain(&project, source)?;
    lock.format = 2;
    lock.managed_toolchain = true;
    write_lock(&project, &lock)?;
    println!("RBE SDK managed toolchain installed");
    println!("  project: {}", project.display());
    println!("  file: .rbe/{PROJECT_TOOLCHAIN_FILE}");
    println!(
        "  tools: {count} pinned compiler{} verified",
        if count == 1 { "" } else { "s" }
    );
    println!("  host PATH fallback: disabled by default");
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
    let lock = load_lock(&project)?;
    if !matches!(lock.format, 1 | 2) {
        bail!("unsupported SDK lock format {}", lock.format);
    }
    let backend_ok = project.join(".rbe").join(&lock.backend).is_file();
    let rpx_ok = project.join(".rbe").join(&lock.rpx).is_file();
    let bindings = requested_languages(&lock.language);
    let missing_bindings = bindings
        .iter()
        .filter(|language| !project.join(".rbe").join("sdk").join(language).is_dir())
        .copied()
        .collect::<Vec<_>>();

    let toolchain_count = if lock.managed_toolchain {
        Some(verify_project_toolchain(&project)?)
    } else {
        None
    };

    println!("RBE SDK STATUS");
    println!("  project: {}", project.display());
    println!("  version: {}", lock.version);
    println!("  language: {}", lock.language);
    println!("  backend: {}", if backend_ok { "OK" } else { "MISSING" });
    println!("  rpx: {}", if rpx_ok { "OK" } else { "MISSING" });
    for language in &bindings {
        let present = !missing_bindings.contains(language);
        println!(
            "  sdk/{language}: {}",
            if present { "OK" } else { "MISSING" }
        );
    }
    match toolchain_count {
        Some(count) => println!(
            "  managed toolchain: VERIFIED ({count} pinned tool{})",
            if count == 1 { "" } else { "s" }
        ),
        None if project_toolchain_exists(&project) => println!(
            "  managed toolchain: UNTRACKED (descriptor exists but this SDK lock does not admit it)"
        ),
        None => println!("  managed toolchain: NOT CONFIGURED"),
    }
    if !backend_ok || !rpx_ok || !missing_bindings.is_empty() {
        bail!(
            "SDK installation is incomplete.\n{}",
            installer_hint(&project)
        );
    }
    Ok(())
}

fn load_lock(project: &Path) -> Result<SdkLock> {
    let lock_path = project.join(".rbe").join(LOCK_FILE);
    serde_json::from_slice(
        &fs::read(&lock_path)
            .with_context(|| format!("SDK lock not found: {}", lock_path.display()))?,
    )
    .with_context(|| format!("invalid SDK lock: {}", lock_path.display()))
}

fn write_lock(project: &Path, lock: &SdkLock) -> Result<()> {
    let path = project.join(".rbe").join(LOCK_FILE);
    fs::write(&path, serde_json::to_vec_pretty(lock)?)
        .with_context(|| format!("failed to write SDK lock: {}", path.display()))
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
            format!(
                "failed to replace old SDK binding at {}",
                destination.display()
            )
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
  backend install sdk.<version> -path=<project> [-language=typescript] [-toolchain=<verified-rpx-toolchain.json>]\n\n\
Status:\n\
  backend sdk status -path=<project>\n\n\
Managed compiler handoff:\n\
  backend sdk toolchain -path=<project> -file=<verified-rpx-toolchain.json>\n\n\
The toolchain descriptor must be RPX format 2 and every absolute compiler/entry path must still match its pinned SHA-256. The SDK backend never discovers host compilers through PATH.\n\n\
Update/repair:\n\
  Re-run the official Kastrick SDK installer so backend, RPX, language bindings, and managed compiler state are restored from a fresh verified bundle/handoff.\n\
  `backend sdk update -path=<project>` and `backend sdk repair -path=<project>` print that bootstrap command.\n\n\
The SDK backend never performs a machine-wide install."
    );
}
