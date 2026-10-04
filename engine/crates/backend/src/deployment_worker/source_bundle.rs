use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use rbe_install_runtime::GitSourceReceipt;
use sha2::{Digest, Sha256};

const MAGIC: &[u8] = b"RBE-SOURCE-BUNDLE-V1\0";
const MAX_FILES: usize = 100_000;
const MAX_TOTAL_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BUNDLE_BYTES: u64 = 576 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const SOURCE_TREE_ALGORITHM: &str = "rbe-source-tree-sha256-v1";

#[derive(Debug, Clone)]
pub struct SourceBundle {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug)]
struct SourceEntry {
    path: String,
    disk_path: PathBuf,
    size: u64,
    executable: bool,
}

pub async fn create_source_bundle(
    source_root: &Path,
    receipt: &GitSourceReceipt,
    destination: &Path,
) -> Result<SourceBundle> {
    let source_root = source_root.to_path_buf();
    let receipt = receipt.clone();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        create_source_bundle_sync(&source_root, &receipt, &destination)
    })
    .await
    .context("join source bundle writer")?
}

fn create_source_bundle_sync(
    source_root: &Path,
    receipt: &GitSourceReceipt,
    destination: &Path,
) -> Result<SourceBundle> {
    if receipt.source_tree.algorithm != SOURCE_TREE_ALGORITHM {
        anyhow::bail!("source receipt uses an unsupported tree algorithm");
    }
    let root_meta = fs::symlink_metadata(source_root)
        .with_context(|| format!("stat source root {}", source_root.display()))?;
    if !source_root.is_absolute() || !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        anyhow::bail!("deployment source root must be an absolute regular directory");
    }
    if destination.exists() {
        anyhow::bail!("deployment source bundle destination must be fresh");
    }
    if !destination.is_absolute() {
        anyhow::bail!("deployment source bundle destination must be absolute");
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut entries = Vec::new();
    collect_entries(source_root, source_root, &mut entries)?;
    if entries.is_empty() || entries.len() > MAX_FILES {
        anyhow::bail!("deployment source bundle has an invalid file count");
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let mut casefolded = BTreeSet::new();
    let mut total_source_bytes = 0_u64;
    for entry in &entries {
        if !casefolded.insert(entry.path.to_ascii_lowercase()) {
            anyhow::bail!("deployment source bundle contains a case-colliding path");
        }
        total_source_bytes = total_source_bytes
            .checked_add(entry.size)
            .context("deployment source size accounting overflow")?;
        if entry.size > MAX_FILE_BYTES || total_source_bytes > MAX_TOTAL_SOURCE_BYTES {
            anyhow::bail!("deployment source bundle exceeds RBE source limits");
        }
    }
    if entries.len() != receipt.source_tree.file_count
        || total_source_bytes != receipt.source_tree.total_bytes
    {
        anyhow::bail!("materialized source no longer matches sealed receipt dimensions");
    }

    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .with_context(|| format!("create source bundle {}", destination.display()))?;
    let mut bundle_hasher = Sha256::new();
    let mut bundle_bytes = 0_u64;
    write_hashed(&mut output, &mut bundle_hasher, MAGIC, &mut bundle_bytes)?;
    let file_count = u32::try_from(entries.len()).context("source file count does not fit u32")?;
    write_hashed(
        &mut output,
        &mut bundle_hasher,
        &file_count.to_be_bytes(),
        &mut bundle_bytes,
    )?;

    let mut tree_hasher = Sha256::new();
    tree_hasher.update(b"RBE-SOURCE-TREE-SHA256-V1\0");
    let mut buffer = vec![0_u8; 64 * 1024];

    for entry in &entries {
        let path_bytes = entry.path.as_bytes();
        if path_bytes.is_empty() || path_bytes.len() > MAX_PATH_BYTES {
            anyhow::bail!("source path length is outside bundle limits");
        }
        let path_len = u32::try_from(path_bytes.len()).context("source path length overflow")?;
        write_hashed(
            &mut output,
            &mut bundle_hasher,
            &path_len.to_be_bytes(),
            &mut bundle_bytes,
        )?;
        write_hashed(
            &mut output,
            &mut bundle_hasher,
            path_bytes,
            &mut bundle_bytes,
        )?;
        write_hashed(
            &mut output,
            &mut bundle_hasher,
            &[u8::from(entry.executable)],
            &mut bundle_bytes,
        )?;
        write_hashed(
            &mut output,
            &mut bundle_hasher,
            &entry.size.to_be_bytes(),
            &mut bundle_bytes,
        )?;

        let mut input = File::open(&entry.disk_path)
            .with_context(|| format!("open source file {}", entry.disk_path.display()))?;
        let mut observed = 0_u64;
        let mut file_hasher = Sha256::new();
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .context("source file size accounting overflow")?;
            if observed > entry.size {
                anyhow::bail!("source file changed while building durable bundle");
            }
            file_hasher.update(&buffer[..read]);
            write_hashed(
                &mut output,
                &mut bundle_hasher,
                &buffer[..read],
                &mut bundle_bytes,
            )?;
        }
        if observed != entry.size {
            anyhow::bail!("source file changed while building durable bundle");
        }
        let file_sha256 = format!("{:x}", file_hasher.finalize());
        tree_hasher.update((path_bytes.len() as u64).to_be_bytes());
        tree_hasher.update(path_bytes);
        tree_hasher.update(entry.size.to_be_bytes());
        tree_hasher.update(file_sha256.as_bytes());
    }

    let tree_sha256 = format!("{:x}", tree_hasher.finalize());
    if !tree_sha256.eq_ignore_ascii_case(&receipt.source_tree.sha256) {
        anyhow::bail!("durable source bundle does not match sealed RBE source-tree digest");
    }
    if bundle_bytes == 0 || bundle_bytes > MAX_BUNDLE_BYTES {
        anyhow::bail!("deployment source bundle exceeds durable artifact limit");
    }
    output.flush()?;
    output.sync_all()?;

    Ok(SourceBundle {
        path: destination.to_path_buf(),
        sha256: format!("{:x}", bundle_hasher.finalize()),
        size_bytes: bundle_bytes,
    })
}

