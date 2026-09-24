$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$EngineDir = Join-Path $RepoRoot 'engine'
$ContainerDir = Join-Path $RepoRoot 'container-runtime'
$DistRoot = Join-Path $RepoRoot 'dist'

$BuildSdk = $false
$OnlyBinary = $null
$BuildWin = $false; $BuildLinux = $false; $BuildMacos = $false; $BuildAll = $false
$Musl = $false; $Release = $true
$ArchX64 = $false; $ArchX86 = $false; $ArchArm64 = $false; $ArchArmv7 = $false
$CustomTarget = $null; $ShowHelp = $false

foreach ($arg in $args) {
    switch -Regex ($arg) {
        '^--build-sdk$' { $BuildSdk = $true; continue }
        '^--only-([A-Za-z0-9_-]+)$' {
            if ($OnlyBinary) { throw "Only one --only-<binary> selector may be used." }
            $OnlyBinary = $Matches[1].ToLowerInvariant(); continue
        }
        '^--build-win(10|11)?$|^--build-windows$' { $BuildWin = $true; continue }
        '^--build-linux$' { $BuildLinux = $true; continue }
        '^--build-macos$' { $BuildMacos = $true; continue }
        '^--build-all$' { $BuildAll = $true; continue }
        '^--musl$' { $Musl = $true; continue }
        '^--debug$' { $Release = $false; continue }
        '^--arch-?x64$|^--achitect-?x64$|^--architect-?x64$' { $ArchX64 = $true; continue }
        '^--arch-?x86$|^--achitext-?x86$|^--architect-?x86$' { $ArchX86 = $true; continue }
        '^--arch-?arm(64)?$' { $ArchArm64 = $true; continue }
        '^--arch-?armv?7$' { $ArchArmv7 = $true; continue }
        '^--target=(.+)$' { $CustomTarget = $Matches[1]; continue }
        '^(--help|-help|-h|-\?)$' { $ShowHelp = $true; continue }
        '^--(no-embed|dev-content)$' { continue }
        default { Write-Warning "build-select.ps1: unrecognized argument '$arg' - ignoring" }
    }
}

if ($ShowHelp) {
    Write-Host 'RBE selective/SDK builder'
    Write-Host '  ./build.ps1 --build-sdk [--only-backend|--only-rpx] [platform/arch flags]'
    Write-Host '  ./build.ps1 --only-<binary> [platform/arch flags]'
    Write-Host 'Known binaries: backend, service, cloud-node, container, rpx.'
    exit 0
}
if (-not $BuildSdk -and -not $OnlyBinary) { throw 'Selective builder requires --build-sdk and/or --only-<binary>.' }

$hostOs = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)) { 'windows' } elseif ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)) { 'macos' } else { 'linux' }
$hostArch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) { 'X64' {'x64'} 'X86' {'x86'} 'Arm64' {'arm64'} 'Arm' {'armv7'} default {'x64'} }

function Resolve-Target([string]$Os, [string]$Arch, [bool]$UseMusl) {
    switch ($Os) {
        'windows' { switch ($Arch) { 'x64' {'x86_64-pc-windows-msvc'} 'x86' {'i686-pc-windows-msvc'} 'arm64' {'aarch64-pc-windows-msvc'} default { throw "No Windows target for $Arch" } } }
        'linux' { switch ($Arch) { 'x64' { if ($UseMusl) {'x86_64-unknown-linux-musl'} else {'x86_64-unknown-linux-gnu'} } 'x86' { if ($UseMusl) {'i686-unknown-linux-musl'} else {'i686-unknown-linux-gnu'} } 'arm64' { if ($UseMusl) {'aarch64-unknown-linux-musl'} else {'aarch64-unknown-linux-gnu'} } 'armv7' { if ($UseMusl) {'armv7-unknown-linux-musleabihf'} else {'armv7-unknown-linux-gnueabihf'} } default { throw "No Linux target for $Arch" } } }
        'macos' { switch ($Arch) { 'x64' {'x86_64-apple-darwin'} 'arm64' {'aarch64-apple-darwin'} default { throw "No macOS target for $Arch" } } }
        default { throw "Unknown target OS $Os" }
    }
}
function Get-TargetOs([string]$Target) { if ($Target -like '*windows*') { 'windows' } elseif ($Target -like '*darwin*') { 'macos' } else { 'linux' } }
function Install-Target([string]$Target) { $installed = & rustup target list --installed 2>$null; if ($installed -notcontains $Target) { & rustup target add $Target; if ($LASTEXITCODE -ne 0) { throw "rustup target add $Target failed" } } }
function Get-BinaryPath([string]$Workspace, [string]$Binary, [string]$Target) { $profile = if ($Release) {'release'} else {'debug'}; $name = if ((Get-TargetOs $Target) -eq 'windows') { "$Binary.exe" } else { $Binary }; return Join-Path (Join-Path (Join-Path $Workspace 'target') $Target) (Join-Path $profile $name) }

$arches = @(); if ($ArchX64) {$arches += 'x64'}; if ($ArchX86) {$arches += 'x86'}; if ($ArchArm64) {$arches += 'arm64'}; if ($ArchArmv7) {$arches += 'armv7'}; if ($arches.Count -eq 0) {$arches = @($hostArch)}
$targets = New-Object System.Collections.Generic.List[string]
if ($CustomTarget) { $targets.Add($CustomTarget) }
elseif ($BuildAll) { $targets.AddRange(@('x86_64-pc-windows-msvc','x86_64-unknown-linux-gnu','x86_64-unknown-linux-musl','aarch64-unknown-linux-gnu','aarch64-unknown-linux-musl')); if ($hostOs -eq 'macos') {$targets.AddRange(@('x86_64-apple-darwin','aarch64-apple-darwin'))} }
else { if ($BuildWin) { foreach ($a in $arches) {$targets.Add((Resolve-Target 'windows' $a $false))} }; if ($BuildLinux) { foreach ($a in $arches) {$targets.Add((Resolve-Target 'linux' $a $Musl))} }; if ($BuildMacos) { foreach ($a in $arches) {$targets.Add((Resolve-Target 'macos' $a $false))} }; if (-not ($BuildWin -or $BuildLinux -or $BuildMacos)) { foreach ($a in $arches) {$targets.Add((Resolve-Target $hostOs $a $Musl))} } }

