<#
.SYNOPSIS
  Runs cargo fmt/clippy/test/build across every workspace in this repo
  — engine, container-runtime, vault, atomic-io — in BOTH dev and
  release profiles side by side, and prints one summary table so you
  can see at a glance what's broken and in which profile.

.DESCRIPTION
  "Side by side" here means: for each workspace, both profiles are run
  and both results are recorded and shown together in the same table
  row — not that they execute as literally concurrent processes (cargo
  builds from the same workspace can't safely share a target dir
  concurrently across two profiles anyway; this runs dev then release,
  sequentially, per workspace, but reports them side by side).

  `module/` isn't included — it's not a Cargo workspace (`.module`
  files aren't compiled), so there's no cargo target to check there.

.EXAMPLE
  .\error-check.ps1
.EXAMPLE
  .\error-check.ps1 --skip-clippy --skip-fmt
.EXAMPLE
  .\error-check.ps1 --only=vault,atomic-io
#>

$ErrorActionPreference = "Continue"   # collect every failure instead of stopping at the first
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path

$SkipFmt = $false
$SkipClippy = $false
$SkipTest = $false
$SkipBuild = $false
$OnlyList = $null
$ShowWarn = $false

foreach ($arg in $args) {
    switch -Regex ($arg) {
        '^--skip-fmt$'      { $SkipFmt = $true; continue }
        '^--skip-clippy$'   { $SkipClippy = $true; continue }
        '^--skip-test$'     { $SkipTest = $true; continue }
        '^--skip-build$'    { $SkipBuild = $true; continue }
        '^--only=(.+)$'     { $OnlyList = $Matches[1] -split ','; continue }
            '^--show-warn(=true)?$' { $ShowWarn = $true; continue }
        '^(--help|-h|-\?)$' { Get-Help $MyInvocation.MyCommand.Path -Full; exit 0 }
        default { Write-Warning "error-check.ps1: unrecognized argument '$arg' — ignoring" }
    }
}

# name -> (path, is a Cargo *workspace* with --workspace support, or a
# lone standalone crate like vault/atomic-io that just takes plain
# cargo commands with no --workspace flag)
$Workspaces = [ordered]@{
    "engine"            = @{ Path = "engine";            IsWorkspace = $true }
    "container-runtime" = @{ Path = "container-runtime"; IsWorkspace = $true }
    "vault"             = @{ Path = "vault";              IsWorkspace = $false }
    "atomic-io"         = @{ Path = "atomic-io";           IsWorkspace = $false }
    "error-client"      = @{ Path = "error-client";        IsWorkspace = $false }
}

if ($OnlyList) {
    $filtered = [ordered]@{}
    foreach ($name in $OnlyList) {
        $trimmed = $name.Trim()
        if ($Workspaces.Contains($trimmed)) {
            $filtered[$trimmed] = $Workspaces[$trimmed]
        } else {
            Write-Warning "error-check.ps1: --only named unknown workspace '$trimmed' — skipping it"
        }
    }
    $Workspaces = $filtered
}

# ---------------------------------------------------------------------------

function Invoke-Check {
    param([string]$WorkDir, [bool]$IsWorkspace, [string[]]$CargoArgs)

    Push-Location $WorkDir
    try {
        $fullArgs = $CargoArgs.Clone()
        if ($IsWorkspace) { $fullArgs += "--workspace" }
        $output = & cargo @fullArgs 2>&1 | Out-String
        $ok = ($LASTEXITCODE -eq 0)
        return @{ Ok = $ok; Output = $output }
    } finally {
        Pop-Location
    }
}

function Test-Profile {
    param([string]$Name, [string]$Path, [bool]$IsWorkspace, [bool]$Release)

    $profileLabel = if ($Release) { "release" } else { "dev" }
    Write-Host ""
    Write-Host "=== $Name [$profileLabel] ===" -ForegroundColor Cyan

    $results = @{}

    if (-not $SkipFmt) {
        Write-Host "  fmt --check ..." -NoNewline
        $r = Invoke-Check -WorkDir $Path -IsWorkspace $IsWorkspace -CargoArgs @("fmt", "--check")
        Write-Host $(if ($r.Ok) { " no error" } else { " FAILED" }) -ForegroundColor $(if ($r.Ok) { "Green" } else { "Red" })
        $results.Fmt = $r
        if (-not $r.Ok -and $ShowWarn) { Write-Host $r.Output }
    }

    if (-not $SkipClippy) {
        Write-Host "  clippy ..." -NoNewline
        $clippyArgs = @("clippy", "--all-targets")
        if ($Release) { $clippyArgs += "--release" }
        $clippyArgs += @("--", "-D", "warnings")
        $r = Invoke-Check -WorkDir $Path -IsWorkspace $IsWorkspace -CargoArgs $clippyArgs
        Write-Host $(if ($r.Ok) { " no error" } else { " FAILED" }) -ForegroundColor $(if ($r.Ok) { "Green" } else { "Red" })
        $results.Clippy = $r
        if (-not $r.Ok -and $ShowWarn) { Write-Host $r.Output }
    }

    if (-not $SkipTest) {
        Write-Host "  test ..." -NoNewline
        $testArgs = @("test")
        if ($Release) { $testArgs += "--release" }
        $r = Invoke-Check -WorkDir $Path -IsWorkspace $IsWorkspace -CargoArgs $testArgs
        Write-Host $(if ($r.Ok) { " no error" } else { " FAILED" }) -ForegroundColor $(if ($r.Ok) { "Green" } else { "Red" })
        $results.Test = $r
        if (-not $r.Ok -and $ShowWarn) { Write-Host $r.Output }
    }

    if (-not $SkipBuild) {
        Write-Host "  build ..." -NoNewline
        $buildArgs = @("build")
        if ($Release) { $buildArgs += "--release" }
        $r = Invoke-Check -WorkDir $Path -IsWorkspace $IsWorkspace -CargoArgs $buildArgs
        Write-Host $(if ($r.Ok) { " no error" } else { " FAILED" }) -ForegroundColor $(if ($r.Ok) { "Green" } else { "Red" })
        $results.Build = $r
        if (-not $r.Ok -and $ShowWarn) { Write-Host $r.Output }
    }

    return $results
}

# ---------------------------------------------------------------------------

$allResults = [ordered]@{}

foreach ($name in $Workspaces.Keys) {
    $info = $Workspaces[$name]
    $path = Join-Path $RepoRoot $info.Path
    if (-not (Test-Path $path)) {
        Write-Warning "error-check.ps1: '$name' expected at $path but it doesn't exist — skipping"
        continue
    }

    $dev = Test-Profile -Name $name -Path $path -IsWorkspace $info.IsWorkspace -Release $false
    $release = Test-Profile -Name $name -Path $path -IsWorkspace $info.IsWorkspace -Release $true

    $allResults[$name] = @{ Dev = $dev; Release = $release }
}

# ---------------------------------------------------------------------------
# Summary table
# ---------------------------------------------------------------------------

function Format-Cell {
    param($StepResult)
    if ($null -eq $StepResult) { return "-" }
    if ($StepResult.Ok) { return "no error" }
    return "FAIL"
}

function Parse-FirstError {
    param([string]$Output)
    if ([string]::IsNullOrEmpty($Output)) { return "(no details)" }
    $lines = $Output -split "`n"
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $ln = $lines[$i].Trim()
        if ($ln -match '^--?>?\s*(.+\.(rs|toml|c|cpp|h|hpp|rsx|rs.in)):(\d+):(\d+)') {
            $file = $Matches[1].Trim()
            $line = $Matches[3]
            $col = $Matches[4]
            # find next non-empty line that looks like an error message
            for ($j = $i+1; $j -lt $lines.Count; $j++) {
                $m = $lines[$j].Trim()
                if (-not [string]::IsNullOrWhiteSpace($m)) {
                    return ("{0}:{1}:{2}: {3}" -f $file, $line, $col, $m)
                }
            }
            return ("{0}:{1}:{2}: (see output)" -f $file, $line, $col)
        }
    }
    # fallback: look for file-like patterns in the output
    foreach ($ln in $lines) {
        if ($ln -match '([\w\./\\-]+\.(rs|toml|c|cpp|h|hpp)):(\d+):(\d+)') {
            return ("{0}:{1}:{2}: (see output)" -f $Matches[1], $Matches[3], $Matches[4])
        }
    }
    # fallback: first line that contains "error" or "warning"
    foreach ($ln in $lines) {
        if ($ln -match '\berror\b' -or $ln -match '\bwarning\b') { return $ln.Trim() }
    }
    return ($Output.Trim().Substring(0,[Math]::Min(200,$Output.Length)) + '...')
}

