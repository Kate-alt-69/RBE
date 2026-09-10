from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing anchor: {label}")
    return text.replace(old, new, 1)


def replace_between(text: str, start: str, end: str, replacement: str, label: str) -> str:
    start_at = text.find(start)
    if start_at < 0:
        raise SystemExit(f"missing start anchor: {label}")
    end_at = text.find(end, start_at)
    if end_at < 0:
        raise SystemExit(f"missing end anchor: {label}")
    return text[:start_at] + replacement + text[end_at:]


# ---------------------------------------------------------------------------
# The Service runtime is a real binary target, not backend.exe under an alias.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/Cargo.toml"
text = read(path)
text = replace_once(
    text,
    '[[bin]]\nname = "backend"\npath = "src/main.rs"\n',
    '[[bin]]\nname = "backend"\npath = "src/main.rs"\n\n[[bin]]\nname = "service"\npath = "src/service_main.rs"\n',
    "backend service bin target",
)
write(path, text)

service_main = r'''//! Standalone RBE Service runtime process.
//!
//! This binary intentionally owns only the `.service` Mother/worker execution
//! entrypoints and operator restart controls. It does not contain the normal
//! backend boot path, HTTP/API server, Vault supervisor, container supervisor,
//! HostBootstrap package provisioning, or Error Reporter daemon entrypoint.

use std::sync::Arc;

mod er_recovery;
mod service_boot;
mod service_control;
mod service_mother;

// er_recovery is shared source with backend for now, but service.exe only needs
// the already-issued in-memory CONTROL key type. Keeping this tiny shim here
// prevents Linux HostBootstrap/keyring/package-manager code from entering the
// Service executable just to decode an inherited capability.
mod host_bootstrap {
    use std::sync::Arc;

    struct ErControlKeyMaterial([u8; 32]);

    impl Drop for ErControlKeyMaterial {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }

    #[derive(Clone)]
    pub struct ErControlKey(Arc<ErControlKeyMaterial>);

    impl ErControlKey {
        pub(crate) fn from_inherited_hex(value: &str) -> anyhow::Result<Self> {
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                anyhow::bail!("ER CONTROL key must be a 32-byte hexadecimal value");
            }
            let decoded = hex::decode(value)?;
            let bytes: [u8; 32] = decoded
                .try_into()
                .map_err(|_| anyhow::anyhow!("ER CONTROL key decoded to the wrong length"))?;
            Ok(Self(Arc::new(ErControlKeyMaterial(bytes))))
        }

        pub(crate) fn to_hex(&self) -> String {
            hex::encode(self.as_bytes())
        }

        pub(crate) fn as_bytes(&self) -> &[u8; 32] {
            &self.0.as_ref().0
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn process_label(args: &[String]) -> String {
    if args.iter().any(|arg| arg == "--service-mother") {
        return "service - mother".into();
    }
    let name = flag_value(args, "--service-file")
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .or_else(|| flag_value(args, "--service-name"))
        .unwrap_or_else(|| "unknown.service".into());
    let clean = name
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    format!("service - {clean} | service.exe")
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match service_control::command_from_args(&args) {
        Ok(Some(command)) => {
            match service_control::submit(&command) {
                Ok(path) => println!("Service restart request queued: {}", path.display()),
                Err(error) => {
                    eprintln!("failed to queue Service restart request: {error:#}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("invalid Service restart command: {error:#}");
            std::process::exit(2);
        }
    }

    let mother = args.iter().any(|arg| arg == "--service-mother");
    let worker = args.iter().any(|arg| arg == "--service-host");
    if mother == worker {
        eprintln!("service executable requires exactly one internal Mother or worker mode");
        std::process::exit(2);
    }

    service_runtime::apply_service_process_label(&process_label(&args));
    let result = if mother {
        service_mother::run_child(&args).await
    } else {
        service_boot::run_host(&args).await
    };
    if let Err(error) = result {
        if mother {
            eprintln!("fatal Service Mother error: {error:#}");
        } else {
            eprintln!("fatal service worker error: {error:#}");
        }
        std::process::exit(1);
    }
}
'''
write("engine/crates/backend/src/service_main.rs", service_main)

