use rpx::application_install::install_application;
use rpx::project::{ProjectLock, ProjectPaths};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "rpx-application-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn empty_application_installs_without_registry_and_writes_json_lock() {
    let root = temp_root("empty");
    fs::write(
        root.join("package.rbe.json"),
        r#"{"name":"empty-app","packages":{},"scripts":{"dev":"rpx --help"}}"#,
    )
    .unwrap();

    let report = install_application(&root, None).unwrap();
    assert_eq!(report.roots, 0);
    assert_eq!(report.private_packages, 0);
    assert!(!report.reused_lock);
    assert_eq!(report.lock_path, ProjectPaths::new(&root).lock_path());

    let lock = ProjectLock::load(&root).unwrap().unwrap();
    assert!(lock.packages.is_empty());
    assert!(lock.private.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn custom_sources_fail_closed_before_any_registry_access() {
    let root = temp_root("custom-source");
    fs::write(
        root.join("package.rbe.json"),
        r#"{"packages":{"demo":{"version":"^1","source":"https://example.invalid/demo.rbe.zip"}}}"#,
    )
    .unwrap();

    let error = install_application(&root, None).unwrap_err().to_string();
    assert!(error.contains("custom source"));
    assert!(error.contains("source hashes"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn fresh_registry_package_rejects_insecure_registry_override_before_network() {
    let root = temp_root("registry-https");
    fs::write(
        root.join("package.rbe.json"),
        r#"{"packages":{"demo":"^1"}}"#,
    )
    .unwrap();

    let error = install_application(&root, Some("http://registry.example"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires HTTPS"));
    let _ = fs::remove_dir_all(root);
}