$cross = Get-Command cross -ErrorAction SilentlyContinue
function Invoke-Cargo([string]$WorkingDir, [string[]]$CargoArgs, [string]$Target) {
    Install-Target $Target
    Push-Location $WorkingDir
    try {
        if ((Get-TargetOs $Target) -ne $hostOs -and $cross) { & cross @CargoArgs } else { if ((Get-TargetOs $Target) -ne $hostOs -and -not $cross) { Write-Warning "Cross-OS target $Target requested without cross; the linker may fail." }; & cargo @CargoArgs }
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed for target $Target" }
    } finally { Pop-Location }
}
function Build-WorkspaceBinary([string]$Workspace, [string]$Package, [string]$Binary, [string]$Target) { $a = @('build','-p',$Package,'--bin',$Binary,'--target',$Target); if ($Release) {$a += '--release'}; Invoke-Cargo $Workspace $a $Target; $p = Get-BinaryPath $Workspace $Binary $Target; if (-not (Test-Path $p)) { throw "Expected binary was not produced: $p" }; return $p }
function Build-Standalone([string]$CrateDir, [string]$Binary, [string]$Target) { $manifest = Join-Path $CrateDir 'Cargo.toml'; $a = @('build','--manifest-path',$manifest,'--bin',$Binary,'--target',$Target); if ($Release) {$a += '--release'}; Invoke-Cargo $RepoRoot $a $Target; $p = Get-BinaryPath $CrateDir $Binary $Target; if (-not (Test-Path $p)) { throw "Expected binary was not produced: $p" }; return $p }
function Copy-Binary([string]$Source, [string]$DestinationBase, [string]$Target) { $dest = if ((Get-TargetOs $Target) -eq 'windows') { "$DestinationBase.exe" } else { $DestinationBase }; Copy-Item $Source $dest -Force; Write-Host "  -> $dest" -ForegroundColor Green }

foreach ($target in $targets) {
    if ($target -notmatch '^[A-Za-z0-9._-]+$') { throw "Unsafe target name: $target" }
    Write-Host "`n=== Selective build for $target ===" -ForegroundColor Cyan
    if ($BuildSdk) {
        if ($OnlyBinary -and $OnlyBinary -notin @('backend','rpx','sdk-backend')) { throw "--build-sdk only supports --only-backend or --only-rpx; got --only-$OnlyBinary" }
        $out = Join-Path (Join-Path $DistRoot $target) 'sdk'; New-Item -ItemType Directory -Force -Path $out | Out-Null
        if (-not $OnlyBinary -or $OnlyBinary -in @('backend','sdk-backend')) { Write-Host '-- SDK backend --' -ForegroundColor Cyan; $p = Build-Standalone (Join-Path $EngineDir 'crates/sdk-backend') 'sdk-backend' $target; Copy-Binary $p (Join-Path $out 'backend') $target }
        if (-not $OnlyBinary -or $OnlyBinary -eq 'rpx') { Write-Host '-- RPX --' -ForegroundColor Cyan; $p = Build-Standalone (Join-Path $EngineDir 'crates/rpx') 'rpx' $target; Copy-Binary $p (Join-Path $out 'rpx') $target }
        $meta = @{ format = 1; target = $target; profile = $(if ($Release) {'release'} else {'debug'}); sdk_backend = (-not $OnlyBinary -or $OnlyBinary -in @('backend','sdk-backend')); rpx = (-not $OnlyBinary -or $OnlyBinary -eq 'rpx') } | ConvertTo-Json
        Set-Content -LiteralPath (Join-Path $out 'sdk-build.json') -Value $meta -Encoding UTF8
        continue
    }

    $out = Join-Path $DistRoot $target; New-Item -ItemType Directory -Force -Path $out | Out-Null
    switch ($OnlyBinary) {
        'backend' { $p = Build-WorkspaceBinary $EngineDir 'backend' 'backend' $target; Copy-Binary $p (Join-Path $out 'backend') $target }
        'service' { $p = Build-WorkspaceBinary $EngineDir 'backend' 'service' $target; Copy-Binary $p (Join-Path $out 'service') $target }
        {$_ -in @('cloud-node','cloud_node')} { $p = Build-WorkspaceBinary $EngineDir 'cloud-node' 'cloud_node' $target; Copy-Binary $p (Join-Path $out 'cloud_node') $target }
        {$_ -in @('container','container-bin','container_bin')} { $p = Build-WorkspaceBinary $ContainerDir 'container-bin' 'container-bin' $target; Copy-Binary $p (Join-Path $out 'container') $target }
        'rpx' { $p = Build-Standalone (Join-Path $EngineDir 'crates/rpx') 'rpx' $target; Copy-Binary $p (Join-Path $out 'rpx') $target }
        default { $p = Build-WorkspaceBinary $EngineDir $OnlyBinary $OnlyBinary $target; Copy-Binary $p (Join-Path $out $OnlyBinary) $target }
    }
}
Write-Host "`nDone. Output in $DistRoot" -ForegroundColor Green
