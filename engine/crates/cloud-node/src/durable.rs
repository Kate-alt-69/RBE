use std::fs;
#[cfg(any(unix, test))]
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let missing = missing_directories(path)?;
    fs::create_dir_all(path)?;
    for directory in missing.iter().rev() {
        sync_parent(directory)?;
    }
    Ok(())
}

pub(crate) fn rename(source: impl AsRef<Path>, target: impl AsRef<Path>) -> io::Result<()> {
    let source = source.as_ref();
    let target = target.as_ref();
    let source_parent = parent_path(source);
    let target_parent = parent_path(target);
    fs::rename(source, target)?;
    sync_directory(&target_parent)?;
    if source_parent != target_parent {
        sync_directory(&source_parent)?;
    }
    Ok(())
}

pub(crate) fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let parent = parent_path(path);
    fs::remove_file(path)?;
    sync_directory(&parent)
}

pub(crate) fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let parent = parent_path(path);
    fs::remove_dir_all(path)?;
    sync_directory(&parent)
}

pub(crate) fn sync_parent(path: impl AsRef<Path>) -> io::Result<()> {
    sync_directory(&parent_path(path.as_ref()))
}

fn parent_path(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn missing_directories(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match fs::metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a directory", current.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.clone());
                let Some(parent) = current.parent() else {
                    break;
                };
                if parent.as_os_str().is_empty() {
                    break;
                }
                current = parent.to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
    Ok(missing)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(feature = "client")]
pub(crate) async fn create_dir_all_async(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let missing = missing_directories_async(path).await?;
    tokio::fs::create_dir_all(path).await?;
    for directory in missing.iter().rev() {
        sync_parent_async(directory).await?;
    }
    Ok(())
}

#[cfg(feature = "client")]
pub(crate) async fn rename_async(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> io::Result<()> {
    let source = source.as_ref();
    let target = target.as_ref();
    let source_parent = parent_path(source);
    let target_parent = parent_path(target);
    tokio::fs::rename(source, target).await?;
    sync_directory_async(&target_parent).await?;
    if source_parent != target_parent {
        sync_directory_async(&source_parent).await?;
    }
    Ok(())
}

#[cfg(feature = "client")]
pub(crate) async fn remove_file_async(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let parent = parent_path(path);
    tokio::fs::remove_file(path).await?;
    sync_directory_async(&parent).await
}

#[cfg(feature = "client")]
pub(crate) async fn remove_dir_all_async(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let parent = parent_path(path);
    tokio::fs::remove_dir_all(path).await?;
    sync_directory_async(&parent).await
}

#[cfg(feature = "client")]
async fn missing_directories_async(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match tokio::fs::metadata(&current).await {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a directory", current.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.clone());
                let Some(parent) = current.parent() else {
                    break;
                };
                if parent.as_os_str().is_empty() {
                    break;
                }
                current = parent.to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
    Ok(missing)
}

#[cfg(all(feature = "client", unix))]
async fn sync_directory_async(path: &Path) -> io::Result<()> {
    tokio::fs::File::open(path).await?.sync_all().await
}

#[cfg(all(feature = "client", not(unix)))]
async fn sync_directory_async(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(feature = "client")]
async fn sync_parent_async(path: &Path) -> io::Result<()> {
    sync_directory_async(&parent_path(path)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-durable-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn durable_directory_creation_and_rename_round_trip() {
        let root = test_root("rename");
        let nested = root.join("a/b/c");
        create_dir_all(&nested).unwrap();
        let part = nested.join("value.part");
        let target = nested.join("value");
        fs::write(&part, b"durable").unwrap();
        File::open(&part).unwrap().sync_all().unwrap();
        sync_parent(&part).unwrap();
        rename(&part, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"durable");
        remove_dir_all(&root).unwrap();
    }
}