# ---------------------------------------------------------------------------
# Bind the separately-built Service artifact into backend at build time.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/build.rs"
text = read(path)
text = replace_once(
    text,
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_BIN_PATH");\n',
    '    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_BIN_PATH");\n    println!("cargo:rerun-if-env-changed=RBE_SERVICE_BIN_PATH");\n',
    "service integrity env rerun",
)
text = replace_once(
    text,
    '    let integrity_dest = Path::new(&out_dir).join("container_integrity.rs");\n    let source = std::env::var("RBE_CONTAINER_BIN_PATH")\n        .ok()\n        .map(PathBuf::from);\n',
    '    let integrity_dest = Path::new(&out_dir).join("container_integrity.rs");\n    let service_integrity_dest = Path::new(&out_dir).join("service_integrity.rs");\n    let source = std::env::var("RBE_CONTAINER_BIN_PATH")\n        .ok()\n        .map(PathBuf::from);\n    let service_source = std::env::var("RBE_SERVICE_BIN_PATH")\n        .ok()\n        .map(PathBuf::from);\n',
    "service integrity source",
)
service_binding = r'''
    let expected_service_hash = match service_source {
        Some(path) if path.is_file() => {
            println!("cargo:rerun-if-changed={}", path.display());
            let hash = sha256_file(&path).unwrap_or_else(|err| {
                panic!(
                    "backend/build.rs: failed to SHA-256 service binary {}: {err}",
                    path.display()
                )
            });
            if std::env::var_os("RBE_BUILD_TRACE").is_some() {
                println!(
                    "cargo:warning=backend: binding standalone service SHA-256 {hash}, build_id {build_id}, target {target}"
                );
            }
            hash
        }
        Some(path) => {
            panic!(
                "backend/build.rs: RBE_SERVICE_BIN_PATH was set to {} but the file does not exist",
                path.display()
            );
        }
        None => String::new(),
    };
    let service_literal = format!(
        "pub const EXPECTED_SERVICE_SHA256: &str = \\"{expected_service_hash}\\";\\n"
    );
    fs::write(&service_integrity_dest, service_literal).unwrap_or_else(|err| {
        panic!("backend/build.rs: failed to write generated service integrity source: {err}")
    });

'''
text = replace_once(
    text,
    '    let source_literal = format!(\n',
    service_binding + '    let source_literal = format!(\n',
    "service integrity generation",
)
write(path, text)

# ---------------------------------------------------------------------------
# backend.exe no longer doubles as service.exe. It only spawns the bound peer.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/main.rs"
text = read(path)
text = replace_once(
    text,
    'mod service_mother;\nmod vault_recovery;\n',
    'mod service_mother;\nmod vault_recovery;\n\nmod service_integrity {\n    include!(concat!(env!("OUT_DIR"), "/service_integrity.rs"));\n}\n',
    "backend service integrity module",
)
text = replace_between(
    text,
    'fn running_as_service_executable() -> bool {\n',
    '#[tokio::main]\n',
    '',
    "remove backend service alias helpers",
)
service_dispatch_start = '    if running_as_service_executable() {\n'
service_dispatch_end = '    if has("--maintenance-notice") {\n'
text = replace_between(
    text,
    service_dispatch_start,
    service_dispatch_end,
    '',
    "remove backend service mode dispatch",
)
text = replace_once(
    text,
    '                service_runtime_env.clone(),\n                er_control_key.clone(),\n            )',
    '                service_runtime_env.clone(),\n                er_control_key.clone(),\n                service_integrity::EXPECTED_SERVICE_SHA256,\n            )',
    "backend passes expected service hash",
)
write(path, text)

