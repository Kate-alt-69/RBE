<#
.SYNOPSIS
  Secure RBE release builder for backend + standalone container.

.DESCRIPTION
  The container is a mandatory runtime dependency. For every target this script:
    1. builds container-bin first;
    2. exposes that exact artifact as RBE_CONTAINER_BIN_PATH;
    3. supplies RBE_BUILD_ID and a persistent local Ed25519 signing key (or a CI-provided key);
    4. builds backend.exe with the container SHA-256/build-id/target and Ed25519
       signature compiled into the backend;
    5. packages the exact same container artifact at dist\<target>\dep\container.exe
       (or container on Linux).

  There is no editable .sha256 sidecar and no embedded fallback container copy.
  The backend fails closed when dep\container.exe is missing or fails integrity
  verification.

  Local developer builds automatically create/reuse the signing key at:
    %LOCALAPPDATA%\RBE\container-signing.key
  CI/release builds can override it with RBE_CONTAINER_SIGNING_PRIVATE_KEY.
#>

$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'build.ps1 requires PowerShell 7+' }

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$ContainerDir = Join-Path $RepoRoot 'container-runtime'
$EngineDir = Join-Path $RepoRoot 'engine'
$DistRoot = Join-Path $RepoRoot 'dist'

function Ensure-ContainerSigningKey {
    if (-not [string]::IsNullOrWhiteSpace($env:RBE_CONTAINER_SIGNING_PRIVATE_KEY)) {
        return
    }

    $keyDir = if ($env:RBE_CONFIG_HOME) { $env:RBE_CONFIG_HOME } else { Join-Path $env:LOCALAPPDATA 'RBE' }
    $keyPath = Join-Path $keyDir 'container-signing.key'

    if (Test-Path -LiteralPath $keyPath) {
        $existing = (Get-Content -LiteralPath $keyPath -Raw).Trim()
        if ($existing -match '^[0-9a-fA-F]{64}$') {
            $env:RBE_CONTAINER_SIGNING_PRIVATE_KEY = $existing
            Write-Host "Using existing local RBE container signing key: $keyPath" -ForegroundColor DarkGray
            return
        }

        Write-Warning "Local RBE container signing key is invalid; regenerating it."
        Remove-Item -LiteralPath $keyPath -Force -ErrorAction SilentlyContinue
    }

    $openssl = Get-Command openssl -ErrorAction SilentlyContinue
    if (-not $openssl) {
        throw "OpenSSL is required to generate the local RBE container signing key. Install OpenSSL or set RBE_CONTAINER_SIGNING_PRIVATE_KEY explicitly."
    }

    New-Item -ItemType Directory -Force -Path $keyDir | Out-Null
    $key = (& $openssl.Source rand -hex 32 2>$null).Trim()
    if ($LASTEXITCODE -ne 0 -or $key -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'OpenSSL failed to generate a valid 32-byte container signing key.'
    }

    Set-Content -LiteralPath $keyPath -Value $key -NoNewline -Encoding ascii
    $env:RBE_CONTAINER_SIGNING_PRIVATE_KEY = $key
    Write-Host "Generated and saved local RBE container signing key: $keyPath" -ForegroundColor Green
}

Ensure-ContainerSigningKey

$BuildWin = $false; $BuildLinux = $false; $BuildMacos = $false; $BuildAll = $false
$Musl = $false; $NoEmbed = $false; $DevContent = $false; $Release = $true
$ArchX64 = $false; $ArchX86 = $false; $ArchArm64 = $false; $ArchArmv7 = $false
$CustomTarget = $null; $ShowHelp = $false

