param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

& "$PSScriptRoot\cargo.ps1" run --release -- @CargoArgs
exit $LASTEXITCODE