# ---------------------------------------------------------------------------
# Backend verifies the true Service artifact and NEVER repairs it from itself.
# ---------------------------------------------------------------------------
path = "engine/crates/backend/src/service_mother.rs"
text = read(path)
text = replace_once(
    text,
    'use std::io::Write;\n',
    'use std::io::{Read, Write};\n',
    "streaming service binary hash import",
)
old_integrity = r'''fn file_sha256_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read runtime executable {}", path.display()))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn ensure_canonical_service_executable(backend: &Path, parent: &Path) -> anyhow::Result<PathBuf> {
    let service = parent.join(service_executable_name());
    let expected = file_sha256_hex(backend)?;
    let valid_existing = service.is_file()
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
                format!(
                    "materialize canonical service runtime {}",
                    service.display()
                )
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
new_integrity = r'''fn file_sha256_hex(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open runtime executable {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read runtime executable {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn ensure_canonical_service_executable(
    backend: &Path,
    parent: &Path,
    expected_service_sha256: &str,
) -> anyhow::Result<PathBuf> {
    if expected_service_sha256.len() != 64
        || !expected_service_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!(
            "backend was built without a valid standalone Service integrity binding"
        );
    }

    let service = parent.join(service_executable_name());
    if !service.is_file() {
        anyhow::bail!(
            "standalone Service runtime {} is missing; backend will not synthesize it from itself",
            service.display()
        );
    }

    let actual = file_sha256_hex(&service)?;
    if !actual.eq_ignore_ascii_case(expected_service_sha256) {
        anyhow::bail!(
            "standalone Service runtime {} failed build-time SHA-256 verification",
            service.display()
        );
    }

    // Regression guard: service.exe must be an independently linked runtime,
    // never backend.exe copied/hard-linked under another filename again.
    let backend_hash = file_sha256_hex(backend)?;
    if actual.eq_ignore_ascii_case(&backend_hash) {
        anyhow::bail!(
            "standalone Service runtime unexpectedly matches backend executable bytes"
        );
    }
    Ok(service)
}
'''
text = replace_once(text, old_integrity, new_integrity, "standalone service integrity verifier")
text = replace_once(
    text,
    '    expected_catalog_fingerprint: &str,\n    runtime_env: &serde_json::Value,\n',
    '    expected_catalog_fingerprint: &str,\n    expected_service_sha256: &str,\n    runtime_env: &serde_json::Value,\n',
    "spawn process service hash argument",
)
text = replace_once(
    text,
    '    let service_exe = ensure_canonical_service_executable(&exe, parent)?;\n',
    '    let service_exe =\n        ensure_canonical_service_executable(&exe, parent, expected_service_sha256)?;\n',
    "spawn verifies standalone service",
)
text = replace_once(
    text,
    'pub async fn spawn(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: Arc<serde_json::Value>,\n    er_control_key: Option<crate::host_bootstrap::ErControlKey>,\n) -> anyhow::Result<ServiceMotherSupervisor> {',
    'pub async fn spawn(\n    settings_path: impl AsRef<Path>,\n    expected_catalog_fingerprint: &str,\n    runtime_env: Arc<serde_json::Value>,\n    er_control_key: Option<crate::host_bootstrap::ErControlKey>,\n    expected_service_sha256: &str,\n) -> anyhow::Result<ServiceMotherSupervisor> {',
    "public service mother spawn hash argument",
)
text = replace_once(
    text,
    '    let expected_catalog_fingerprint = expected_catalog_fingerprint.to_string();\n    let initial = spawn_process(\n        &settings_path,\n        &expected_catalog_fingerprint,\n        runtime_env.as_ref(),\n',
    '    let expected_catalog_fingerprint = expected_catalog_fingerprint.to_string();\n    let expected_service_sha256 = expected_service_sha256.to_string();\n    let initial = spawn_process(\n        &settings_path,\n        &expected_catalog_fingerprint,\n        &expected_service_sha256,\n        runtime_env.as_ref(),\n',
    "initial Mother spawn uses service hash",
)
text = replace_once(
    text,
    '            match spawn_process(\n                &settings_path,\n                &expected_catalog_fingerprint,\n                runtime_env.as_ref(),\n',
    '            match spawn_process(\n                &settings_path,\n                &expected_catalog_fingerprint,\n                &expected_service_sha256,\n                runtime_env.as_ref(),\n',
    "replacement Mother spawn uses service hash",
)
write(path, text)

# ---------------------------------------------------------------------------
# PowerShell 7 builder: build service first, then bind/package those exact bytes.
# ---------------------------------------------------------------------------
path = "build.ps1"
text = read(path)
text = replace_once(
    text,
    '    function Invoke-Build([string]$Package, [string]$Target, [bool]$IsRelease) {\n        Install-Target $Target\n        $args2 = @(\'build\',\'-p\',$Package,\'--target\',$Target); if ($IsRelease) { $args2 += \'--release\' }\n',
    '    function Invoke-Build([string]$Package, [string]$Target, [bool]$IsRelease, [string]$Binary = $null) {\n        Install-Target $Target\n        $args2 = @(\'build\',\'-p\',$Package,\'--target\',$Target); if ($Binary) { $args2 += @(\'--bin\',$Binary) }; if ($IsRelease) { $args2 += \'--release\' }\n',
    "PowerShell binary-select build helper",
)
old_build = r'''        Write-Host "-- backend ($target) --" -ForegroundColor Cyan
        $env:RBE_CONTAINER_BIN_PATH = $containerPath
        if ([string]::IsNullOrWhiteSpace($env:RBE_BUILD_ID)) { $env:RBE_BUILD_ID = (& git -C $RepoRoot rev-parse HEAD 2>$null) }
        Push-Location $EngineDir
        try { Invoke-Build 'backend' $target $Release; $backendPath = Get-BinaryPath $EngineDir 'backend' $target $Release }
        finally { Pop-Location; Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue }
        if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }
        Copy-Item $backendPath $outDir -Force
        $serviceName = if ((Get-TargetOs $target) -eq 'windows') { 'service.exe' } else { 'service' }
        Copy-Item $backendPath (Join-Path $outDir $serviceName) -Force
'''
new_build = r'''        if ([string]::IsNullOrWhiteSpace($env:RBE_BUILD_ID)) { $env:RBE_BUILD_ID = (& git -C $RepoRoot rev-parse HEAD 2>$null) }

        Write-Host "-- service ($target) --" -ForegroundColor Cyan
        Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        Remove-Item Env:RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
        Push-Location $EngineDir
        try { Invoke-Build 'backend' $target $Release 'service'; $servicePath = Get-BinaryPath $EngineDir 'service' $target $Release }
        finally { Pop-Location }
        if (-not (Test-Path $servicePath)) { throw "service runtime was not produced: $servicePath" }

        Write-Host "-- backend ($target) --" -ForegroundColor Cyan
        $env:RBE_CONTAINER_BIN_PATH = $containerPath
        $env:RBE_SERVICE_BIN_PATH = $servicePath
        Push-Location $EngineDir
        try { Invoke-Build 'backend' $target $Release 'backend'; $backendPath = Get-BinaryPath $EngineDir 'backend' $target $Release }
        finally {
            Pop-Location
            Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
            Remove-Item Env:RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
        }
        if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }
        Copy-Item $backendPath $outDir -Force
        $serviceName = if ((Get-TargetOs $target) -eq 'windows') { 'service.exe' } else { 'service' }
        Copy-Item $servicePath (Join-Path $outDir $serviceName) -Force
