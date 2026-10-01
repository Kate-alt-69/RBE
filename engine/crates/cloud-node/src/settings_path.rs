use std::path::{Path, PathBuf};

pub const PREFERRED_SETTINGS_FILE_NAME: &str = "setting.cn.json";
pub const LEGACY_SETTINGS_FILE_NAME: &str = "setting.node.cn.json";

/// Discover an implicit Cloud Node settings file beside an executable.
///
/// `setting.cn.json` is the preferred DX name. The original
/// `setting.node.cn.json` remains supported for compatibility, but if both are
/// present Cloud Node refuses to guess which configuration is authoritative.
pub fn discover_settings_path(executable: &Path) -> anyhow::Result<Option<PathBuf>> {
    let parent = executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node executable {} has no parent directory for settings discovery",
                executable.display()
            )
        })?;
    let preferred = parent.join(PREFERRED_SETTINGS_FILE_NAME);
    let legacy = parent.join(LEGACY_SETTINGS_FILE_NAME);
    let preferred_exists = preferred.is_file();
    let legacy_exists = legacy.is_file();

    match (preferred_exists, legacy_exists) {
        (true, true) => anyhow::bail!(
            "Cloud Node found both {} and {} beside {}; keep one settings file or select an explicit path with --config/RBE_CN_SETTINGS",
            preferred.display(),
            legacy.display(),
            executable.display()
        ),
        (true, false) => Ok(Some(preferred)),
        (false, true) => Ok(Some(legacy)),
        (false, false) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cn-settings-discovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn preferred_and_legacy_settings_are_deterministic() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let executable = root.join(if cfg!(windows) {
            "cloud_node.exe"
        } else {
            "cloud_node"
        });

        assert!(discover_settings_path(&executable).unwrap().is_none());

        let legacy = root.join(LEGACY_SETTINGS_FILE_NAME);
        fs::write(&legacy, b"{}").unwrap();
        assert_eq!(
            discover_settings_path(&executable).unwrap(),
            Some(legacy.clone())
        );

        let preferred = root.join(PREFERRED_SETTINGS_FILE_NAME);
        fs::write(&preferred, b"{}").unwrap();
        assert!(discover_settings_path(&executable).is_err());

        fs::remove_file(&legacy).unwrap();
        assert_eq!(
            discover_settings_path(&executable).unwrap(),
            Some(preferred)
        );
        let _ = fs::remove_dir_all(root);
    }
}
