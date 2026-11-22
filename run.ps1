# Run axtail from the repository root, building it first if needed.
# Usage: .\run.ps1 [axtail arguments], e.g. .\run.ps1 C:\logs --grep ERROR
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 'axtail.exe'
if (-not (Test-Path $exe)) {
    & (Join-Path $PSScriptRoot 'build.ps1')
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& $exe @args
exit $LASTEXITCODE