'''
text = replace_once(text, old_build, new_build, "PowerShell standalone service build/package")
write(path, text)

# ---------------------------------------------------------------------------
# Bash builder mirrors the same artifact ordering and binding.
# ---------------------------------------------------------------------------
path = "build-core.sh"
text = read(path)
text = replace_once(
    text,
    '    local package="$1" target="$2" release="$3"; install_target_if_missing "$target"\n    local target_os; target_os=$(get_target_os "$target")\n    local args=(build -p "$package" --target "$target"); [ "$release" = true ] && args+=(--release)\n',
    '    local package="$1" target="$2" release="$3" binary="${4:-}"; install_target_if_missing "$target"\n    local target_os; target_os=$(get_target_os "$target")\n    local args=(build -p "$package" --target "$target"); [ -n "$binary" ] && args+=(--bin "$binary"); [ "$release" = true ] && args+=(--release)\n',
    "Bash binary-select build helper",
)
old_bash = r'''    echo "-- backend ($target) --" >&2
    export RBE_CONTAINER_BIN_PATH="$container_bin_path"
    export RBE_BUILD_ID="${RBE_BUILD_ID:-$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown-build)}"
    (cd "$ENGINE_DIR" && invoke_cargo_build backend "$target" "$RELEASE")
    unset RBE_CONTAINER_BIN_PATH
    backend_path=$(get_built_binary_path "$ENGINE_DIR" backend "$target" "$RELEASE")
    [ -f "$backend_path" ] || { echo "ERROR: backend artifact missing: $backend_path" >&2; exit 1; }
    cp "$backend_path" "$out_dir/"
    service_dest="$out_dir/service"; [ "$(get_target_os "$target")" = windows ] && service_dest="$service_dest.exe"
    cp "$backend_path" "$service_dest"
