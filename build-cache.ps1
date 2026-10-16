# Share the Python resolver with Bash so all entrypoints honor Cargo config,
# explicit target directories and restored Render caches in the same order.
function Initialize-RbeBuildCache([string]$Root) {
	$python = Get-Command python3 -ErrorAction SilentlyContinue
	if (-not $python) { $python = Get-Command python -ErrorAction SilentlyContinue }
	if (-not $python) { throw 'Python 3.9+ is required to resolve the Cargo build cache.' }
	$target = & $python.Source (Join-Path $Root 'build-cache.py') $Root
	if ($LASTEXITCODE -ne 0 -or -not $target) { throw 'Cannot resolve the Cargo build cache.' }
	$env:CARGO_TARGET_DIR = [string]$target
}
