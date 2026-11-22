# Build the release binary and place it in the repository root as .\axtail.exe
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force target\release\axtail.exe .\axtail.exe
Write-Host 'Built .\axtail.exe'