'''
new_bash = r'''    export RBE_BUILD_ID="${RBE_BUILD_ID:-$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown-build)}"

    echo "-- service ($target) --" >&2
    unset RBE_CONTAINER_BIN_PATH RBE_SERVICE_BIN_PATH || true
    (cd "$ENGINE_DIR" && invoke_cargo_build backend "$target" "$RELEASE" service)
    service_path=$(get_built_binary_path "$ENGINE_DIR" service "$target" "$RELEASE")
    [ -f "$service_path" ] || { echo "ERROR: service artifact missing: $service_path" >&2; exit 1; }

    echo "-- backend ($target) --" >&2
    export RBE_CONTAINER_BIN_PATH="$container_bin_path"
    export RBE_SERVICE_BIN_PATH="$service_path"
    (cd "$ENGINE_DIR" && invoke_cargo_build backend "$target" "$RELEASE" backend)
    unset RBE_CONTAINER_BIN_PATH RBE_SERVICE_BIN_PATH
    backend_path=$(get_built_binary_path "$ENGINE_DIR" backend "$target" "$RELEASE")
    [ -f "$backend_path" ] || { echo "ERROR: backend artifact missing: $backend_path" >&2; exit 1; }
    cp "$backend_path" "$out_dir/"
    service_dest="$out_dir/service"; [ "$(get_target_os "$target")" = windows ] && service_dest="$service_dest.exe"
    cp "$service_path" "$service_dest"
