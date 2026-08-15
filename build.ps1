<#
.SYNOPSIS
  Builds RBE (engine + container-runtime) for one or more target
  platforms/architectures. By default, for each target it builds
  container-bin FIRST and embeds its bytes into the engine's
  backend(.exe) (see engine/crates/backend/build.rs), so you end up
  with ONE distributable file per target instead of two — while the
  container runtime still runs as a genuinely separate OS process at
  runtime once extracted (see engine/README.md's "non-negotiable
  exception"). Embedding happens at build time; nothing about how the
  two processes run at runtime changes.

.DESCRIPTION
  Platform flags (pick one or more):
    --build-win10 / --build-win11 / --build-windows   (all three are
        the SAME Rust target, x86_64/aarch64-pc-windows-msvc — Windows
        10 and 11 share one ABI, there's no separate binary to build;
        the split flags exist so you can express intent, not because
        the output differs)
    --build-linux
    --build-macos
    --build-all        builds a reasonable "ship everywhere" default
                        set: windows-x64, linux-x64-gnu, linux-x64-musl,
                        macos-x64, macos-arm64

  Architecture flags (pick one or more; default is the host's own
  architecture if none given):
    --arch-x64          (aliases: --achitect-x64, --architect-x64)
    --arch-x86          (aliases: --achitext-x86, --architect-x86)
    --arch-arm64         (aliases: --arch-arm)
    --arch-armv7

  Other flags:
    --musl              Linux only — build against musl libc instead of
                         glibc (what Alpine and similar minimal distros
                         need; glibc covers the rest — see the NOTES
                         section on why "every type of Linux" maps to
                         this axis, not a distro list)
    --no-embed          build backend and container-bin separately,
                         don't embed one into the other
    --dev-content       copy this repo's actual dev api/ and module/
                         content into dist\<target>\ — by default those
                         folders are created EMPTY, since a
                         distributable build shouldn't bundle this
                         repo's own dev .route/.module files (a
                         deployment supplies its own)
    --debug             debug profile instead of release (default)
    --target=<triple>   bypass the OS/arch flags entirely and build
                         for an exact Rust target triple ("or other")
    --help

.EXAMPLE
  .\build.ps1 --build-win11 --arch-x64
.EXAMPLE
  .\build.ps1 --build-linux --arch-arm64 --musl
.EXAMPLE
  .\build.ps1 --build-macos --arch-arm64
.EXAMPLE
  .\build.ps1 --build-all

.NOTES
  Cross-OS builds (e.g. producing a Linux binary from a Windows host)
  usually need more than `rustup target add` — a real cross-linker.
  This script uses the `cross` tool (https://github.com/cross-rs/cross,
  Docker-based) automatically if it's installed and the target OS
  differs from the host OS; otherwise it falls back to plain
  `cargo build --target` and prints a clear warning that this can fail
  without `cross` for genuine cross-OS targets. Same-OS/different-
  architecture builds generally work with just the rustup target
  installed (this script adds it automatically if missing).

  Output lands in .\dist\<target-triple>\ — both binaries if
  --no-embed, just backend(.exe) (with container-bin embedded inside)
  otherwise.
#>

$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    Write-Error "build.ps1 needs PowerShell 7+ (pwsh) — it uses `$IsWindows/`$IsMacOS and multi-segment Join-Path, neither reliable on Windows PowerShell 5.1. Install pwsh: https://aka.ms/powershell"
    exit 1
}

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path

# ---------------------------------------------------------------------------
# Argument parsing. Reads $args directly (rather than a typed param()
# block) specifically so the `--flag-name` style you'd naturally type
# works without fighting PowerShell's native `-FlagName` binding.
# ---------------------------------------------------------------------------

$BuildWin = $false
$BuildLinux = $false
$BuildMacos = $false
$BuildAll = $false
$Musl = $false
$NoEmbed = $false
$DevContent = $false
$Release = $true
$ArchX64 = $false
$ArchX86 = $false
$ArchArm64 = $false
$ArchArmv7 = $false
$CustomTarget = $null
$ShowHelp = $false
$CacheRemove = $false
$CacheRemoveWin = $false
$CacheRemoveLinux = $false
$CacheRemoveAll = $false
$Distro = $null