foreach ($arg in $args) {
    switch -Regex ($arg) {
        '^--build-win(10|11)?$' { $BuildWin = $true; continue }
        '^--build-windows$' { $BuildWin = $true; continue }
        '^--build-linux$' { $BuildLinux = $true; continue }
        '^--build-macos$' { $BuildMacos = $true; continue }
        '^--build-all$' { $BuildAll = $true; continue }
        '^--musl$' { $Musl = $true; continue }
        '^--no-embed$' { $NoEmbed = $true; continue }
        '^--dev-content$' { $DevContent = $true; continue }
        '^--debug$' { $Release = $false; continue }
        '^--arch-?x64$|^--achitect-?x64$|^--architect-?x64$' { $ArchX64 = $true; continue }
        '^--arch-?x86$|^--achitext-?x86$|^--architect-?x86$' { $ArchX86 = $true; continue }
        '^--arch-?arm(64)?$' { $ArchArm64 = $true; continue }
        '^--arch-?armv?7$' { $ArchArmv7 = $true; continue }
        '^--target=(.+)$' { $CustomTarget = $Matches[1]; continue }
        '^(--help|-h|-\?)$' { $ShowHelp = $true; continue }
        default { Write-Warning "build.ps1: unrecognized argument '$arg' — ignoring" }
    }
}
if ($ShowHelp) { Get-Help $MyInvocation.MyCommand.Path -Full; exit 0 }

$hostOs = if ($IsWindows) { 'windows' } elseif ($IsMacOS) { 'macos' } else { 'linux' }
$hostArch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) {
    'X64' { 'x64' }; 'X86' { 'x86' }; 'Arm64' { 'arm64' }; 'Arm' { 'armv7' }; default { 'x64' }
}

function Resolve-Target([string]$Os, [string]$Arch, [bool]$UseMusl) {
    switch ($Os) {
        'windows' { switch ($Arch) { 'x64' {'x86_64-pc-windows-msvc'} 'x86' {'i686-pc-windows-msvc'} 'arm64' {'aarch64-pc-windows-msvc'} 'armv7' {'thumbv7-pc-windows-msvc'} default { throw "No Windows target for $Arch" } } }
        'linux' {
            switch ($Arch) {
                'x64' { if ($UseMusl) {'x86_64-unknown-linux-musl'} else {'x86_64-unknown-linux-gnu'} }
                'x86' { if ($UseMusl) {'i686-unknown-linux-musl'} else {'i686-unknown-linux-gnu'} }
                'arm64' { if ($UseMusl) {'aarch64-unknown-linux-musl'} else {'aarch64-unknown-linux-gnu'} }
                'armv7' { if ($UseMusl) {'armv7-unknown-linux-musleabihf'} else {'armv7-unknown-linux-gnueabihf'} }
                default { throw "No Linux target for $Arch" }
            }
        }
        'macos' { switch ($Arch) { 'x64' {'x86_64-apple-darwin'} 'arm64' {'aarch64-apple-darwin'} default { throw "No macOS target for $Arch" } } }
        default { throw "Unknown target OS $Os" }
    }
}

$arches = @()
if ($ArchX64) { $arches += 'x64' }; if ($ArchX86) { $arches += 'x86' }; if ($ArchArm64) { $arches += 'arm64' }; if ($ArchArmv7) { $arches += 'armv7' }
if ($arches.Count -eq 0) { $arches = @($hostArch) }

$targets = New-Object System.Collections.Generic.List[string]
if ($CustomTarget) {
    $targets.Add($CustomTarget)
} elseif ($BuildAll) {
    $targets.AddRange(@('x86_64-pc-windows-msvc','x86_64-unknown-linux-gnu','x86_64-unknown-linux-musl','i686-unknown-linux-gnu','i686-unknown-linux-musl','aarch64-unknown-linux-gnu','aarch64-unknown-linux-musl','armv7-unknown-linux-gnueabihf','armv7-unknown-linux-musleabihf'))
    if ($IsMacOS) { $targets.AddRange(@('x86_64-apple-darwin','aarch64-apple-darwin')) } else { Write-Host 'Info: host is not macOS — macOS targets removed from --build-all' -ForegroundColor Yellow }
} else {
    if ($BuildWin) { foreach ($a in $arches) { $targets.Add((Resolve-Target 'windows' $a $false)) } }
    if ($BuildLinux) { foreach ($a in $arches) { $targets.Add((Resolve-Target 'linux' $a $Musl)) } }
    if ($BuildMacos) { foreach ($a in $arches) { $targets.Add((Resolve-Target 'macos' $a $false)) } }
    if (-not ($BuildWin -or $BuildLinux -or $BuildMacos)) { foreach ($a in $arches) { $targets.Add((Resolve-Target $hostOs $a $Musl)) } }
}
Write-Host "Targets: $($targets -join ' ')" -ForegroundColor Cyan