'''
text = replace_once(text, old_bash, new_bash, "Bash standalone service build/package")
write(path, text)

# ---------------------------------------------------------------------------
# PS5 compatibility builder: add binary selection and package service artifact.
# Keep this tolerant of formatting differences from the PS7 script.
# ---------------------------------------------------------------------------
path = "build-ps5.ps1"
text = read(path)
text = replace_once(
    text,
    'function Invoke-CargoBuild {\n    param([string]$Package, [string]$Target, [bool]$IsRelease)\n',
    'function Invoke-CargoBuild {\n    param([string]$Package, [string]$Target, [bool]$IsRelease, [string]$Binary = $null)\n',
    "PS5 binary-select helper signature",
)
# All helper implementations construct an argument array containing build/-p/target.
needle = '$args = @("build", "-p", $Package, "--target", $Target)'
if needle in text:
    text = text.replace(
        needle,
        needle + '\n    if ($Binary) { $args += @("--bin", $Binary) }',
        1,
    )
else:
    needle = "$args = @('build', '-p', $Package, '--target', $Target)"
    if needle in text:
        text = text.replace(
            needle,
            needle + "\n    if ($Binary) { $args += @('--bin', $Binary) }",
            1,
        )
    else:
        raise SystemExit("missing anchor: PS5 cargo argument array")

# Replace the old backend-copy packaging block by locating the backend stage and
# the settings-copy boundary. This avoids depending on minor PS5 formatting.
start = text.find('    Write-Host "-- backend ($target) --" -ForegroundColor Cyan')
if start < 0:
    raise SystemExit("missing anchor: PS5 backend build stage")
end_candidates = [
    text.find('    # Copy settings', start),
    text.find('    $settings', start),
    text.find('    if (Test-Path (Join-Path $engineDir "settings.json"))', start),
]
end_candidates = [value for value in end_candidates if value >= 0]
if not end_candidates:
    raise SystemExit("missing anchor: PS5 post-build settings stage")
end = min(end_candidates)
old_stage = text[start:end]
# Preserve variable naming used by this legacy script ($engineDir/$outDir).
new_stage = r'''    Write-Host "-- service ($target) --" -ForegroundColor Cyan
    Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
    Remove-Item Env:RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
    Push-Location $engineDir
    try {
        Invoke-CargoBuild "backend" $target $Release "service"
        $servicePath = Get-BuiltBinaryPath $engineDir "service" $target $Release
    } finally {
        Pop-Location
    }
    if (-not (Test-Path $servicePath)) { throw "service runtime was not produced: $servicePath" }

    Write-Host "-- backend ($target) --" -ForegroundColor Cyan
    Push-Location $engineDir
    try {
        if ($containerBinPath -and (Test-Path $containerBinPath)) {
            $env:RBE_CONTAINER_BIN_PATH = $containerBinPath
        } else {
            Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        }
        $env:RBE_SERVICE_BIN_PATH = $servicePath
        Invoke-CargoBuild "backend" $target $Release "backend"
        $backendPath = Get-BuiltBinaryPath $engineDir "backend" $target $Release
    } finally {
        Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        Remove-Item Env:RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
        Pop-Location
    }
    if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }
    Copy-Item $backendPath -Destination $outDir -Force
    $serviceName = if ((Get-TargetOs $target) -eq "windows") { "service.exe" } else { "service" }
    Copy-Item $servicePath -Destination (Join-Path $outDir $serviceName) -Force

'''
text = text[:start] + new_stage + text[end:]
write(path, text)

# ---------------------------------------------------------------------------
# Documentation now describes the physical process/image split correctly.
# ---------------------------------------------------------------------------
path = "docs/service-runtime.md"
text = read(path)
old_docs = "The packaged `service.exe` is byte-for-byte identical to the packaged `backend.exe`; only the execution name and restricted internal mode differ. At development boot RBE materializes/repairs the canonical sibling from its own executable and verifies the SHA-256 before spawning Mother. `backend.exe` refuses `--service-host`/`--service-mother` unless it was launched through the canonical `service.exe` name."
new_docs = "The packaged `service.exe` is now a separately linked executable target. It contains the Service Mother/worker entrypoints, `.service` compiler/executor path, Service Fabric IPC, Runtime ENV bootstrap, CONTROL-ER recovery client, and restart controls, but it does not expose the normal backend API/Vault/container/HostBootstrap/ER-daemon boot path. The release builder compiles `service` first, passes its exact artifact to the backend build, and backend embeds that Service SHA-256. At runtime backend requires the sibling `service.exe`/`service` to match that build-time digest and explicitly refuses a Service image whose bytes are identical to backend. Backend never repairs, copies, or hard-links itself into the Service path."
text = replace_once(text, old_docs, new_docs, "service runtime process-model docs")
write(path, text)

print("standalone service executable split staged")