foreach ($arg in $args) {
    switch -Regex ($arg) {
        '^--build-win(10|11)?$' { $BuildWin = $true; continue }
        '^--build-windows$'     { $BuildWin = $true; continue }
        '^--build-linux$'       { $BuildLinux = $true; continue }
        '^--build-macos$'       { $BuildMacos = $true; continue }
        '^--build-all$'         { $BuildAll = $true; continue }
        '^--musl$'              { $Musl = $true; continue }
        '^--no-embed$'          { $NoEmbed = $true; continue }
        '^--dev-content$'       { $DevContent = $true; continue }
        '^--debug$'             { $Release = $false; continue }
        '^--(arch|achitect|architect)-?x64$'  { $ArchX64 = $true; continue }
        '^--(arch|achitext|architect)-?x86$'  { $ArchX86 = $true; continue }
        '^--arch-?arm(64)?$'                  { $ArchArm64 = $true; continue }
        '^--arch-?armv?7$'                    { $ArchArmv7 = $true; continue }
        '^--target=(.+)$'       { $CustomTarget = $Matches[1]; continue }
        '^--distro=(.+)$'       { $Distro = $Matches[1]; continue }
        '^--cache-remove$' { $CacheRemove = $true; continue }
        '^--cache-remove-win-cache$' { $CacheRemoveWin = $true; continue }
        '^--cache-remove-linux-cache$' { $CacheRemoveLinux = $true; continue }
        '^--cache-remove-all-cache$' { $CacheRemoveAll = $true; continue }
        '^(--help|-h|-\?)$'     { $ShowHelp = $true; continue }
        default {
            Write-Warning "build.ps1: unrecognized argument '$arg' — ignoring (run with --help for usage)"
        }
    }
}

if ($ShowHelp) {
    Get-Help $MyInvocation.MyCommand.Path -Full
    exit 0
}