function Get-ShortError {
    param($StepResult)
    if ($null -eq $StepResult) { return "-" }
    if ($StepResult.Ok) { return "-" }
    return Parse-FirstError $StepResult.Output
}

Write-Host ""
Write-Host "========================================================================" -ForegroundColor Cyan
Write-Host " SUMMARY" -ForegroundColor Cyan
Write-Host "========================================================================" -ForegroundColor Cyan

$rows = @()
$anyFailure = $false

foreach ($name in $allResults.Keys) {
    $entry = $allResults[$name]
    foreach ($profileName in @("Dev", "Release")) {
        $r = $entry[$profileName]
        $row = [ordered]@{
            Workspace = $name
            Profile   = $profileName.ToLower()
            Fmt       = Format-Cell $r.Fmt
            FmtMsg    = Get-ShortError $r.Fmt
            Clippy    = Format-Cell $r.Clippy
            ClippyMsg = Get-ShortError $r.Clippy
            Test      = Format-Cell $r.Test
            TestMsg   = Get-ShortError $r.Test
            Build     = Format-Cell $r.Build
            BuildMsg  = Get-ShortError $r.Build
        }
        $rows += [PSCustomObject]$row
        foreach ($k in @("Fmt", "Clippy", "Test", "Build")) {
            if ($row[$k] -eq "FAIL") { $anyFailure = $true }
        }
    }
}

$rows | Format-Table -AutoSize

if ($anyFailure) {
    Write-Host "One or more checks FAILED — scroll up for the full cargo output of each failing step." -ForegroundColor Red
    Write-Host "Re-run a single failing step directly for a tighter feedback loop, e.g.:" -ForegroundColor Yellow
    Write-Host "  cd vault; cargo clippy --all-targets -- -D warnings" -ForegroundColor Yellow
    exit 1
} else {
    Write-Host "No errors found in any check across all workspaces and profiles." -ForegroundColor Green
    exit 0
}