fn collect_entries(root: &Path, directory: &Path, entries: &mut Vec<SourceEntry>) -> Result<()> {
    let mut children = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let disk_path = child.path();
        let metadata = fs::symlink_metadata(&disk_path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            anyhow::bail!("deployment source tree contains a symbolic link");
        }
        if file_type.is_dir() {
            collect_entries(root, &disk_path, entries)?;
            continue;
        }
        if !file_type.is_file() {
            anyhow::bail!("deployment source tree contains an unsupported filesystem entry");
        }
        if entries.len() >= MAX_FILES {
            anyhow::bail!("deployment source tree exceeds file-count limit");
        }
        let relative = disk_path
            .strip_prefix(root)
            .context("source file escaped source root")?;
        let path = canonical_path(relative)?;
        entries.push(SourceEntry {
            path,
            disk_path,
            size: metadata.len(),
            executable: executable(&metadata),
        });
    }
    Ok(())
}

fn canonical_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().context("source path is not UTF-8")?;
                if value.is_empty() || value == "." || value == ".." || value.contains(':') {
                    anyhow::bail!("source path contains an unsafe component");
                }
                parts.push(value);
            }
            _ => anyhow::bail!("source path contains an unsafe component"),
        }
    }
    if parts.is_empty() {
        anyhow::bail!("source path is empty");
    }
    let path = parts.join("/");
    if path.len() > MAX_PATH_BYTES || path.contains('\\') {
        anyhow::bail!("source path is outside bundle limits");
    }
    Ok(path)
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn write_hashed(
    output: &mut File,
    hasher: &mut Sha256,
    bytes: &[u8],
    observed: &mut u64,
) -> Result<()> {
    *observed = observed
        .checked_add(bytes.len() as u64)
        .context("source bundle size accounting overflow")?;
    if *observed > MAX_BUNDLE_BYTES {
        anyhow::bail!("deployment source bundle exceeds durable artifact limit");
    }
    output.write_all(bytes)?;
    hasher.update(bytes);
    Ok(())
}
