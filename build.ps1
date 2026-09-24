$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$SelectMode = $false
foreach ($arg in $args) {
    if ($arg -eq '--build-sdk' -or $arg -match '^--only-[A-Za-z0-9_-]+$') {
        $SelectMode = $true
        break
    }
}

if ($SelectMode) {
    & (Join-Path $RepoRoot 'build-select.ps1') @args
    exit $LASTEXITCODE
}

& (Join-Path $RepoRoot 'build-release.ps1') @args
exit $LASTEXITCODE