if (-not ($BuildWin -or $BuildLinux -or $BuildMacos -or $BuildAll -or $CustomTarget)) {
    Write-Host "No platform flag given — building for the host platform only. Use --help to see all options." -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Target triple resolution
# ---------------------------------------------------------------------------

function Get-TargetsForDistro {
    param([string]$distro)
    if ([string]::IsNullOrEmpty($distro)) { return @() }
    $d = $distro.ToLower().Trim()
    # Parse optional arch suffix: e.g. ubuntu-x64, alpine-arm64
    $arch = $null
    if ($d -match '^(.*?)[-_](x64|x86|arm64|armv7)$') {
        $d = $Matches[1]
        $arch = $Matches[2]
    }

    switch ($d) {
        { $_ -in @('all-linux', 'common') } {
            return @(
                'x86_64-unknown-linux-gnu',
                'x86_64-unknown-linux-musl',
                'i686-unknown-linux-gnu',
                'i686-unknown-linux-musl',
                'aarch64-unknown-linux-gnu',
                'aarch64-unknown-linux-musl',
                'armv7-unknown-linux-gnueabihf',
                'armv7-unknown-linux-musleabihf'
            )
        }

        { $_ -in @('ubuntu', 'debian', 'linuxmint', 'kali', 'pop', 'elementary', 'zorin', 'deepin') } {
            if ($arch) {
                switch ($arch) {
                    'x64'  { return @('x86_64-unknown-linux-gnu') }
                    'x86'  { return @('i686-unknown-linux-gnu') }
                    'arm64'{ return @('aarch64-unknown-linux-gnu') }
                    'armv7'{ return @('armv7-unknown-linux-gnueabihf') }
                }
            }
            return @('x86_64-unknown-linux-gnu','i686-unknown-linux-gnu','aarch64-unknown-linux-gnu','armv7-unknown-linux-gnueabihf')
        }

        { $_ -in @('fedora', 'centos', 'rhel', 'amazonlinux', 'oraclelinux', 'almalinux', 'rocky', 'rocky-linux') } {
            if ($arch) {
                switch ($arch) {
                    'x64'  { return @('x86_64-unknown-linux-gnu') }
                    'x86'  { return @('i686-unknown-linux-gnu') }
                    'arm64'{ return @('aarch64-unknown-linux-gnu') }
                    'armv7'{ return @('armv7-unknown-linux-gnueabihf') }
                }
            }
            return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu')
        }

        { $_ -in @('arch', 'manjaro', 'artix') } {
            if ($arch -eq 'x86') { return @('i686-unknown-linux-gnu') }
            if ($arch -eq 'arm64'){ return @('aarch64-unknown-linux-gnu') }
            return @('x86_64-unknown-linux-gnu')
        }

        { $_ -in @('opensuse', 'suse', 'tumbleweed', 'sles') } {
            return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu')
        }

        { $_ -in @('alpine', 'musl', 'void-musl') } {
            if ($arch) {
                switch ($arch) {
                    'x64'  { return @('x86_64-unknown-linux-musl') }
                    'x86'  { return @('i686-unknown-linux-musl') }
                    'arm64'{ return @('aarch64-unknown-linux-musl') }
                    'armv7'{ return @('armv7-unknown-linux-musleabihf') }
                }
            }
            return @('x86_64-unknown-linux-musl','aarch64-unknown-linux-musl','i686-unknown-linux-musl','armv7-unknown-linux-musleabihf')
        }

        { $_ -in @('gentoo', 'slackware', 'void', 'nixos', 'guix') } {
            return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu')
        }

        { $_ -in @('raspbian', 'raspios', 'pi', 'raspberry') } {
            if ($arch -eq 'arm64') { return @('aarch64-unknown-linux-gnu') }
            return @('armv7-unknown-linux-gnueabihf')
        }

        { $_ -in @('clear', 'clearlinux') } {
            return @('x86_64-unknown-linux-gnu')
        }

        { $_ -in @('wsl', 'windows-subsystem-for-linux', 'chromeos') } {
            if ($arch -eq 'arm64') { return @('aarch64-unknown-linux-gnu') }
            if ($arch -eq 'x86')   { return @('i686-unknown-linux-gnu') }
            return @('x86_64-unknown-linux-gnu')
        }

        'openwrt' {
            Write-Warning "OpenWRT builds are specialized; prefer --target=... or use a dedicated toolchain"
            return @()
        }

        { $_ -in @('android', 'termux') } {
            Write-Warning "Android/Termux targets require the Android NDK; returning Android triples but builds may fail without proper toolchain"
            if ($arch -eq 'arm64') { return @('aarch64-linux-android') }
            if ($arch -eq 'x86')   { return @('i686-linux-android') }
            if ($arch -eq 'x64')   { return @('x86_64-linux-android') }
            return @('x86_64-linux-android','aarch64-linux-android','i686-linux-android','armv7-linux-androideabi')
        }

        default {
            if ($d -match '^[a-z0-9_\-]+-[a-z0-9_\-]+-[a-z0-9_\-]+') { return @($d) }
            Write-Warning ("Unknown distro alias '{0}' — ignoring" -f $distro)
            return @()
        }
    }
}

$targets = New-Object System.Collections.Generic.List[string]

if ($CustomTarget) {
    $targets.Add($CustomTarget)
} elseif ($BuildAll) {
    # Build Windows + a wide cross-section of common Linux server triples
    $targets.Add("x86_64-pc-windows-msvc")
    $targets.Add("x86_64-unknown-linux-gnu")
    $targets.Add("x86_64-unknown-linux-musl")
    $targets.Add("i686-unknown-linux-gnu")
    $targets.Add("i686-unknown-linux-musl")
    $targets.Add("aarch64-unknown-linux-gnu")
    $targets.Add("aarch64-unknown-linux-musl")
    $targets.Add("armv7-unknown-linux-gnueabihf")
    $targets.Add("armv7-unknown-linux-musleabihf")
    # macOS as part of a broad "build all"
    $targets.Add("x86_64-apple-darwin")
    $targets.Add("aarch64-apple-darwin")

    # If we're not on macOS, skip adding Apple targets to avoid failed cross-link attempts
    if (-not $IsMacOS) {
        $targets = [System.Collections.Generic.List[string]]($targets | Where-Object { $_ -notlike '*apple-darwin' })
        Write-Host "Info: host is not macOS — macOS targets removed from --build-all" -ForegroundColor Yellow
    }
} else {
    $arches = Get-RequestedArches
    if ($BuildWin)   { foreach ($a in $arches) { $targets.Add((Resolve-Target "windows" $a $false)) } }
    if ($BuildLinux) { foreach ($a in $arches) { $targets.Add((Resolve-Target "linux" $a $Musl)) } }
    if ($BuildMacos) { foreach ($a in $arches) { $targets.Add((Resolve-Target "macos" $a $false)) } }
    if (-not ($BuildWin -or $BuildLinux -or $BuildMacos)) {
        # No OS flag at all — host OS, requested (or host) arches.
        $hostOs = if ($IsWindows) { "windows" } elseif ($IsMacOS) { "macos" } else { "linux" }
        foreach ($a in $arches) { $targets.Add((Resolve-Target $hostOs $a $Musl)) }
    }
}

Write-Host "Targets: $($targets -join ', ')" -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# Distro/alias mapping for common Linux variants
# ---------------------------------------------------------------------------
function Get-TargetsForDistro {
    param([string]$distro)
    if ([string]::IsNullOrEmpty($distro)) { return @() }
    $d = $distro.ToLower().Trim()
    # Parse optional arch suffix: e.g. ubuntu-x64, alpine-arm64
    $arch = $null
    if ($d -match '^(.*?)[-_](x64|x86|arm64|armv7)$') { $d = $Matches[1]; $arch = $Matches[2] }

    switch ($d) {
        { $_ -in @('all-linux', 'common') } {
            return @(
                'x86_64-unknown-linux-gnu','x86_64-unknown-linux-musl',
                'i686-unknown-linux-gnu','i686-unknown-linux-musl',
                'aarch64-unknown-linux-gnu','aarch64-unknown-linux-musl',
                'armv7-unknown-linux-gnueabihf','armv7-unknown-linux-musleabihf'
            )
        }
        { $_ -in @('ubuntu', 'debian', 'linuxmint', 'kali', 'pop', 'elementary', 'zorin', 'deepin') } {
            if ($arch) { switch ($arch) { 'x64' { return @('x86_64-unknown-linux-gnu') } 'x86' { return @('i686-unknown-linux-gnu') } 'arm64' { return @('aarch64-unknown-linux-gnu') } 'armv7' { return @('armv7-unknown-linux-gnueabihf') } } }
            return @('x86_64-unknown-linux-gnu','i686-unknown-linux-gnu','aarch64-unknown-linux-gnu','armv7-unknown-linux-gnueabihf')
        }
        { $_ -in @('fedora', 'centos', 'rhel', 'amazonlinux', 'oraclelinux', 'almalinux', 'rocky', 'rocky-linux') } {
            if ($arch) { switch ($arch) { 'x64' { return @('x86_64-unknown-linux-gnu') } 'x86' { return @('i686-unknown-linux-gnu') } 'arm64' { return @('aarch64-unknown-linux-gnu') } 'armv7' { return @('armv7-unknown-linux-gnueabihf') } } }
            return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu')
        }
        { $_ -in @('arch', 'manjaro', 'artix') } { if ($arch -eq 'x86') { return @('i686-unknown-linux-gnu') } if ($arch -eq 'arm64') { return @('aarch64-unknown-linux-gnu') } return @('x86_64-unknown-linux-gnu') }
        { $_ -in @('opensuse', 'suse', 'tumbleweed', 'sles') } { return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu') }
        { $_ -in @('alpine', 'musl', 'void-musl') } { if ($arch) { switch ($arch) { 'x64' { return @('x86_64-unknown-linux-musl') } 'x86' { return @('i686-unknown-linux-musl') } 'arm64' { return @('aarch64-unknown-linux-musl') } 'armv7' { return @('armv7-unknown-linux-musleabihf') } } } return @('x86_64-unknown-linux-musl','aarch64-unknown-linux-musl','i686-unknown-linux-musl','armv7-unknown-linux-musleabihf') }
        { $_ -in @('gentoo', 'slackware', 'void', 'nixos', 'guix') } { return @('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu') }
        { $_ -in @('raspbian', 'raspios', 'pi', 'raspberry') } { if ($arch -eq 'arm64') { return @('aarch64-unknown-linux-gnu') } return @('armv7-unknown-linux-gnueabihf') }
        { $_ -in @('clear', 'clearlinux') } { return @('x86_64-unknown-linux-gnu') }
        { $_ -in @('wsl', 'windows-subsystem-for-linux', 'chromeos') } { if ($arch -eq 'arm64') { return @('aarch64-unknown-linux-gnu') } if ($arch -eq 'x86') { return @('i686-unknown-linux-gnu') } return @('x86_64-unknown-linux-gnu') }
        'openwrt' { Write-Warning "OpenWRT builds are specialized; prefer --target=... or use a dedicated toolchain"; return @() }
        { $_ -in @('android', 'termux') } {
            Write-Warning "Android/Termux targets require the Android NDK; returning Android triples but builds may fail without proper toolchain"
            if ($arch -eq 'arm64') { return @('aarch64-linux-android') }
            if ($arch -eq 'x86') { return @('i686-linux-android') }
            if ($arch -eq 'x64') { return @('x86_64-linux-android') }
            return @('x86_64-linux-android','aarch64-linux-android','i686-linux-android','armv7-linux-androideabi')
        }
        default { if ($d -match '^[a-z0-9_\-]+-[a-z0-9_\-]+-[a-z0-9_\-]+') { return @($d) } Write-Warning ("Unknown distro alias '{0}' — ignoring" -f $distro); return @() }
    }
}

if ($Distro) {
    $mapped = Get-TargetsForDistro $Distro
    foreach ($t in $mapped) { if (-not ($targets -contains $t)) { $targets.Add($t) } }
    Write-Host "After distro mapping, targets: $($targets -join ', ')" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------
# Cache removal helpers
# ---------------------------------------------------------------------------
function Get-WorkspaceDirs {
    param()
    $dirs = @()
    $candidates = @('engine','container-runtime','atomic-io','vault')
    foreach ($d in $candidates) {
        $p = Join-Path $RepoRoot $d
        if (Test-Path (Join-Path $p 'Cargo.toml')) { $dirs += $p }
    }
    return $dirs
}

function Do-CacheRemove {
    param([bool]$All, [string[]]$Triples)
    $workspaces = Get-WorkspaceDirs
    if ($workspaces.Count -eq 0) { Write-Host "No Rust workspace dirs found to clean" -ForegroundColor Yellow; return }
    foreach ($w in $workspaces) {
        Write-Host "Cleaning workspace: $w" -ForegroundColor Cyan
        Push-Location $w
        try {
            if ($All) {
                Write-Host "  cargo clean" -ForegroundColor DarkGray
                & cargo clean
            } else {
                foreach ($t in $Triples) {
                    Write-Host "  cargo clean --target $t" -ForegroundColor DarkGray
                    & cargo clean --target $t
                }
            }
        } catch {
            Write-Warning ("cargo clean failed in {0}: {1}" -f $w, $_)
        } finally {
            Pop-Location
        }
    }
}

# If any cache-remove flag is present, perform cleaning before build.
if ($CacheRemove -or $CacheRemoveWin -or $CacheRemoveLinux -or $CacheRemoveAll) {
    if ($CacheRemove -or $CacheRemoveAll) {
        Do-CacheRemove -All $true -Triples @()
    } else {
        $triples = @()
        if ($CacheRemoveWin) {
            $triples += 'x86_64-pc-windows-msvc','i686-pc-windows-msvc','aarch64-pc-windows-msvc'
        }
        if ($CacheRemoveLinux) {
            $triples += 'x86_64-unknown-linux-gnu','x86_64-unknown-linux-musl','i686-unknown-linux-gnu','i686-unknown-linux-musl','aarch64-unknown-linux-gnu','aarch64-unknown-linux-musl','armv7-unknown-linux-gnueabihf','armv7-unknown-linux-musleabihf'
        }
        if ($triples.Count -gt 0) { Do-CacheRemove -All $false -Triples $triples }
    }
    # If user only wanted cache removal (no build targets specified), exit now.
    if (-not ($BuildWin -or $BuildLinux -or $BuildMacos -or $BuildAll -or $CustomTarget)) {
        Write-Host "Cache removal complete." -ForegroundColor Green
        exit 0
    }
}

# ---------------------------------------------------------------------------
# Build helpers
# ---------------------------------------------------------------------------

$HostOsName = if ($IsWindows) { "windows" } elseif ($IsMacOS) { "macos" } else { "linux" }
# Detect available cross build helpers. Prefer Docker-based `cross`, fall back
# to `cargo-zigbuild` (which uses Zig as the cross linker/sysroot).
$CrossCmd = Get-Command cross -ErrorAction SilentlyContinue
$CargoZigbuildCmd = Get-Command cargo-zigbuild -ErrorAction SilentlyContinue
$ZigCmd = Get-Command zig -ErrorAction SilentlyContinue
if ($CrossCmd) {
    $CrossMode = "cross"
} elseif ($CargoZigbuildCmd) {
    $CrossMode = "cargo-zigbuild"
} else {
    $CrossMode = $null
}
$CrossAvailable = [bool]$CrossMode

function Test-TargetInstalled {
    param([string]$Target)
    $installed = & rustup target list --installed 2>$null
    return $installed -contains $Target
}

function Install-TargetIfMissing {
    param([string]$Target)
    if (-not (Test-TargetInstalled $Target)) {
        Write-Host "  rustup target '$Target' not installed — adding it" -ForegroundColor DarkGray
        & rustup target add $Target
        if ($LASTEXITCODE -ne 0) {
            throw "rustup target add $Target failed (exit $LASTEXITCODE)"
        }
    }
}

function Get-TargetOs {
    param([string]$Target)
    if ($Target -like "*windows*") { return "windows" }
    if ($Target -like "*darwin*")  { return "macos" }
    return "linux"
}

function Invoke-CargoBuild {
    param([string]$Package, [string]$Target, [bool]$IsRelease)

    Install-TargetIfMissing $Target

    $targetOs = Get-TargetOs $Target
    $useCross = ($targetOs -ne $HostOsName) -and $CrossAvailable
    if (($targetOs -ne $HostOsName) -and -not $CrossAvailable) {
        Write-Warning "Cross-OS build ($HostOsName -> $targetOs) without 'cross' or 'cargo-zigbuild' installed — this may fail to link. See: https://github.com/cross-rs/cross and https://github.com/messense/cargo-zigbuild"
    }

    # For cross (Windows -> Linux) builds, ensure Cargo's target dir
    # does not contain spaces (some linker wrappers mishandle spaced
    # paths). Respect an explicitly set CARGO_TARGET_DIR unless it is
    # problematic (contains whitespace). If missing or contains spaces,
    # set a safe per-target directory under %LOCALAPPDATA%.
    #
    # Windows-only: %LOCALAPPDATA% doesn't exist on Linux/macOS, and
    # the "spaced user-profile path" problem this works around is a
    # Windows-specific thing (paths like `C:\Users\John Doe\...`) — on
    # a Linux/macOS host doing a cross-build with CARGO_TARGET_DIR
    # unset (the common default state), this would otherwise try to
    # Join-Path against $null and fail for no reason.
    if ($useCross -and $IsWindows) {
        $currentTargetDir = $env:CARGO_TARGET_DIR
        $needOverride = $false
        if ([string]::IsNullOrEmpty($currentTargetDir)) {
            $needOverride = $true
        } else {
            if ($currentTargetDir -match '\s') { $needOverride = $true }
        }
        if ($needOverride) {
            $safeBase = Join-Path $env:LOCALAPPDATA 'rbe-cargo-targets'
            $safeDir = Join-Path $safeBase ($Target -replace '[^A-Za-z0-9_\-]', '_')
            New-Item -ItemType Directory -Force -Path $safeDir | Out-Null
            $env:CARGO_TARGET_DIR = $safeDir
            Write-Host "  Using safe CARGO_TARGET_DIR=$env:CARGO_TARGET_DIR" -ForegroundColor DarkGray
        }
    }

    # Base cargo args (same for cross/cargo)
    $cargoArgs = @("build", "-p", $Package, "--target", $Target)
    if ($IsRelease) { $cargoArgs += "--release" }

    if ($useCross) {
        if ($CrossMode -eq "cross") {
            $tool = "cross"
            $toolArgs = $cargoArgs
        } elseif ($CrossMode -eq "cargo-zigbuild") {
            # Invoke `cargo zigbuild ...` (cargo-zigbuild expects the
            # build subcommand to be omitted; it acts as the build
            # wrapper itself). Drop the leading `build` token.
            $tool = "cargo"
            if ($cargoArgs.Count -gt 0 -and $cargoArgs[0] -eq "build") {
                if ($cargoArgs.Count -gt 1) {
                    $innerArgs = $cargoArgs[1..($cargoArgs.Count - 1)]
                } else {
                    $innerArgs = @()
                }
            } else {
                $innerArgs = $cargoArgs
            }
            $toolArgs = @("zigbuild") + $innerArgs
        } else {
            $tool = "cargo"
            $toolArgs = $cargoArgs
        }
    } else {
        $tool = "cargo"
        $toolArgs = $cargoArgs
    }

    Write-Host "  $tool $($toolArgs -join ' ')" -ForegroundColor DarkGray
    & $tool @toolArgs
    if ($LASTEXITCODE -ne 0) {
        throw "$tool build of '$Package' for '$Target' failed (exit $LASTEXITCODE)"
    }
}

function Get-BuiltBinaryPath {
    param([string]$WorkspaceDir, [string]$BinName, [string]$Target, [bool]$IsRelease)
    $profileDir = if ($IsRelease) { "release" } else { "debug" }
    $fileName = if ((Get-TargetOs $Target) -eq "windows") { "$BinName.exe" } else { $BinName }
    # If CARGO_TARGET_DIR is set (we may override it for cross builds),
    # artifacts are placed under that directory instead of the workspace's
    # `target/` folder. Prefer the env var when present and non-empty.
    if (-not [string]::IsNullOrEmpty($env:CARGO_TARGET_DIR)) {
        return Join-Path $env:CARGO_TARGET_DIR $Target $profileDir $fileName
    }
    return Join-Path $WorkspaceDir "target" $Target $profileDir $fileName
}

# ---------------------------------------------------------------------------
# Build loop
# ---------------------------------------------------------------------------

$distRoot = Join-Path $RepoRoot "dist"
$engineDir = Join-Path $RepoRoot "engine"
$containerDir = Join-Path $RepoRoot "container-runtime"

foreach ($target in $targets) {
    Write-Host ""
    Write-Host "=== Building for $target ===" -ForegroundColor Green

    $outDir = Join-Path $distRoot $target
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null

    $containerBinPath = $null
    if (-not $NoEmbed) {
        Write-Host "-- container-bin ($target) --" -ForegroundColor Cyan
        Push-Location $containerDir
        try {
            Invoke-CargoBuild -Package "container-bin" -Target $target -IsRelease $Release
            $containerBinPath = Get-BuiltBinaryPath -WorkspaceDir $containerDir -BinName "container-bin" -Target $target -IsRelease $Release
        } finally {
            Pop-Location
        }
    }

    Write-Host "-- backend ($target) --" -ForegroundColor Cyan
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

    if ($NoEmbed -and $containerBinPath -and (Test-Path $containerBinPath)) {
        Copy-Item $containerBinPath -Destination $outDir -Force
        Write-Host "  -> $outDir\$(Split-Path -Leaf $containerBinPath)" -ForegroundColor Green
    }

    # settings.json is a runtime sibling of the binary, never compiled
    # in (see engine/README.md) — copy it so dist\<target>\ is
    # actually runnable, not just the bare exe.
    Copy-Item (Join-Path $engineDir "settings.json") -Destination $outDir -Force -ErrorAction SilentlyContinue

    # api/ and module/ are ALSO runtime siblings, but a distributable
    # build shouldn't bundle this repo's own dev .route/.module files —
    # a deployment target supplies its own. Just create the empty
    # folders (so the layout is obviously right and .route/.module
    # discovery doesn't need to special-case "folder missing") unless
    # -DevContent was passed, which copies this repo's actual dev
    # content instead — useful for smoke-testing a dist build locally
    # against the same routes you've been developing against.
    if ($DevContent) {
        Copy-Item (Join-Path $RepoRoot "api") -Destination $outDir -Recurse -Force -ErrorAction SilentlyContinue
        Copy-Item (Join-Path $RepoRoot "module") -Destination $outDir -Recurse -Force -ErrorAction SilentlyContinue
    } else {
        New-Item -ItemType Directory -Force -Path (Join-Path $outDir "api") | Out-Null
        New-Item -ItemType Directory -Force -Path (Join-Path $outDir "module") | Out-Null
    }

    # Create a simple Linux launch script so users can run the backend
    # with `./launch.sh` inside the dist/<target>/ folder. Contents are
    # minimal and set `RBE_CONTAINER_BIN_PATH` if a sibling container-bin
    # exists.
    try {
        if ((Get-TargetOs $target) -eq "linux") {
            $launchPath = Join-Path $outDir "launch.sh"
            $launchContent = @'
#!/bin/sh
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$DIR/container-bin" ]; then
  export RBE_CONTAINER_BIN_PATH="$DIR/container-bin"
fi
exec "$DIR/backend" "$@"
'@
            $launchContent | Set-Content -Path $launchPath -Encoding UTF8
        }
    } catch {
        Write-Warning ("Failed to create launch script in {0}: {1}" -f $outDir, $_)
    }
}

Write-Host ""
Write-Host "Done. Output in $distRoot" -ForegroundColor Green
