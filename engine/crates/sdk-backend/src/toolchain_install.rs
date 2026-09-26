use anyhow::{bail, Context, Result};
use rpx::toolchain::{verify_managed_program, ManagedCompilerToolchain};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROJECT_TOOLCHAIN_FILE: &str = "rpx-toolchain.json";

pub fn install_verified_toolchain(project: &Path, source: &Path) -> Result<usize> {
    let source = absolute(source)?;
    let input = fs::read_to_string(&source)
        .with_context(|| format!("failed to read RPX toolchain handoff {}", source.display()))?;
    let toolchain = parse_and_verify(&input).with_context(|| {
        format!(
            "refused RPX toolchain handoff {}; only fully pinned, currently matching managed compiler files may be admitted",
            source.display()
        )
    })?;

    let rbe = project.join(".rbe");
    fs::create_dir_all(&rbe)?;
    let destination = rbe.join(PROJECT_TOOLCHAIN_FILE);
    if source.canonicalize().ok() == destination.canonicalize().ok() {
        return Ok(toolchain.tools.len());
    }

    let temporary = rbe.join(format!("{PROJECT_TOOLCHAIN_FILE}.new"));
    fs::write(&temporary, input.as_bytes()).with_context(|| {
        format!(
            "failed to stage managed RPX toolchain at {}",
            temporary.display()
        )
    })?;

    match fs::rename(&temporary, &destination) {
        Ok(()) => {}
        Err(first) if destination.exists() => {
            fs::remove_file(&destination).with_context(|| {
                format!(
                    "failed to replace existing RPX toolchain {} after rename error: {first}",
                    destination.display()
                )
            })?;
            fs::rename(&temporary, &destination).with_context(|| {
                format!(
                    "failed to publish verified RPX toolchain {}",
                    destination.display()
                )
            })?;
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "failed to publish verified RPX toolchain {}",
                    destination.display()
                )
            });
        }
    }

    Ok(toolchain.tools.len())
}

pub fn verify_project_toolchain(project: &Path) -> Result<usize> {
    let path = project.join(".rbe").join(PROJECT_TOOLCHAIN_FILE);
    let input = fs::read_to_string(&path)
        .with_context(|| format!("managed RPX toolchain is missing: {}", path.display()))?;
    let toolchain = parse_and_verify(&input)
        .with_context(|| format!("managed RPX toolchain verification failed: {}", path.display()))?;
    Ok(toolchain.tools.len())
}

pub fn project_toolchain_exists(project: &Path) -> bool {
    project
        .join(".rbe")
        .join(PROJECT_TOOLCHAIN_FILE)
        .is_file()
}

fn parse_and_verify(input: &str) -> Result<ManagedCompilerToolchain> {
    let toolchain = ManagedCompilerToolchain::parse_json(input)?;
    if toolchain.tools.is_empty() {
        bail!("managed RPX toolchain contains no compiler entries");
    }
    for (name, tool) in &toolchain.tools {
        verify_managed_program(&tool.path, &tool.sha256).with_context(|| {
            format!(
                "managed compiler {name:?} does not match its pinned identity at {}",
                tool.path.display()
            )
        })?;
    }
    Ok(toolchain)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpx::toolchain::sha256_file;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sdk-backend-toolchain-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn handoff_is_verified_before_project_admission_and_on_status() {
        let project = temp_project("admission");
        let tool = project.join("managed-node.bin");
        fs::write(&tool, b"trusted node bytes").unwrap();
        let hash = sha256_file(&tool).unwrap();
        let handoff = project.join("handoff.json");
        fs::write(
            &handoff,
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": 2,
                "tools": {
                    "node": {
                        "path": tool.canonicalize().unwrap(),
                        "sha256": hash,
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(install_verified_toolchain(&project, &handoff).unwrap(), 1);
        assert_eq!(verify_project_toolchain(&project).unwrap(), 1);

        fs::write(&tool, b"replaced node bytes").unwrap();
        let error = verify_project_toolchain(&project).unwrap_err();
        assert!(format!("{error:#}").contains("pinned identity"));
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn invalid_handoff_never_replaces_existing_verified_map() {
        let project = temp_project("reject");
        let tool = project.join("managed-python.bin");
        fs::write(&tool, b"trusted python bytes").unwrap();
        let hash = sha256_file(&tool).unwrap();
        let good = project.join("good.json");
        fs::write(
            &good,
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": 2,
                "tools": {
                    "python": {
                        "path": tool.canonicalize().unwrap(),
                        "sha256": hash,
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        install_verified_toolchain(&project, &good).unwrap();
        let installed = project.join(".rbe").join(PROJECT_TOOLCHAIN_FILE);
        let before = fs::read(&installed).unwrap();

        let bad = project.join("bad.json");
        fs::write(
            &bad,
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": 2,
                "tools": {
                    "python": {
                        "path": tool.canonicalize().unwrap(),
                        "sha256": "a".repeat(64),
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(install_verified_toolchain(&project, &bad).is_err());
        assert_eq!(fs::read(&installed).unwrap(), before);
        fs::remove_dir_all(project).unwrap();
    }
}
