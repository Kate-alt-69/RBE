use anyhow::{bail, Context, Result};
use rpx::compile_plan::{CompilerPlan, ResolvedCompilerPlan};
use rpx::compiler_execution::{
    bun_build, node_check, python_compile, rust_check, typescript_check, CompilerInvocation,
    CompilerProgram,
};
use rpx::toolchain::CompilerResolver;
use sdk_package::{
    check_package, check_target, CheckedComponent, CheckedPackage, JsRuntime, PackageLanguage,
    PACKAGE_MANIFEST,
};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use zip::write::SimpleFileOptions;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            render_error(&error);
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    let allow_host_toolchain = take_flag(&mut args, "--allow-host-toolchain");
    let command = args.remove(0);
    match command.as_str() {
        "check" => {
            let target = target_from(&args, 0)?;
            let package = check_target(target)?;
            print_check(&package);
        }
        "compile" => {
            if args.first().is_some_and(|value| value == "package") {
                let target = target_from(&args, 1)?;
                compile_package(target, allow_host_toolchain)?;
            } else {
                let target = target_from(&args, 0)?;
                compile(target, allow_host_toolchain)?;
            }
        }
        "compile.package" | "package" => {
            let target = target_from(&args, 0)?;
            compile_package(target, allow_host_toolchain)?;
        }
        "info" => {
            let target = target_from(&args, 0)?;
            let package = check_target(target)?;
            println!("{}", serde_json::to_string_pretty(&package.index())?);
        }
        unknown => bail!("unknown RPX command {unknown:?}"),
    }
    Ok(())
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let mut found = false;
    args.retain(|value| {
        if value == flag {
            found = true;
            false
        } else {
            true
        }
    });
    found
}

fn target_from(args: &[String], index: usize) -> Result<PathBuf> {
    if let Some(value) = args.get(index) {
        if value.starts_with('-') {
            bail!("expected a package path, found option {value:?}");
        }
        Ok(PathBuf::from(value))
    } else {
        Ok(std::env::current_dir()?)
    }
}

fn compile(target: PathBuf, allow_host_toolchain: bool) -> Result<()> {
    let package = check_target(&target)?;
    compile_sources(&package, allow_host_toolchain)?;

    let cache = package.root.join(".cache").join("rbe").join("build");
    fs::create_dir_all(&cache)?;

    let component_target = package.components.len() == 1 && is_component_target(&package, &target);
    let index_name = if component_target {
        format!("component-{}.json", package.components[0].name)
    } else {
        "package-index.json".to_string()
    };
    let index_path = cache.join(index_name);
    fs::write(&index_path, serde_json::to_vec_pretty(&package.index())?)?;

    print_check(&package);
    println!("\nCOMPILE OK");
    println!("  index: {}", index_path.display());
    if component_target {
        println!("  scope: component/{}", package.components[0].name);
    } else {
        println!("  scope: complete package");
    }
    println!("  compiler checks: passed");
    Ok(())
}

fn compile_package(target: PathBuf, allow_host_toolchain: bool) -> Result<()> {
    let package = check_package(&target)?;
    compile_sources(&package, allow_host_toolchain)?;

    let dist = package.root.join("dist");
    fs::create_dir_all(&dist)?;
    let archive_path = dist.join(format!(
        "{}-{}.rbe.zip",
        package.manifest.package.name, package.manifest.package.version
    ));

    let file = fs::File::create(&archive_path)?;
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    append_tree(&mut archive, &package.root, &package.root, options)?;
    archive.start_file(".rbe/package-index.json", options)?;
    archive.write_all(&serde_json::to_vec_pretty(&package.index())?)?;
    archive.finish()?;

    print_check(&package);
    println!("\nPACKAGE OK");
    println!("  artifact: {}", archive_path.display());
    println!("  scope: complete package");
    println!("  compiler checks: passed");
    println!("  transitive RBE dependencies: package-private graph");
    Ok(())
}

fn compile_sources(package: &CheckedPackage, allow_host_toolchain: bool) -> Result<()> {
    let build_root = package.root.join(".cache").join("rbe").join("compile");
    fs::create_dir_all(&build_root)?;

    let resolver = CompilerResolver::from_project(&package.root, allow_host_toolchain)
        .context("failed to resolve RPX compiler toolchain")?;
    println!(
        "  compiler authority: {}",
        if resolver.is_managed() {
            "RBE-managed"
        } else {
            "explicit host authoring"
        }
    );

    for component in &package.components {
        println!(
            "  compiler: {} ({})",
            component.name,
            language_name(component.language)
        );
        let plan = CompilerPlan::for_component(
            component.language,
            package.manifest.package.runtime,
        )
        .with_context(|| format!("could not plan compiler for component {:?}", component.name))?
        .resolve(&resolver)
        .with_context(|| {
            format!(
                "could not resolve compiler tools for component {:?}",
                component.name
            )
        })?;

        match component.language {
            PackageLanguage::Rust => compile_rust(package, component, &build_root, &plan)?,
            PackageLanguage::Javascript => {
                compile_javascript(package, component, &build_root, &plan)?
            }
            PackageLanguage::Typescript => {
                compile_typescript(package, component, &build_root, &plan)?
            }
            PackageLanguage::Python => compile_python(component, &build_root, &plan)?,
            PackageLanguage::Global => {
                bail!(
                    "component {:?} resolved to global instead of a concrete SDK language",
                    component.name
                )
            }
        }
    }
    Ok(())
}

