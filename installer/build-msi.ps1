# Builds the Feather .msi installer.
#
#   .\installer\build-msi.ps1            # builds the release exe, then the .msi
#   .\installer\build-msi.ps1 -NoBuild   # packages the exe that is already built
#
# Needs the WiX Toolset 7 command line (dotnet tool install --global wix)
# with its UI extension:
#   wix extension add -g WixToolset.UI.wixext/7.0.1
# The .msi lands in target\installer\Feather-<version>-x64.msi.

param(
    [switch]$NoBuild,
    [string]$TargetDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# The version comes from the workspace Cargo.toml.
$version = (Select-String -Path "Cargo.toml" -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
if (-not $TargetDir) { $TargetDir = Join-Path $root "target" }

if (-not $NoBuild) {
    $env:CARGO_TARGET_DIR = $TargetDir
    cargo build --release -p feather-app
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

$exe = Join-Path $TargetDir "release\feather.exe"
if (-not (Test-Path $exe)) { throw "feather.exe not found at $exe" }

$out = Join-Path $root "target\installer"
New-Item -ItemType Directory -Force $out | Out-Null
$msi = Join-Path $out "Feather-$version-x64.msi"

wix build `
    -arch x64 `
    -ext WixToolset.UI.wixext `
    -d "Version=$version" `
    -d "Exe=$exe" `
    -d "Icon=$root\crates\feather-app\feather.ico" `
    -d "License=$root\installer\License.rtf" `
    -o $msi `
    "$root\installer\Product.wxs"
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

Remove-Item (Join-Path $out "*.wixpdb") -ErrorAction SilentlyContinue
Write-Host "Built $msi"
