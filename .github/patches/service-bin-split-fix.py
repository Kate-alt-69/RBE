from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
primary = ROOT / ".github/patches/service-bin-split.py"
text = primary.read_text(encoding="utf-8")

# Keep the new service entrypoint warning-clean.
text = text.replace(
    "Error Reporter daemon entrypoint.\n\nuse std::sync::Arc;\n\nmod er_recovery;",
    "Error Reporter daemon entrypoint.\n\nmod er_recovery;",
    1,
)

# The generated Rust source wants ordinary escaped quotes/newline, not literal
# backslashes in service_integrity.rs.
text = text.replace(
    r'"pub const EXPECTED_SERVICE_SHA256: &str = \\\\"{expected_service_hash}\\\\";\\\\n"',
    r'"pub const EXPECTED_SERVICE_SHA256: &str = \\"{expected_service_hash}\\";\\n"',
    1,
)

# PS5 has its own spelling/argument-array layout. Handle it directly here and
# remove the legacy heuristic block from the primary transformer.
start = text.find("# PS5 compatibility builder: add binary selection and package service artifact.")
end = text.find("# Documentation now describes the physical process/image split correctly.", start)
if start < 0 or end < 0:
    raise SystemExit("could not isolate primary PS5 transformer")
text = text[:start] + "# PS5 compatibility builder is handled by service-bin-split-fix.py.\n\n" + text[end:]
primary.write_text(text, encoding="utf-8")

ps5 = ROOT / "build-ps5.ps1"
source = ps5.read_text(encoding="utf-8")
old_sig = 'function Invoke-CargoBuild {\n    param([string]$Package, [string]$Target, [bool]$IsRelease)\n'
new_sig = 'function Invoke-CargoBuild {\n    param([string]$Package, [string]$Target, [bool]$IsRelease, [string]$Binary = $null)\n'
if old_sig not in source:
    raise SystemExit("PS5 Invoke-CargoBuild signature anchor changed")
source = source.replace(old_sig, new_sig, 1)
old_args = '    $cargoArgs = @("build", "-p", $Package, "--target", $Target)\n    if ($IsRelease) { $cargoArgs += "--release" }'
new_args = '    $cargoArgs = @("build", "-p", $Package, "--target", $Target)\n    if ($Binary) { $cargoArgs += @("--bin", $Binary) }\n    if ($IsRelease) { $cargoArgs += "--release" }'
if old_args not in source:
    raise SystemExit("PS5 cargo argument anchor changed")
source = source.replace(old_args, new_args, 1)

old_stage = r'''    Write-Host "-- backend ($target) --" -ForegroundColor Cyan
    Push-Location $engineDir
    try {
        if ($containerBinPath -and (Test-Path $containerBinPath)) {
            $env:RBE_CONTAINER_BIN_PATH = $containerBinPath
        } else {
            Remove-Item Env:\RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        }
        Invoke-CargoBuild -Package "backend" -Target $target -IsRelease $Release
        $backendPath = Get-BuiltBinaryPath -WorkspaceDir $engineDir -BinName "backend" -Target $target -IsRelease $Release
    } finally {
        Remove-Item Env:\RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        Pop-Location
    }

    Copy-Item $backendPath -Destination $outDir -Force
    Write-Host "  -> $outDir\$(Split-Path -Leaf $backendPath)" -ForegroundColor Green
    $serviceName = if ((Get-TargetOs $target) -eq "windows") { "service.exe" } else { "service" }
    Copy-Item $backendPath -Destination (Join-Path $outDir $serviceName) -Force
    Write-Host "  -> $outDir\$serviceName" -ForegroundColor Green
'''
new_stage = r'''    Write-Host "-- service ($target) --" -ForegroundColor Cyan
    Remove-Item Env:\RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
    Remove-Item Env:\RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
    Push-Location $engineDir
    try {
        Invoke-CargoBuild -Package "backend" -Target $target -IsRelease $Release -Binary "service"
        $servicePath = Get-BuiltBinaryPath -WorkspaceDir $engineDir -BinName "service" -Target $target -IsRelease $Release
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
            Remove-Item Env:\RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        }
        $env:RBE_SERVICE_BIN_PATH = $servicePath
        Invoke-CargoBuild -Package "backend" -Target $target -IsRelease $Release -Binary "backend"
        $backendPath = Get-BuiltBinaryPath -WorkspaceDir $engineDir -BinName "backend" -Target $target -IsRelease $Release
    } finally {
        Remove-Item Env:\RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue
        Remove-Item Env:\RBE_SERVICE_BIN_PATH -ErrorAction SilentlyContinue
        Pop-Location
    }

    Copy-Item $backendPath -Destination $outDir -Force
    Write-Host "  -> $outDir\$(Split-Path -Leaf $backendPath)" -ForegroundColor Green
    $serviceName = if ((Get-TargetOs $target) -eq "windows") { "service.exe" } else { "service" }
    Copy-Item $servicePath -Destination (Join-Path $outDir $serviceName) -Force
    Write-Host "  -> $outDir\$serviceName" -ForegroundColor Green
'''
if old_stage not in source:
    raise SystemExit("PS5 backend-copy stage anchor changed")
source = source.replace(old_stage, new_stage, 1)
ps5.write_text(source, encoding="utf-8")

print("service split staging corrections applied")