fn compile_rust(
    package: &CheckedPackage,
    component: &CheckedComponent,
    build_root: &Path,
    plan: &ResolvedCompilerPlan,
) -> Result<()> {
    let sdk = package.root.join(".rbe").join("sdk").join("rust");
    require_sdk_binding(&sdk, "rust")?;

    let work = build_root.join("rust").join(&component.name);
    let src = work.join("src");
    recreate_dir(&work)?;
    fs::create_dir_all(&src)?;
    let component_dir = component
        .source
        .parent()
        .context("Rust component source has no parent directory")?;
    copy_source_tree(component_dir, &src)?;
    fs::copy(&component.source, src.join("lib.rs"))?;

    let crate_name = format!("rbe_component_check_{}", component.name.replace('-', "_"));
    let sdk_path = toml_path(&sdk.canonicalize()?);
    let manifest = format!(
        "[package]\nname = \"{crate_name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nrbe-sdk = {{ path = \"{sdk_path}\" }}\n"
    );
    fs::write(work.join("Cargo.toml"), manifest)?;

    let manifest_path = work.join("Cargo.toml");
    let invocation = rust_check(plan, &manifest_path)?;
    execute_invocation(invocation, component)?;
    Ok(())
}

fn compile_javascript(
    package: &CheckedPackage,
    component: &CheckedComponent,
    build_root: &Path,
    plan: &ResolvedCompilerPlan,
) -> Result<()> {
    match package.manifest.package.runtime {
        Some(JsRuntime::Node) => {
            // RBE JavaScript components are modules. Stage the exact bytes as
            // .mjs so Node performs a real ES-module syntax check without
            // executing package code.
            let out = build_root.join("javascript").join(&component.name);
            recreate_dir(&out)?;
            let staged = out.join(format!("{}.mjs", component.name));
            fs::copy(&component.source, &staged)?;
            execute_invocation(node_check(plan, &staged)?, component)?;
        }
        Some(JsRuntime::Bun) => {
            let out = build_root.join("javascript").join(&component.name);
            recreate_dir(&out)?;
            execute_invocation(bun_build(plan, &component.source, &out)?, component)?;
        }
        None => bail!(
            "JavaScript component {:?} has no Node/Bun runtime in package.rbe.toml",
            component.name
        ),
    }
    Ok(())
}

fn compile_typescript(
    package: &CheckedPackage,
    component: &CheckedComponent,
    build_root: &Path,
    plan: &ResolvedCompilerPlan,
) -> Result<()> {
    let sdk = package.root.join(".rbe").join("sdk").join("typescript");
    require_sdk_binding(&sdk, "typescript")?;

    let work = build_root.join("typescript").join(&component.name);
    recreate_dir(&work)?;
    let source = slash_path(&component.source.canonicalize()?);
    let sdk_types = slash_path(&sdk.canonicalize()?.join("index.d.ts"));
    let config = serde_json::json!({
        "compilerOptions": {
            "noEmit": true,
            "target": "ES2022",
            "module": "ESNext",
            "moduleResolution": "Bundler",
            "strict": true,
            "skipLibCheck": true,
            "baseUrl": slash_path(&package.root.canonicalize()?),
            "paths": {
                "@rbe/sdk": [sdk_types]
            }
        },
        "files": [source]
    });
    let config_path = work.join("tsconfig.rbe.json");
    fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;

    execute_invocation(typescript_check(plan, &config_path)?, component)?;
    Ok(())
}

fn compile_python(
    component: &CheckedComponent,
    build_root: &Path,
    plan: &ResolvedCompilerPlan,
) -> Result<()> {
    let pycache = build_root.join("python").join("pycache");
    fs::create_dir_all(&pycache)?;
    execute_invocation(python_compile(plan, &component.source, &pycache)?, component)?;
    Ok(())
}

