from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


# backend should not gain a new direct Win32 dependency merely to label service
# processes. service-runtime already owns the required platform dependencies.
path = "engine/crates/backend/Cargo.toml"
text = read(path)
text = text.replace(
    '''\n[target.'cfg(windows)'.dependencies]\nwindows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Console", "Win32_System_Threading"] }\n''',
    "",
    1,
)
write(path, text)

# Extend the already-existing service-runtime Windows dependency with Console;
# Cargo.lock already contains windows-sys, and feature selection is not a new
# package dependency.
path = "engine/crates/service-runtime/Cargo.toml"
text = read(path)
old = 'windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Security", "Win32_System_JobObjects", "Win32_System_Threading"] }'
new = 'windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Security", "Win32_System_Console", "Win32_System_JobObjects", "Win32_System_Threading"] }'
if old not in text and new not in text:
    raise SystemExit("missing service-runtime windows-sys dependency anchor")
text = text.replace(old, new, 1)
write(path, text)

# service-runtime owns portable process metadata. On Windows the console title
# is changed only if this process owns a console by itself; shared inherited
# consoles must not be renamed by a worker. On Linux/macOS, the native process
# or thread label is set while argv still carries the full --service-file/name.
path = "engine/crates/service-runtime/src/lib.rs"
text = read(path)
marker = '''#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]\n#[serde(rename_all = "kebab-case")]\npub enum RestartPolicy {\n'''
if marker not in text:
    raise SystemExit("missing service-runtime process label insertion anchor")
if "pub fn apply_service_process_label" not in text:
    helper = r'''pub fn apply_service_process_label(label: &str) {
    apply_service_process_label_platform(label);
}

fn short_service_process_label(label: &str, max_bytes: usize) -> String {
    let semantic = if label == "service - mother" {
        "service-mother"
    } else {
        label.strip_prefix("service - ")
            .and_then(|value| value.split(" | ").next())
            .unwrap_or(label)
    };
    let mut out = String::new();
    for character in semantic.chars() {
        if character.is_control() || character == '\0' {
            continue;
        }
        if out.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        out.push(character);
    }
    if out.is_empty() {
        "service".into()
    } else {
        out
    }
}

#[cfg(windows)]
fn apply_service_process_label_platform(label: &str) {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, SetConsoleTitleW};
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadDescription};

    let clean = label
        .chars()
        .filter(|character| !character.is_control() && *character != '\0')
        .take(120)
        .collect::<String>();
    let wide = clean
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        let _ = SetThreadDescription(GetCurrentThread(), wide.as_ptr());
        let mut processes = [0u32; 2];
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            let _ = SetConsoleTitleW(wide.as_ptr());
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_service_process_label_platform(label: &str) {
    use std::ffi::CString;
    let short = short_service_process_label(label, 15);
    if let Ok(name) = CString::new(short) {
        unsafe {
            let _ = libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0);
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_service_process_label_platform(label: &str) {
    use std::ffi::CString;
    let short = short_service_process_label(label, 63);
    if let Ok(name) = CString::new(short) {
        unsafe {
            let _ = libc::pthread_setname_np(name.as_ptr());
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn apply_service_process_label_platform(_label: &str) {}

#[cfg(not(any(unix, windows)))]
fn apply_service_process_label_platform(_label: &str) {}

'''
    text = text.replace(marker, helper + marker, 1)

# A pure testable naming helper should preserve the .service identity where the
# platform length allows it.
test_marker = '''#[cfg(test)]\nmod tests {\n'''
if test_marker in text and "short_process_label_preserves_service_file_identity" not in text:
    test = r'''    #[test]
    fn short_process_label_preserves_service_file_identity() {
        assert_eq!(short_service_process_label("service - mother", 15), "service-mother");
        assert_eq!(
            short_service_process_label("service - auth.service | service.exe", 15),
            "auth.service"
        );
        assert!(short_service_process_label(
            "service - this-is-a-very-long-name.service | service.exe",
            15
        )
        .len()
            <= 15);
    }

'''
    text = text.replace(test_marker, test_marker + test, 1)
