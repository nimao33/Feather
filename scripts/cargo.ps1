param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$candidates = @(
    "C:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
)

$vcvars = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $vcvars) {
    throw "vcvars64.bat not found. Install Visual Studio Build Tools with the C++ workload."
}

$quoted = foreach ($arg in $CargoArgs) {
    if ($arg -match '\s') { '"' + $arg + '"' } else { $arg }
}
# Cargo writes progress to stderr. With ErrorActionPreference = Stop, letting
# that stream reach PowerShell would abort the script on the first line, so
# merge it into stdout inside cmd.
$command = 'call "' + $vcvars + '" >nul 2>&1 && cargo ' + ($quoted -join ' ') + ' 2>&1'

Push-Location $root
try {
    & cmd.exe /c $command
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