fn execute_invocation(
    invocation: CompilerInvocation,
    component: &CheckedComponent,
) -> Result<()> {
    if invocation.network_allowed {
        bail!("RPX refused compiler invocation with network authority");
    }
    if invocation.use_shell {
        bail!("RPX refused compiler invocation with shell authority");
    }

    let (mut command, program_name) = match &invocation.program {
        CompilerProgram::Managed(path) => (
            Command::new(path),
            path.to_string_lossy().into_owned(),
        ),
        CompilerProgram::HostAuthoring(name) => (Command::new(name), name.clone()),
    };
    command
        .current_dir(&invocation.working_directory)
        .args(&invocation.args);
    if invocation.clear_environment {
        command.env_clear();
        if cfg!(windows) {
            for key in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        } else if let Some(value) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", value);
        }
    }
    command.envs(&invocation.environment);

    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "compiler not found for component {:?}: resolved tool `{}` could not be launched",
                component.name,
                program_name
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to launch {program_name}"))
        }
    };
    ensure_success(&program_name, component, output)
}

fn ensure_success(program: &str, component: &CheckedComponent, output: Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{} compiler rejected component {:?} at {}\n{}{}",
        program,
        component.name,
        component.source.display(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!("\nstdout:\n{}", stdout.trim())
        },
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("\nstderr:\n{}", stderr.trim())
        }
    )
}

fn require_sdk_binding(path: &Path, language: &str) -> Result<()> {
    if path.is_dir() {
        Ok(())
    } else {
        bail!(
            "RBE {language} SDK binding is not installed at {}\nHINT: re-run the project-local RBE SDK installer with -Language {language} (or global).",
            path.display()
        )
    }
}

fn recreate_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn copy_source_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_source_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_component_target(package: &CheckedPackage, target: &Path) -> bool {
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(target),
            Err(_) => return false,
        }
    };
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    let component_root = package.root.join(&package.manifest.components.root);
    let Ok(component_root) = component_root.canonicalize() else {
        return false;
    };
    target
        .strip_prefix(component_root)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some()
}

fn append_tree(
    archive: &mut zip::ZipWriter<fs::File>,
    root: &Path,
    directory: &Path,
    options: SimpleFileOptions,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if should_skip(relative) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            append_tree(archive, root, &path, options)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        archive.start_file(name, options)?;
        let mut input = fs::File::open(&path)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            archive.write_all(&buffer[..read])?;
        }
    }
    Ok(())
}

fn should_skip(relative: &Path) -> bool {
    relative.components().next().is_some_and(|part| {
        matches!(
            part.as_os_str().to_string_lossy().as_ref(),
            ".git" | ".cache" | ".rbe" | "target" | "dist"
        )
    })
}

fn print_check(package: &CheckedPackage) {
    println!("RBE PACKAGE CHECK");
    println!(
        "  package: {}@{}",
        package.manifest.package.name, package.manifest.package.version
    );
    println!("  manifest: {}", package.manifest_path.display());
    println!(
        "  language: {}",
        language_name(package.manifest.package.language)
    );
    println!("  components checked: {}", package.components.len());
    for component in &package.components {
        println!("    ✓ {} -> {}", component.name, component.source.display());
    }
    if package.manifest.dependencies.rbe.is_empty() {
        println!("  RBE dependencies: none");
    } else {
        println!("  RBE dependencies (package-private):");
        for (name, version) in &package.manifest.dependencies.rbe {
            println!("    ✓ {name} {version}");
        }
    }
    println!("  export surface: components/* only");
}

fn language_name(language: PackageLanguage) -> &'static str {
    match language {
        PackageLanguage::Rust => "rust",
        PackageLanguage::Javascript => "javascript",
        PackageLanguage::Typescript => "typescript",
        PackageLanguage::Python => "python",
        PackageLanguage::Global => "global",
    }
}

fn render_error(error: &anyhow::Error) {
    eprintln!("ERROR : packaging issue!");
    eprintln!();
    eprintln!("{error:#}");
    eprintln!();
    eprintln!("HINT : run `rpx check .` for package structure or `rpx compile .` for real compiler checks. Managed compilation reads .rbe/rpx-toolchain.json; use --allow-host-toolchain only for explicit local authoring. Use `rpx compile components/<name>` to compile only one exported component. Every exported component folder must contain <name>.<language-extension>, and the package root must contain {PACKAGE_MANIFEST}.");
}

fn print_help() {
    println!(
        "RPX — RBE package executor\n\n\
Usage:\n\
  rpx check [path]\n\
  rpx compile [path] [--allow-host-toolchain]\n\
  rpx compile.package [path] [--allow-host-toolchain]\n\
  rpx compile package [path] [--allow-host-toolchain]\n\
  rpx package [path] [--allow-host-toolchain]\n\
  rpx info [path]\n\n\
`path` defaults to the current directory. RPX walks upward until it finds package.rbe.toml.\n\
A path inside components/<name>/ checks/compiles only that component.\n\
`check` validates RBE package/component structure. `compile` additionally invokes the selected language compiler/toolchain.\n\
Compiler execution is RBE-managed by default through .rbe/rpx-toolchain.json. --allow-host-toolchain is an explicit local-authoring escape hatch and is never an automatic fallback from a partial managed toolchain.\n\
Package exports are discovered from components/<name>/<name>.<ext>."
    );
}