$cross = Get-Command cross -ErrorAction SilentlyContinue
$cargoZigbuild = Get-Command cargo-zigbuild -ErrorAction SilentlyContinue

function Get-TargetOs([string]$Target) { if ($Target -like '*windows*') { 'windows' } elseif ($Target -like '*darwin*') { 'macos' } else { 'linux' } }
function Install-Target([string]$Target) { $installed = & rustup target list --installed 2>$null; if ($installed -notcontains $Target) { & rustup target add $Target; if ($LASTEXITCODE -ne 0) { throw "rustup target add $Target failed" } } }
function Get-BinaryPath([string]$Workspace, [string]$Binary, [string]$Target, [bool]$IsRelease) {
    $profile = if ($IsRelease) { 'release' } else { 'debug' }; $name = if ((Get-TargetOs $Target) -eq 'windows') { "$Binary.exe" } else { $Binary }
    if ($env:CARGO_TARGET_DIR) { return Join-Path $env:CARGO_TARGET_DIR $Target $profile $name }
    return Join-Path $Workspace 'target' $Target $profile $name
}
function Invoke-Build([string]$Package, [string]$Target, [bool]$IsRelease) {
    Install-Target $Target
    $args2 = @('build','-p',$Package,'--target',$Target); if ($IsRelease) { $args2 += '--release' }
    $targetOs = Get-TargetOs $Target
    if (($targetOs -ne $hostOs) -and $cross) { & cross @args2 }
    elseif (($targetOs -ne $hostOs) -and $cargoZigbuild) { & cargo zigbuild @($args2[1..($args2.Count-1)]) }
    else { & cargo @args2 }
    if ($LASTEXITCODE -ne 0) { throw "Build failed for $Package / $Target" }
}

foreach ($target in $targets) {
    Write-Host ""; Write-Host "=== Building for $target ===" -ForegroundColor Green
    $outDir = Join-Path $DistRoot $target; $depDir = Join-Path $outDir 'dep'
    New-Item -ItemType Directory -Force -Path $depDir | Out-Null

    Write-Host "-- container-bin ($target) --" -ForegroundColor Cyan
    Push-Location $ContainerDir
    try { Invoke-Build 'container-bin' $target $Release; $containerPath = Get-BinaryPath $ContainerDir 'container-bin' $target $Release }
    finally { Pop-Location }
    if (-not (Test-Path $containerPath)) { throw "container dependency was not produced: $containerPath" }
    $containerName = if ((Get-TargetOs $target) -eq 'windows') { 'container.exe' } else { 'container' }
    Copy-Item $containerPath (Join-Path $depDir $containerName) -Force

    Write-Host "-- backend ($target) --" -ForegroundColor Cyan
    $env:RBE_CONTAINER_BIN_PATH = $containerPath
    if ([string]::IsNullOrWhiteSpace($env:RBE_BUILD_ID)) { $env:RBE_BUILD_ID = (& git -C $RepoRoot rev-parse HEAD 2>$null) }
    Push-Location $EngineDir
    try { Invoke-Build 'backend' $target $Release; $backendPath = Get-BinaryPath $EngineDir 'backend' $target $Release }
    finally { Pop-Location; Remove-Item Env:RBE_CONTAINER_BIN_PATH -ErrorAction SilentlyContinue }
    if (-not (Test-Path $backendPath)) { throw "backend was not produced: $backendPath" }
    Copy-Item $backendPath $outDir -Force

    $settings = Join-Path $EngineDir 'settings.json'; if (Test-Path $settings) { Copy-Item $settings $outDir -Force }
    if ($DevContent) { Copy-Item (Join-Path $RepoRoot 'api') $outDir -Recurse -Force -ErrorAction SilentlyContinue; Copy-Item (Join-Path $RepoRoot 'module') $outDir -Recurse -Force -ErrorAction SilentlyContinue }
    else { New-Item -ItemType Directory -Force -Path (Join-Path $outDir 'api') | Out-Null; New-Item -ItemType Directory -Force -Path (Join-Path $outDir 'module') | Out-Null }
    Write-Host "  -> $outDir" -ForegroundColor Green
}

Write-Host ""; Write-Host "Done. Output in $DistRoot" -ForegroundColor Green
