use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use atomic_io::AtomicIo;
use rbe_install_runtime::{
    read_verified_package_services, read_verified_rpx_root_snapshots, VerifiedPackageServiceSource,
    PACKAGE_SERVICE_CAPABILITY,
};
use service_runtime::ServiceCatalog;

const PACKAGE_LOCK: &str = "package.lock.rbe.yaml";
const SERVICE_CATALOG_STATE_DIR: &str = "service-catalog";

#[derive(Debug)]
pub struct StagedServiceCatalog {
    root: PathBuf,
    package_roots: Vec<(String, PathBuf)>,
    package_service_count: usize,
}

impl StagedServiceCatalog {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn package_service_count(&self) -> usize {
        self.package_service_count
    }

    pub fn validate_namespaces(&self, catalog: &ServiceCatalog) -> Result<(), String> {
        let mut diagnostics = Vec::new();
        for (package, root) in &self.package_roots {
            if !valid_service_namespace(package) {
                diagnostics.push(format!(
                    "package {package:?} cannot own .service programs because its package name is not a valid Service namespace; expected only ASCII letters, digits, '-', '_' or '.'"
                ));
                continue;
            }
            let prefix = format!("{package}.");
            for service in catalog.services() {
                if !service.path.starts_with(root) {
                    continue;
                }
                if service.name.as_str() != package.as_str() && !service.name.starts_with(&prefix) {
                    diagnostics.push(format!(
                        "{}:1:1 package-owned service {:?} must stay in package namespace {:?}; use name = {} or name = {}<service>",
                        service.path.display(),
                        service.name,
                        package,
                        package,
                        prefix,
                    ));
                }
            }
        }
        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(diagnostics.join("\n"))
        }
    }
}

/// Build a process-owned Service source snapshot only when at least one
/// verified package has explicit `service:package` approval.
///
/// Application `.service` sources are copied into the same snapshot so Service
/// Catalog compilation sees one ordinary directory tree. Package sources are
/// never copied into the application's source `service/` directory; ownership
/// remains under `.rbe/service-catalog/<generation>/packages/...`.
pub fn stage_if_needed(
    project_root: &Path,
    application_service_root: &Path,
    io: &AtomicIo,
) -> anyhow::Result<Option<StagedServiceCatalog>> {
    let project_root = fs::canonicalize(project_root).with_context(|| {
        format!(
            "canonicalize project root before package Service discovery: {}",
            project_root.display()
        )
    })?;
    if !project_root.is_dir() {
        bail!("package Service project root is not a directory");
    }

    let lock_path = project_root.join(PACKAGE_LOCK);
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "active package lock is not a safe regular file while discovering package Services: {}",
                    lock_path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect package lock before package Service discovery: {}",
                    lock_path.display()
                )
            })
        }
    }

    let snapshots = read_verified_rpx_root_snapshots(&project_root)
        .context("load verified RPX roots before package Service discovery")?;
    let mut packages = Vec::new();
    for snapshot in snapshots {
        let approved =
            crate::package_approval::approved_runtime_capabilities(&project_root, &snapshot)
                .with_context(|| {
                    format!(
                        "load package Service approval for verified root {:?}",
                        snapshot.package
                    )
                })?;
        if !approved
            .iter()
            .any(|capability| capability == PACKAGE_SERVICE_CAPABILITY)
        {
            continue;
        }
        let sources =
            read_verified_package_services(&project_root, &snapshot).with_context(|| {
                format!(
                    "read verified package-owned .service sources for {:?}",
                    snapshot.package
                )
            })?;
        packages.push((snapshot.package, sources));
    }

    if packages.is_empty() {
        return Ok(None);
    }

    let root = new_stage_root(&project_root)?;
    let application_root = root.join("application");
    ensure_directory(&application_root)?;
    copy_application_services(application_service_root, &application_root, io)?;

    let mut package_roots = Vec::new();
    let mut package_service_count = 0usize;
    for (package, sources) in packages {
        let package_key = service_runtime::service_source_digest_hex(&package);
        let package_root = root.join("packages").join(package_key);
        ensure_directory(&package_root)?;
        write_package_services(&package, &sources, &package_root, io)?;
        package_service_count = package_service_count.saturating_add(sources.len());
        package_roots.push((package, package_root));
    }

    tracing::info!(
        root = %root.display(),
        package_services = package_service_count,
        packages = package_roots.len(),
        "staged approved package-owned .service sources for Service Catalog compilation"
    );

    Ok(Some(StagedServiceCatalog {
        root,
        package_roots,
        package_service_count,
    }))
}

