from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:180]!r}")
    write(path, text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# service.exe is a canonical sibling execution name for the exact backend
# binary bytes. It is NOT another runtime implementation and there are no
# per-service executable aliases anymore.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/Cargo.toml"
text = read(path)
if "default-run = \"backend\"" not in text:
    text = text.replace(
        "publish.workspace = true\n",
        "publish.workspace = true\ndefault-run = \"backend\"\n",
        1,
    )
if "Win32_System_Console" not in text:
    text += '''\n[target.'cfg(windows)'.dependencies]\nwindows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Console", "Win32_System_Threading"] }\n'''
write(path, text)


# ---------------------------------------------------------------------------
# backend.exe may contain the implementation, but internal service modes are
# accepted only when the image was launched through the canonical service.exe
# sibling name. This keeps backend.exe out of Task Manager's worker list and
# prevents accidentally entering service-host mode through backend.exe.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)

if "fn running_as_service_executable()" not in text:
    marker = '''#[tokio::main]\nasync fn main() {\n'''
    if marker not in text:
        raise SystemExit("missing backend main anchor")
    helper = r'''fn running_as_service_executable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().to_string()))
        .is_some_and(|stem| stem.eq_ignore_ascii_case("service"))
}

fn service_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn service_process_label(args: &[String]) -> String {
    if args.iter().any(|arg| arg == "--service-mother") {
        return "service - mother".into();
    }
    let name = service_flag_value(args, "--service-name")
        .or_else(|| {
            service_flag_value(args, "--service-file").and_then(|path| {
                std::path::Path::new(&path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
            })
        })
        .unwrap_or_else(|| "unknown".into());
    let clean = name
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    format!("service - {clean} | service.exe")
}

#[cfg(windows)]
fn apply_service_process_label(label: &str) {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, SetConsoleTitleW};
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadDescription};

    let wide = label
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        // Thread Description is per-process diagnostic metadata and is safe
        // even when the service inherits the backend's console.
        let _ = SetThreadDescription(GetCurrentThread(), wide.as_ptr());

        // A console title belongs to the console, not the process. Only set it
        // when this service owns the console exclusively; otherwise a worker
        // would rename the backend/PowerShell window it inherited.
        let mut processes = [0u32; 2];
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            let _ = SetConsoleTitleW(wide.as_ptr());
        }
    }
}

#[cfg(not(windows))]
fn apply_service_process_label(_label: &str) {}

'''
    text = text.replace(marker, helper + marker, 1)

old_modes = r'''    if has("--service-mother") {
        if let Err(error) = service_mother::run_child(&args).await {
            eprintln!("fatal service-mother error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    // This must branch before normal backend boot. A user .service process is
    // the same binary in a restricted host mode, not a second mother backend.
    if has("--service-host") {
        if let Err(error) = service_boot::run_host(&args).await {
            eprintln!("fatal service-host error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
'''
if old_modes not in text:
    raise SystemExit("missing backend service mode block")
new_modes = r'''    let service_mother_mode = has("--service-mother");
    let service_host_mode = has("--service-host");
    if service_mother_mode || service_host_mode {
        if !running_as_service_executable() {
            eprintln!(
                "internal service runtime modes must be launched through the sibling service executable"
            );
            std::process::exit(2);
        }
        apply_service_process_label(&service_process_label(&args));
        if service_mother_mode {
            if let Err(error) = service_mother::run_child(&args).await {
                eprintln!("fatal Service Mother error: {error:#}");
                std::process::exit(1);
            }
        } else if let Err(error) = service_boot::run_host(&args).await {
            eprintln!("fatal service worker error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if running_as_service_executable() {
        eprintln!("service executable requires an internal Mother or worker mode");
        std::process::exit(2);
    }
'''
text = text.replace(old_modes, new_modes, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Backend -> Mother: create/verify one canonical service(.exe) sibling whose
# bytes are exactly the running backend binary. No random .runtime/process alias.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
if "use sha2::{Digest, Sha256};" not in text:
    text = text.replace("use anyhow::Context;", "use anyhow::Context;\nuse sha2::{Digest, Sha256};", 1)

# Alias paths are no longer owned temporary files.
text = text.replace("    alias: PathBuf,\n", "")
text = text.replace("        let _ = tokio::fs::remove_file(&self.alias).await;\n", "")
text = text.replace("            let _ = std::fs::remove_file(&self.alias);\n", "")
text = text.replace("        let _ = std::fs::remove_file(&self.alias);\n", "")
text = text.replace("                let alias = process.alias.clone();\n                let _ = std::fs::remove_file(alias);\n", "")
text = text.replace("    let alias = initial.alias.clone();\n", "")
text = text.replace("        alias,\n", "")

# Replace only the old executable-alias materialization block inside spawn_process.
pattern = re.compile(
    r'''    let exe = std::env::current_exe\(\)\.context\("resolve backend executable for Service Mother"\)\?;\n'''
    r'''    let parent = exe\n        \.parent\(\)\n        \.ok_or_else\(\|\| anyhow::anyhow!\("backend executable has no parent directory"\)\)\?;\n'''
    r'''    let process_dir = parent\.join\("\.runtime"\)\.join\("process"\);\n'''
    r'''    std::fs::create_dir_all\(&process_dir\)\?;\n'''
    r'''    let extension = exe\n        \.extension\(\)\n        \.and_then\(\|value\| value\.to_str\(\)\)\n        \.map\(\|value\| format!\("\.\{value\}"\)\)\n        \.unwrap_or_default\(\);\n'''
    r'''    let alias = process_dir\.join\(format!\(\n        "rbe-service-mother-parent-\{\}\{\}",\n        std::process::id\(\),\n        extension\n    \)\);\n'''
    r'''    let _ = std::fs::remove_file\(&alias\);\n'''
    r'''    if std::fs::hard_link\(&exe, &alias\)\.is_err\(\) \{\n'''
    r'''        std::fs::copy\(&exe, &alias\)\n            \.with_context\(\|\| format!\("create Service Mother process alias \{\}", alias\.display\(\)\)\)\?;\n'''
    r'''    \}\n'''
)
replacement = r'''    let exe = std::env::current_exe().context("resolve backend executable for Service Mother")?;
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backend executable has no parent directory"))?;
    let service_exe = ensure_canonical_service_executable(&exe, parent)?;
'''
text, count = pattern.subn(replacement, text, count=1)
if count != 1:
    raise SystemExit(f"failed to replace Service Mother alias block ({count})")

# Source-security hardening leaves Command::new(&alias); point it at canonical service.exe.
if "Command::new(&alias)" not in text:
    raise SystemExit("missing Service Mother Command alias anchor")
text = text.replace("Command::new(&alias)", "Command::new(&service_exe)", 1)
text = text.replace("            let _ = std::fs::remove_file(&alias);\n", "")
text = text.replace("cleanup_failed_spawn(&alias, &mut child)", "cleanup_failed_spawn(&mut child)")
text = text.replace(
    "async fn cleanup_failed_spawn(alias: &Path, child: &mut Child) {\n    let _ = child.kill().await;\n    let _ = child.wait().await;\n    let _ = std::fs::remove_file(alias);\n}",
    "async fn cleanup_failed_spawn(child: &mut Child) {\n    let _ = child.kill().await;\n    let _ = child.wait().await;\n}",
    1,
)

# Give Mother enough information to verify that it really is the canonical image.
command_anchor = '''        .args(["--service-mother", "--launch-separate"])\n        .arg("--service-catalog-fingerprint")\n        .arg(expected_catalog_fingerprint)\n'''
if command_anchor not in text:
    raise SystemExit("missing Service Mother command args anchor")
digest_args = '''        .args(["--service-mother", "--launch-separate"])\n        .arg("--service-catalog-fingerprint")\n        .arg(expected_catalog_fingerprint)\n        .arg("--service-runtime-digest")\n        .arg(file_sha256_hex(&service_exe)?)\n'''
text = text.replace(command_anchor, digest_args, 1)

# Verify the executing service.exe before it receives/uses runtime authority.
run_anchor = '''pub async fn run_child(args: &[String]) -> anyhow::Result<()> {\n'''
if run_anchor not in text:
    raise SystemExit("missing Service Mother run_child anchor")
verify_child = r'''pub async fn run_child(args: &[String]) -> anyhow::Result<()> {
    let expected_runtime_digest = flag_value(args, "--service-runtime-digest")
        .ok_or_else(|| anyhow::anyhow!("Service Mother requires parent runtime image digest"))?;
    let current_exe = std::env::current_exe().context("resolve Service Mother executable")?;
    let actual_runtime_digest = file_sha256_hex(&current_exe)?;
    if !expected_runtime_digest.eq_ignore_ascii_case(&actual_runtime_digest) {
        anyhow::bail!(
            "Service Mother executable digest mismatch; refusing unverified service runtime"
        );
    }
'''
text = text.replace(run_anchor, verify_child, 1)
text = text.replace("backend --service-mother requires", "service --service-mother requires")

# Helpers inserted before spawn_process.
spawn_anchor = '''async fn spawn_process(\n'''
if spawn_anchor not in text:
    raise SystemExit("missing Service Mother spawn_process anchor")
helpers = r'''fn service_executable_name() -> &'static str {
    if cfg!(windows) {
        "service.exe"
    } else {
        "service"
    }
}

fn file_sha256_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read runtime executable {}", path.display()))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn ensure_canonical_service_executable(backend: &Path, parent: &Path) -> anyhow::Result<PathBuf> {
    let service = parent.join(service_executable_name());
    let expected = file_sha256_hex(backend)?;
    let valid_existing = service
        .is_file()
        && file_sha256_hex(&service)
            .map(|actual| actual.eq_ignore_ascii_case(&expected))
            .unwrap_or(false);
    if !valid_existing {
        if service.exists() {
            std::fs::remove_file(&service).with_context(|| {
                format!(
                    "replace stale service runtime {}; stop stale service.exe processes first",
                    service.display()
                )
            })?;
        }
        if std::fs::hard_link(backend, &service).is_err() {
            std::fs::copy(backend, &service).with_context(|| {
                format!("materialize canonical service runtime {}", service.display())
            })?;
        }
    }
    let actual = file_sha256_hex(&service)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        anyhow::bail!(
            "canonical service runtime {} does not match backend executable bytes",
            service.display()
        );
    }
    Ok(service)
}

'''
text = text.replace(spawn_anchor, helpers + spawn_anchor, 1)
write(path, text)


# ---------------------------------------------------------------------------
# Mother -> workers: reuse the currently verified canonical service executable.
# Remove all per-service hardlink/copy aliases and their deletion lifecycle.
# ---------------------------------------------------------------------------
path = "engine/crates/service-runtime/src/manager.rs"
text = read(path)
text = text.replace("    alias: PathBuf,\n", "")
text = text.replace(
    '''                if let Some(process) = service.process.as_ref() {\n                    let _ = std::fs::remove_file(&process.alias);\n                }\n''',
    "",
)
text = text.replace("    let _ = std::fs::remove_file(&process.alias);\n", "")

pattern = re.compile(
    r'''    let exe = std::env::current_exe\(\)\.context\("resolve backend executable"\)\?;\n'''
    r'''    let parent = exe\.parent\(\)\.context\("backend executable has no parent"\)\?;\n'''
    r'''    let dir = parent\.join\("\.runtime/process"\);\n'''
    r'''    std::fs::create_dir_all\(&dir\)\?;\n'''
    r'''    let extension = exe\n        \.extension\(\)\n        \.and_then\(\|value\| value\.to_str\(\)\)\n        \.map\(\|value\| format!\("\.\{value\}"\)\)\n        \.unwrap_or_default\(\);\n'''
    r'''    let alias = dir\.join\(format!\(\n        "rbe-service-\{\}-parent-\{\}\{\}",\n        process_name\(&file\.name\),\n        std::process::id\(\),\n        extension\n    \)\);\n'''
    r'''    let _ = std::fs::remove_file\(&alias\);\n'''
    r'''    if std::fs::hard_link\(&exe, &alias\)\.is_err\(\) \{\n'''
    r'''        std::fs::copy\(&exe, &alias\)\?;\n'''
    r'''    \}\n'''
)
replacement = r'''    let service_exe = std::env::current_exe().context("resolve service runtime executable")?;
    let parent = service_exe
        .parent()
        .context("service runtime executable has no parent")?;
    let stem = service_exe
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !stem.eq_ignore_ascii_case("service") {
        anyhow::bail!(
            "ServiceManager process spawning is restricted to the canonical service executable"
        );
    }
'''
text, count = pattern.subn(replacement, text, count=1)
if count != 1:
    raise SystemExit(f"failed to replace service worker alias block ({count})")
if "Command::new(&alias)" not in text:
    raise SystemExit("missing service worker Command alias anchor")
text = text.replace("Command::new(&alias)", "Command::new(&service_exe)", 1)

# Make the desired service identity visible in the service runtime itself.
args_anchor = '''        .args(["--service-host", "--service-file"])\n        .arg(&file.path)\n'''
if args_anchor not in text:
    raise SystemExit("missing service worker args anchor")
text = text.replace(
    args_anchor,
    '''        .args(["--service-host", "--service-file"])\n        .arg(&file.path)\n        .arg("--service-name")\n        .arg(&file.name)\n''',
    1,
)
text = text.replace("            let _ = std::fs::remove_file(&alias);\n", "")
text = text.replace("cleanup_failed_spawn(&alias, &mut child)", "cleanup_failed_spawn(&mut child)")
text = text.replace("        alias,\n", "")
text = text.replace(
    "async fn cleanup_failed_spawn(alias: &Path, child: &mut Child) {\n    let _ = child.kill().await;\n    let _ = child.wait().await;\n    let _ = std::fs::remove_file(alias);\n}",
    "async fn cleanup_failed_spawn(child: &mut Child) {\n    let _ = child.kill().await;\n    let _ = child.wait().await;\n}",
    1,
)
# Remove the obsolete filename sanitizer and its alias-specific unit test.
text = re.sub(
    r'''\nfn process_name\(name: &str\) -> String \{.*?\n\}\n\nfn random_token''',
    "\nfn random_token",
    text,
    count=1,
    flags=re.S,
)
text = re.sub(
    r'''\n    #\[test\]\n    fn service_process_alias_preserves_distinct_legal_names\(\) \{.*?\n    \}\n''',
    "\n",
    text,
    count=1,
    flags=re.S,
)
write(path, text)


# ---------------------------------------------------------------------------
# Worker diagnostics now name service.exe rather than backend internal modes.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_boot.rs"
text = read(path)
text = text.replace("backend --service-host requires", "service --service-host requires")
write(path, text)


# ---------------------------------------------------------------------------
# Packagers always ship canonical service(.exe) next to backend(.exe). It is
# the exact same built artifact copied under the service execution name.
# ---------------------------------------------------------------------------
path = "build-core.sh"
text = read(path)
anchor = '''    cp "$backend_path" "$out_dir/"\n\n    [ -f "$ENGINE_DIR/settings.json" ]'''
if anchor not in text:
    raise SystemExit("missing build-core backend copy anchor")
text = text.replace(
    anchor,
    '''    cp "$backend_path" "$out_dir/"\n    service_dest="$out_dir/service"; [ "$(get_target_os "$target")" = windows ] && service_dest="$service_dest.exe"\n    cp "$backend_path" "$service_dest"\n\n    [ -f "$ENGINE_DIR/settings.json" ]''',
    1,
)
write(path, text)

path = "build.ps1"
text = read(path)
anchor = '''        if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }\n        Copy-Item $backendPath $outDir -Force\n\n        $settings ='''
if anchor not in text:
    raise SystemExit("missing build.ps1 backend copy anchor")
text = text.replace(
    anchor,
    '''        if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }\n        Copy-Item $backendPath $outDir -Force\n        $serviceName = if ((Get-TargetOs $target) -eq 'windows') { 'service.exe' } else { 'service' }\n        Copy-Item $backendPath (Join-Path $outDir $serviceName) -Force\n\n        $settings =''',
    1,
)
write(path, text)

path = "build-ps5.ps1"
text = read(path)
anchor = '''    Copy-Item $backendPath -Destination $outDir -Force\n    Write-Host "  -> $outDir\\$(Split-Path -Leaf $backendPath)" -ForegroundColor Green\n'''
if anchor not in text:
    raise SystemExit("missing build-ps5 backend copy anchor")
text = text.replace(
    anchor,
    '''    Copy-Item $backendPath -Destination $outDir -Force\n    Write-Host "  -> $outDir\\$(Split-Path -Leaf $backendPath)" -ForegroundColor Green\n    $serviceName = if ((Get-TargetOs $target) -eq "windows") { "service.exe" } else { "service" }\n    Copy-Item $backendPath -Destination (Join-Path $outDir $serviceName) -Force\n    Write-Host "  -> $outDir\\$serviceName" -ForegroundColor Green\n''',
    1,
)
write(path, text)


# ---------------------------------------------------------------------------
# Documentation: process tree and why one canonical service executable is used.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
old = '''Each active service is a separate OS process. RBE launches the same backend executable through a service-specific alias under `.runtime/process/` and starts it with internal `--service-host`, `--service-file`, and authenticated token arguments.\n'''
if old in text:
    new = '''Each active service is a separate OS process. RBE now uses one canonical sibling executable named `service` (`service.exe` on Windows). The Service Mother and every active Service REL worker are separate processes of that same canonical image; per-service executable aliases under `.runtime/process/` are no longer created.\n\nTypical Windows process layout:\n\n```text\nbackend.exe\n└─ service.exe              service - mother\n   ├─ service.exe           service - Auth | service.exe\n   ├─ service.exe           service - Cache | service.exe\n   └─ service.exe           service - Mail | service.exe\n```\n\nThe packaged `service.exe` is byte-for-byte identical to the packaged `backend.exe`; only the execution name and restricted internal mode differ. At development boot RBE materializes/repairs the canonical sibling from its own executable and verifies the SHA-256 before spawning Mother. `backend.exe` refuses `--service-host`/`--service-mother` unless it was launched through the canonical `service.exe` name.\n'''
    text = text.replace(old, new, 1)
write(path, text)

path = "doc/x.service/README.md"
text = read(path)
if "service - mother" not in text:
    text += r'''

## Process identity

Service REL does not run inside `backend.exe`. RBE uses one canonical sibling runtime image named `service` (`service.exe` on Windows): one Mother process plus one separate process for each active service. The same executable file is reused; RBE does not manufacture per-service `rbe-service-*-parent-*` executable aliases.

```text
backend.exe
└─ service.exe              service - mother
   ├─ service.exe           service - Auth | service.exe
   └─ service.exe           service - Cache | service.exe
```

The canonical service image is byte-identical to the backend build artifact and is SHA-256 checked before Mother receives runtime authority. Per-service authentication, liveness, resource limits, and IPC remain independent even though the processes execute the same image file.
'''
write(path, text)
