use std::fmt;
use std::path::{Path, PathBuf};

use sdk_package::check_package;

#[derive(Debug, Clone)]
pub struct ValidationSummary {
    pub package_name: String,
    pub version: String,
    pub component_count: usize,
}

#[derive(Debug)]
pub enum ValidationFailure {
    Package(String),
    Infrastructure(anyhow::Error),
}

impl fmt::Display for ValidationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package(message) => write!(formatter, "{message}"),
            Self::Infrastructure(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for ValidationFailure {}

pub async fn validate_package(
    source_root: &Path,
    build_root: &str,
    expected_package: Option<&str>,
) -> Result<ValidationSummary, ValidationFailure> {
    let package_root =
        resolve_build_root(source_root, build_root).map_err(ValidationFailure::Package)?;
    if !package_root.is_dir() {
        return Err(ValidationFailure::Package(format!(
            "configured package root {build_root:?} does not exist"
        )));
    }

    let target = package_root.clone();
    let checked = tokio::task::spawn_blocking(move || check_package(&target))
        .await
        .map_err(|error| {
            ValidationFailure::Infrastructure(anyhow::anyhow!(
                "join shared RPX package checker: {error}"
            ))
        })?
        .map_err(|error| ValidationFailure::Package(error.to_string()))?;

    if let Some(expected) = expected_package {
        if checked.manifest.package.name != expected {
            return Err(ValidationFailure::Package(format!(
                "repository package name {:?} does not match deployment package {:?}",
                checked.manifest.package.name, expected
            )));
        }
    }

    Ok(ValidationSummary {
        package_name: checked.manifest.package.name,
        version: checked.manifest.package.version,
        component_count: checked.components.len(),
    })
}

fn resolve_build_root(source_root: &Path, build_root: &str) -> Result<PathBuf, String> {
    if !source_root.is_absolute() {
        return Err("validation source root is not absolute".to_owned());
    }
    if build_root == "/" {
        return Ok(source_root.to_path_buf());
    }
    let relative = build_root
        .strip_prefix('/')
        .ok_or_else(|| "configured package root must start with '/'".to_owned())?;
    let mut root = source_root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.contains('\\')
            || segment.contains('\0')
            || segment.contains(':')
        {
            return Err("configured package root contains an unsafe segment".to_owned());
        }
        root.push(segment);
    }
    if !root.starts_with(source_root) || root == source_root {
        return Err("configured package root escaped the restored source tree".to_owned());
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_root_stays_inside_source_tree() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\rbe\source")
        } else {
            PathBuf::from("/tmp/rbe/source")
        };
        assert_eq!(resolve_build_root(&root, "/").unwrap(), root);
        assert_eq!(
            resolve_build_root(&root, "/packages/mail").unwrap(),
            root.join("packages").join("mail")
        );
        assert!(resolve_build_root(&root, "/../secret").is_err());
        assert!(resolve_build_root(&root, "relative").is_err());
    }
}
