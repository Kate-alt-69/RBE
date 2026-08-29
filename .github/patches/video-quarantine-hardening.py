from pathlib import Path

path = Path("engine/crates/video-manager/src/download_worker.rs")
source = path.read_text()

old = '''            match tokio::time::timeout(
                policy.total_timeout,
                download_into_quarantine(target, &quarantine_path, &policy),
            )
'''
new = '''            match tokio::time::timeout(
                policy.total_timeout,
                download_into_quarantine(
                    target,
                    &quarantine_path,
                    &self.quarantine_root,
                    &policy,
                ),
            )
'''
if old not in source:
    raise SystemExit("download invocation anchor missing")
source = source.replace(old, new, 1)

old = '''async fn download_into_quarantine(
    mut target: DownloadTarget,
    quarantine_path: &Path,
    policy: &DownloadPolicy,
) -> anyhow::Result<DownloadReceipt> {
'''
new = '''async fn download_into_quarantine(
    mut target: DownloadTarget,
    quarantine_path: &Path,
    quarantine_root: &Path,
    policy: &DownloadPolicy,
) -> anyhow::Result<DownloadReceipt> {
'''
if old not in source:
    raise SystemExit("download function signature anchor missing")
source = source.replace(old, new, 1)

old = '''        let mut response = response;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(quarantine_path)
            .await
            .with_context(|| {
                format!(
                    "open Video Manager quarantine file {}",
                    quarantine_path.display()
                )
            })?;
        let mut bytes = 0u64;
'''
new = '''        let mut response = response;
        let mut file = open_reserved_quarantine(quarantine_path, quarantine_root).await?;
        let mut bytes = 0u64;
'''
if old not in source:
    raise SystemExit("quarantine open anchor missing")
source = source.replace(old, new, 1)

anchor = '''fn redirect_target(current: &DownloadTarget, location: &str) -> anyhow::Result<DownloadTarget> {
'''
helper = r'''async fn open_reserved_quarantine(
    quarantine_path: &Path,
    quarantine_root: &Path,
) -> anyhow::Result<tokio::fs::File> {
    let before = tokio::fs::symlink_metadata(quarantine_path)
        .await
        .with_context(|| {
            format!(
                "inspect reserved Video Manager quarantine file {}",
                quarantine_path.display()
            )
        })?;
    if !before.file_type().is_file() {
        anyhow::bail!("Video Manager quarantine entry is not a reserved regular file");
    }

    let canonical = tokio::fs::canonicalize(quarantine_path)
        .await
        .with_context(|| {
            format!(
                "canonicalize reserved Video Manager quarantine file {}",
                quarantine_path.display()
            )
        })?;
    if !canonical.starts_with(quarantine_root) {
        anyhow::bail!("Video Manager quarantine file escaped its storage root");
    }

    let path = quarantine_path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("open reserved Video Manager quarantine file {}", path.display()))
    })
    .await
    .context("Video Manager quarantine open task failed")??;

    let opened = file
        .metadata()
        .context("inspect opened Video Manager quarantine handle")?;
    let after = std::fs::symlink_metadata(quarantine_path).with_context(|| {
        format!(
            "reinspect reserved Video Manager quarantine file {}",
            quarantine_path.display()
        )
    })?;
    if !opened.file_type().is_file()
        || !after.file_type().is_file()
        || !same_file_identity(&before, &opened)
        || !same_file_identity(&opened, &after)
    {
        anyhow::bail!("Video Manager quarantine file identity changed while opening");
    }

    file.set_len(0)
        .context("truncate verified Video Manager quarantine file")?;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

'''
if anchor not in source:
    raise SystemExit("quarantine helper insertion anchor missing")
source = source.replace(anchor, helper + anchor, 1)

# Unit tests exercise the filesystem boundary without performing network I/O.
test_anchor = '''    #[test]
    fn redirect_policy_accepts_relative_public_targets() {
'''
tests = r'''    fn quarantine_test_root(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-video-quarantine-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    #[tokio::test]
    async fn reserved_quarantine_open_requires_existing_regular_file() {
        let root = quarantine_test_root("missing");
        let missing = root.join("missing.part");
        assert!(open_reserved_quarantine(&missing, &root).await.is_err());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reserved_quarantine_open_truncates_only_verified_file() {
        let root = quarantine_test_root("regular");
        let path = root.join("reserved.part");
        std::fs::write(&path, b"stale bytes").unwrap();
        let file = open_reserved_quarantine(&path, &root).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 0);
        drop(file);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reserved_quarantine_open_rejects_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = quarantine_test_root("symlink");
        let outside_root = quarantine_test_root("outside");
        let target = outside_root.join("target.bin");
        std::fs::write(&target, b"do not truncate").unwrap();
        let link = root.join("reserved.part");
        symlink(&target, &link).unwrap();

        assert!(open_reserved_quarantine(&link, &root).await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not truncate");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn redirect_policy_accepts_relative_public_targets() {
'''
if test_anchor not in source:
    raise SystemExit("download worker test insertion anchor missing")
source = source.replace(test_anchor, tests, 1)
path.write_text(source)