fn new_stage_root(project_root: &Path) -> anyhow::Result<PathBuf> {
    let rbe = project_root.join(".rbe");
    ensure_directory(&rbe)?;
    let state = rbe.join(SERVICE_CATALOG_STATE_DIR);
    ensure_directory(&state)?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = state.join(format!("{}-{nonce}", std::process::id()));
    if root.exists() {
        bail!(
            "package Service staging generation already exists unexpectedly: {}",
            root.display()
        );
    }
    fs::create_dir(&root).with_context(|| {
        format!(
            "create package Service staging generation: {}",
            root.display()
        )
    })?;
    Ok(root)
}

fn copy_application_services(
    source_root: &Path,
    target_root: &Path,
    io: &AtomicIo,
) -> anyhow::Result<()> {
    match fs::symlink_metadata(source_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "application Service source root is not a safe directory: {}",
                    source_root.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect application Service source root: {}",
                    source_root.display()
                )
            })
        }
    }
    copy_service_tree(source_root, source_root, target_root, io)
}

fn copy_service_tree(
    source_root: &Path,
    current: &Path,
    target_root: &Path,
    io: &AtomicIo,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(current)
        .with_context(|| format!("scan Service source directory: {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect Service source entry: {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "Service source tree contains a symbolic link, which is not allowed in a staged catalog: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            copy_service_tree(source_root, &path, target_root, io)?;
            continue;
        }
        if !metadata.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("service")
        {
            continue;
        }

        let relative = path.strip_prefix(source_root).with_context(|| {
            format!(
                "derive Service source path relative to catalog root: {}",
                path.display()
            )
        })?;
        validate_relative_path(relative)?;
        let destination = target_root.join(relative);
        ensure_directory(destination.parent().ok_or_else(|| {
            anyhow::anyhow!("staged application Service path has no parent directory")
        })?)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("read application Service source: {}", path.display()))?;
        io.write_atomic(&destination, &bytes).with_context(|| {
            format!(
                "stage application Service source into catalog snapshot: {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn write_package_services(
    package: &str,
    sources: &[VerifiedPackageServiceSource],
    target_root: &Path,
    io: &AtomicIo,
) -> anyhow::Result<()> {
    for source in sources {
        if source.package != package {
            bail!(
                "verified package Service identity drifted during staging: expected {:?}, observed {:?}",
                package,
                source.package
            );
        }
        let archive_path = Path::new(&source.archive_path);
        let relative = archive_path.strip_prefix("service").with_context(|| {
            format!(
                "verified package Service path is outside service/: {:?}",
                source.archive_path
            )
        })?;
        validate_relative_path(relative)?;
        if relative.extension().and_then(|value| value.to_str()) != Some("service") {
            bail!(
                "verified package Service path lost .service extension during staging: {:?}",
                source.archive_path
            );
        }
        let actual_sha256 = service_runtime::service_source_digest_hex(&source.source);
        if !actual_sha256.eq_ignore_ascii_case(&source.source_sha256) {
            bail!(
                "verified package Service source changed before staging for {:?}: expected {}, got {}",
                source.archive_path,
                source.source_sha256,
                actual_sha256
            );
        }

        let destination = target_root.join(relative);
        ensure_directory(destination.parent().ok_or_else(|| {
            anyhow::anyhow!("staged package Service path has no parent directory")
        })?)?;
        io.write_atomic(&destination, source.source.as_bytes())
            .with_context(|| {
                format!(
                    "stage verified package Service source: {}",
                    destination.display()
                )
            })?;
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() {
        bail!("Service source path cannot be empty");
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!(
                "Service source path contains an unsafe component: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "Service catalog path is not a safe directory: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| format!("create Service catalog directory: {}", path.display())),
        Err(error) => Err(error)
            .with_context(|| format!("inspect Service catalog directory: {}", path.display())),
    }
}

fn valid_service_namespace(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}
