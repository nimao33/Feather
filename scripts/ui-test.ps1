param(
    [string] $Screenshots = "screenshots\ui-test",
    [string] $Index = "target\demo-index",
    [int] $Entries = 200000
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    & "$PSScriptRoot\cargo.ps1" run --release -- --index $Index --entries $Entries --ui-test --screenshots $Screenshots
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host ""
    Get-ChildItem $Screenshots | Select-Object Name, Length | Format-Table
}
finally {
    Pop-Location
}
