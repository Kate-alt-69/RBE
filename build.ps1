$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$HelpPath = Join-Path $RepoRoot 'build-help.txt'

function Normalize-BuildArgument([string]$Value) {
    switch -Regex ($Value) {
        '^-build-sdk$' { return '--build-sdk' }
        '^-only-([A-Za-z0-9_-]+)$' { return "--only-$($Matches[1])" }
        '^(--cloud-node-only|--build-cloud-node|-CloudNodeOnly)$' { return '--only-cloud-node' }
        default { return $Value }
    }
}

function Normalize-OnlyComponent([string]$Value) {
    switch ($Value.ToLowerInvariant()) {
        'backend' { return 'backend' }
        'service' { return 'service' }
        'cloud-node' { return 'cloud-node' }
        'cloud_node' { return 'cloud-node' }
        'container' { return 'container' }
        'container-bin' { return 'container' }
        'container_bin' { return 'container' }
        'rpx' { return 'rpx' }
        'sdk-backend' { return 'sdk-backend' }
        'sdk_backend' { return 'sdk-backend' }
        default { throw "Unknown --only component '$Value'. Run ./build.ps1 -h for the supported component list." }
    }
}

$NormalizedArgs = New-Object System.Collections.Generic.List[string]
$OnlySeen = $false
$BuildSdk = $false
$HelpRequested = $false

foreach ($raw in $args) {
    $arg = Normalize-BuildArgument $raw
    if ($arg -match '^--only-([A-Za-z0-9_-]+)$') {
        if ($OnlySeen) { throw 'Only one --only-<component> selector may be used.' }
        $OnlySeen = $true
        $component = Normalize-OnlyComponent $Matches[1]
        $NormalizedArgs.Add("--only-$component")
        continue
    }
    if ($arg -eq '--build-sdk') { $BuildSdk = $true }
    if ($arg -match '^(--help|-help|-h|-\?)$') { $HelpRequested = $true }
    $NormalizedArgs.Add($arg)
}

if ($HelpRequested) {
    if (Test-Path -LiteralPath $HelpPath) {
        Get-Content -LiteralPath $HelpPath
    } else {
        Write-Host 'RBE build help is missing. Expected build-help.txt beside build.ps1.'
    }
    exit 0
}

if ($OnlySeen -and $NormalizedArgs -contains '--only-sdk-backend' -and -not $BuildSdk) {
    throw '--only-sdk-backend is valid only together with --build-sdk.'
}

$SelectMode = $BuildSdk -or $OnlySeen
if ($SelectMode) {
    & (Join-Path $RepoRoot 'build-select.ps1') @NormalizedArgs
    exit $LASTEXITCODE
}

& (Join-Path $RepoRoot 'build-release.ps1') @NormalizedArgs
exit $LASTEXITCODE