write(path, text)

# Remove backend-local platform API implementation and call the runtime owner.
path = "engine/crates/backend/src/main.rs"
text = read(path)
start = text.find('#[cfg(windows)]\nfn apply_service_process_label(label: &str) {')
end_marker = '#[cfg(not(windows))]\nfn apply_service_process_label(_label: &str) {}\n\n'
if start >= 0:
    end = text.find(end_marker, start)
    if end < 0:
        raise SystemExit("missing backend process-label block end")
    text = text[:start] + text[end + len(end_marker):]
text = text.replace(
    "        apply_service_process_label(&service_process_label(&args));",
    "        service_runtime::apply_service_process_label(&service_process_label(&args));",
    1,
)
# The visible label should prefer the actual .service filename exactly as the
# operator sees it on disk, falling back to the declared service name.
old_name = '''    let name = service_flag_value(args, "--service-name")\n        .or_else(|| {\n            service_flag_value(args, "--service-file").and_then(|path| {\n                std::path::Path::new(&path)\n                    .file_stem()\n                    .map(|stem| stem.to_string_lossy().to_string())\n            })\n        })\n        .unwrap_or_else(|| "unknown".into());'''
new_name = '''    let name = service_flag_value(args, "--service-file")\n        .and_then(|path| {\n            std::path::Path::new(&path)\n                .file_name()\n                .map(|name| name.to_string_lossy().to_string())\n        })\n        .or_else(|| service_flag_value(args, "--service-name"))\n        .unwrap_or_else(|| "unknown.service".into());'''
if old_name not in text:
    raise SystemExit("missing backend service_process_label name anchor")
text = text.replace(old_name, new_name, 1)
write(path, text)

# Canonical service.exe removed all temporary alias filesystem paths from the
# manager, so Path/PathBuf are no longer used here.
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace("use std::path::{Path, PathBuf};\n", "", 1)
write(path, text)

# Runtime Image route registration borrows url_path for Router::route while the
# handler also needs an owned path. Clone the cheap String for the handler.
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
old = "build_method_router(route_file.as_ref(), module_program.clone(), url_path),"
new = "build_method_router(route_file.as_ref(), module_program.clone(), url_path.clone()),"
if old not in text and new not in text:
    raise SystemExit("missing Runtime Image route path ownership anchor")
text = text.replace(old, new, 1)
write(path, text)

# Service Runtime ENV propagation uses Arc in Mother supervision. Keep that
# ownership local to backend rather than leaking it through globals.
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
if "use std::sync::Arc;" not in text:
    text = text.replace("use std::process::Stdio;\n", "use std::process::Stdio;\nuse std::sync::Arc;\n", 1)

# Internal Service modes are parent-owned. Remove the historical argv token
# fallback now that service(.exe) exposes only restart control publicly.
token_start = text.find('    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("Service Mother")?')
if token_start < 0:
    raise SystemExit("missing Service Mother token bootstrap anchor")
token_end = text.find("    };\n", token_start)
if token_end < 0:
    raise SystemExit("missing Service Mother token bootstrap end")
token_end += len("    };\n")
text = text[:token_start] + '''    let token = service_runtime::read_parent_bootstrap_secret_if_configured("Service Mother")?\n        .ok_or_else(|| {\n            anyhow::anyhow!(\n                "Service Mother requires inherited parent authentication; command-line tokens are not accepted"\n            )\n        })?;\n''' + text[token_end:]
write(path, text)

path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
token_start = text.find('    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("service host")?')
if token_start < 0:
    raise SystemExit("missing Service worker token bootstrap anchor")
token_end = text.find("    };\n", token_start)
if token_end < 0:
    raise SystemExit("missing Service worker token bootstrap end")
token_end += len("    };\n")
text = text[:token_start] + '''    let token = service_runtime::read_parent_bootstrap_secret_if_configured("service host")?\n        .ok_or_else(|| {\n            anyhow::anyhow!(\n                "Service worker requires inherited parent authentication; command-line tokens are not accepted"\n            )\n        })?;\n''' + text[token_end:]
write(path, text)
