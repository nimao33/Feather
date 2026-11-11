param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

& "$PSScriptRoot\cargo.ps1" build --release @CargoArgs
exit $LASTEXITCODE
