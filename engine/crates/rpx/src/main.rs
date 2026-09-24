use anyhow::{bail, Result};
use sdk_package::{check_package, CheckedPackage, PACKAGE_MANIFEST};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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

    let command = args.remove(0);
    match command.as_str() {
        "check" => {
            let target = target_from(&args, 0)?;
            let package = check_package(target)?;
            print_check(&package);
        }
        "compile" => {
            if args.first().is_some_and(|value| value == "package") {
                let target = target_from(&args, 1)?;
                compile_package(target)?;
            } else {
                let target = target_from(&args, 0)?;
                compile(target)?;
            }
        }
        "compile.package" | "package" => {
            let target = target_from(&args, 0)?;
            compile_package(target)?;
        }
        "info" => {
            let target = target_from(&args, 0)?;
            let package = check_package(target)?;
            println!("{}", serde_json::to_string_pretty(&package.index())?);
        }
        unknown => bail!("unknown RPX command {unknown:?}"),
    }
    Ok(())
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

fn compile(target: PathBuf) -> Result<()> {
    let package = check_package(&target)?;
    let cache = package.root.join(".cache").join("rbe").join("build");
    fs::create_dir_all(&cache)?;
    let index_path = cache.join("package-index.json");
    fs::write(&index_path, serde_json::to_vec_pretty(&package.index())?)?;

    print_check(&package);
    println!("\nCOMPILE OK");
    println!("  index: {}", index_path.display());
    println!(
        "  note: component source syntax compilation is delegated to the installed {} SDK compiler",
        language_name(package.manifest.package.language)
    );
    Ok(())
}

fn compile_package(target: PathBuf) -> Result<()> {
    let package = check_package(&target)?;
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
    println!("  transitive RBE dependencies: private to this package graph");
    Ok(())
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
    println!("  components: {}", package.components.len());
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

fn language_name(language: sdk_package::PackageLanguage) -> &'static str {
    match language {
        sdk_package::PackageLanguage::Rust => "rust",
        sdk_package::PackageLanguage::Javascript => "javascript",
        sdk_package::PackageLanguage::Typescript => "typescript",
        sdk_package::PackageLanguage::Python => "python",
        sdk_package::PackageLanguage::Global => "global",
    }
}

fn render_error(error: &anyhow::Error) {
    eprintln!("ERROR : packaging issue!");
    eprintln!();
    eprintln!("{error:#}");
    eprintln!();
    eprintln!("HINT : run `rpx check .` from a directory containing {PACKAGE_MANIFEST} and make sure every components/<name>/ folder contains <name>.<language-extension>.");
}

fn print_help() {
    println!(
        "RPX — RBE package executor\n\n\
Usage:\n\
  rpx check [path]\n\
  rpx compile [path]\n\
  rpx compile.package [path]\n\
  rpx compile package [path]\n\
  rpx package [path]\n\
  rpx info [path]\n\n\
`path` defaults to the current directory. RPX walks upward until it finds package.rbe.toml.\n\
Package exports are discovered from components/<name>/<name>.<ext>."
    );
}
