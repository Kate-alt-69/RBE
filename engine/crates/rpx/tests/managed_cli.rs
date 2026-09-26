use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_project(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "rpx-managed-cli-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("components/hello")).unwrap();
    fs::write(
        root.join("package.rbe.toml"),
        r#"[package]
name = "managed-smoke"
version = "1.0.0"
language = "javascript"
runtime = "node"
"#,
    )
    .unwrap();
    fs::write(
        root.join("components/hello/hello.js"),
        "export function hello() { return 'hi'; }\n",
    )
    .unwrap();
    root
}

fn rpx() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rpx"))
}

fn compile(root: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(rpx());
    command.arg("compile").arg(root.join("components/hello"));
    command.args(extra);
    command.output().unwrap()
}

#[test]
fn compile_fails_closed_without_managed_toolchain() {
    let root = temp_project("missing");
    let output = compile(&root, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("RBE-managed compiler \"node\" is required"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_managed_toolchain_never_falls_back_to_host_even_when_opted_in() {
    let root = temp_project("partial");
    fs::create_dir_all(root.join(".rbe")).unwrap();
    let fake_python = root.join("fake-python");
    fs::write(
        root.join(".rbe/rpx-toolchain.json"),
        serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "tools": { "python": fake_python }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = compile(&root, &["--allow-host-toolchain"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("RBE-managed compiler \"node\" is not installed"));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn compile_executes_exact_managed_program_with_cleared_environment() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_project("exact-program");
    fs::create_dir_all(root.join(".rbe")).unwrap();
    let marker = root.join("managed-invocation.txt");
    let fake_node = root.join("managed-node.sh");
    fs::write(
        &fake_node,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nif [ -n \"$PATH\" ]; then printf 'PATH=%s\\n' \"$PATH\" >> '{}'; fi\nexit 0\n",
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_node).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&fake_node, permissions).unwrap();

    fs::write(
        root.join(".rbe/rpx-toolchain.json"),
        serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "tools": { "node": fake_node.canonicalize().unwrap() }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = compile(&root, &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let invocation = fs::read_to_string(&marker).unwrap();
    assert!(invocation.contains("--check"));
    assert!(!invocation.contains("PATH="));
    fs::remove_dir_all(root).unwrap();
}
