param(
    [string] $Query = "file000",
    [string] $Output = "screenshots\feather.png",
    [string] $Index = "target\demo-index",
    [int] $Entries = 200000
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $resolved = Join-Path $root $Output
    $parent = Split-Path -Parent $resolved
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    & "$PSScriptRoot\cargo.ps1" run --release -- --index $Index --entries $Entries --screenshot $Output --query $Query
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Get-Item $resolved | Select-Object FullName, Length, LastWriteTime | Format-List
}
finally {
    Pop-Location
}
