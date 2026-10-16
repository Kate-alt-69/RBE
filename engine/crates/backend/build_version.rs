//! Build identity is separate from package/ABI versions. Development snapshots
//! carry an immutable source SHA and unique build number without editing Cargo.
use std::{env, fs, path::Path};
pub fn emit(source: &str, target: &str) {
    let base_path = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../../VERSION");
    println!("cargo:rerun-if-changed={}", base_path.display());
    for name in ["RBE_BINARY_VERSION", "RBE_BINARY_REPOSITORY"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    // Cargo otherwise may reuse a build-script result after only HEAD changes,
    // leaving a local binary reporting the previous source commit.
    let git_path = |name: &str| {
        std::process::Command::new("git")
            .args(["rev-parse", "--git-path", name])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(path) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
    {
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout);
            if let Some(path) = git_path(branch.trim()) {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    let base = fs::read_to_string(base_path)
        .expect("read VERSION")
        .trim()
        .to_string();
    let version = env::var("RBE_BINARY_VERSION").unwrap_or_else(|_| base.clone());
    assert!(valid(&base, &version), "invalid RBE binary version");
    let repository = env::var("RBE_BINARY_REPOSITORY").unwrap_or_else(|_| "Kate-alt-69/RBE".into());
    assert!(
        repository.split('/').count() == 2
            && repository.split('/').all(|v| !v.is_empty()
                && v != "."
                && v != ".."
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))),
        "invalid binary repository"
    );
    for (key, value) in [
        ("RBE_VERSION", version.as_str()),
        ("RBE_BASE_VERSION", base.as_str()),
        ("RBE_SOURCE_COMMIT", source),
        ("RBE_TARGET", target),
        ("RBE_BINARY_REPOSITORY", repository.as_str()),
    ] {
        assert!(
            !value.chars().any(char::is_control),
            "invalid build metadata"
        );
        println!("cargo:rustc-env={key}={value}");
    }
}
fn number(s: &str) -> bool {
    !s.is_empty() && (s == "0" || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit())
}
fn valid(base: &str, version: &str) -> bool {
    base.split('.').count() == 3
        && base.split('.').all(number)
        && (version == base
            || version
                .strip_prefix(&format!("{base}-dev."))
                .is_some_and(|s| s.split('.').count() == 2 && s.split('.').all(number)))
}
#[cfg(test)]
mod tests {
    #[test]
    fn identities_are_bounded_to_the_base_version() {
        assert!(super::valid("0.1.0", "0.1.0-dev.14.1"));
        for bad in ["9.1.0-dev.14.1", "0.1.0-dev.01.1", "0.1.0-dev.2.1\n"] {
            assert!(!super::valid("0.1.0", bad));
        }
    }
}
