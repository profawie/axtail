# Build the release binary and place it in the repository root as .\axtail.exe
#
#   .\build.ps1                    auto: MSVC if Visual Studio C++ tools exist, otherwise GNU
#   .\build.ps1 -Toolchain gnu     no Visual Studio needed (rustup downloads the GNU toolchain)
#   .\build.ps1 -Toolchain msvc    needs Visual Studio Build Tools with the C++ workload
param(
    [ValidateSet('auto', 'gnu', 'msvc')]
    [string]$Toolchain = 'auto'
)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Test-MsvcTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $false }
    $found = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return [bool]$found
}

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Write-Error 'rustup was not found. Install Rust from https://rustup.rs and open a new PowerShell window.'
}

if ($Toolchain -eq 'auto') {
    $Toolchain = if (Test-MsvcTools) { 'msvc' } else { 'gnu' }
}
$name = "stable-x86_64-pc-windows-$Toolchain"
Write-Host "Building with $name"

# The GNU toolchain ships its own linker, so no Visual Studio or MinGW setup is needed.
rustup toolchain install $name --profile minimal --no-self-update
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo "+$name" build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Copy-Item -Force target\release\axtail.exe .\axtail.exe
Write-Host 'Built .\axtail.exe'
