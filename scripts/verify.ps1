param(
    [int] $Entries = 1000000,
    [int] $Iterations = 5,
    [int] $MemorySeconds = 5,
    [switch] $SkipUi,
    [switch] $SkipMemory
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root "target\release\feather.exe"
$index = Join-Path $root "target\index-verify"
$screenshots = Join-Path $root "target\verify-screenshots"
$process = $null
$oldAppData = $null

function Step($name) {
    Write-Host ""
    Write-Host "== $name ==" -ForegroundColor Cyan
}

Push-Location $root
try {
    Step "format"
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed" }

    Step "clippy"
    & "$PSScriptRoot\cargo.ps1" clippy --workspace --all-targets --offline '--' '-D' 'warnings'
    if ($LASTEXITCODE -ne 0) { throw "clippy failed" }

    Step "tests"
    & "$PSScriptRoot\cargo.ps1" test --workspace --all-targets --offline
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }

    Step "release build"
    & "$PSScriptRoot\cargo.ps1" build --release --offline
    if ($LASTEXITCODE -ne 0) { throw "release build failed" }

    Step "index ($Entries entries)"
    if (-not (Test-Path (Join-Path $index "manifest.bin"))) {
        & $app --gen-index --index $index --entries $Entries
        if ($LASTEXITCODE -ne 0) { throw "index generation failed" }
    } else {
        Write-Host "reusing $index"
    }

    Step "benchmark"
    & $app --bench --index $index --iterations $Iterations
    if ($LASTEXITCODE -ne 0) { throw "benchmark failed" }

    if (-not $SkipUi) {
        Step "UI screenshots"
        & $app --index $index --ui-test --screenshots $screenshots
        if ($LASTEXITCODE -ne 0) { throw "UI scenarios failed" }
    }

    if (-not $SkipMemory) {
        Step "idle memory"
        $verifyAppData = Join-Path $root "target\verify-appdata"
        New-Item -ItemType Directory -Force -Path $verifyAppData | Out-Null
        # A fresh profile with setup already done and no roots: the app must
        # be idle, not reading a drive or showing first-run setup. A 5% cap
        # keeps the 1M benchmark at 50,000 rows, the target's fixed list.
        $featherAppData = Join-Path $verifyAppData "Feather"
        New-Item -ItemType Directory -Force -Path $featherAppData | Out-Null
        Set-Content -Path (Join-Path $featherAppData "settings.ini") `
            -Value "version=2`nsetup_done=1`nresults_percent=5`nshow_all_when_empty=1" `
            -Encoding utf8
        $oldAppData = $env:APPDATA
        $env:APPDATA = $verifyAppData
        # The counter below matches every feather process, so a window left
        # open by an earlier step would be summed into this sample. Only this
        # build is stopped; an unrelated feather.exe is left alone.
        $appFull = [System.IO.Path]::GetFullPath($app)
        Get-Process feather* -ErrorAction SilentlyContinue | ForEach-Object {
            try {
                if ($_.Path -eq $appFull) {
                    Stop-Process -Id $_.Id -Force -ErrorAction Stop
                }
            } catch { }
        }
        Start-Sleep -Milliseconds 300
        # NoNewWindow keeps the temporary APPDATA: ShellExecute (the default)
        # would use the shell's environment and load the real profile.
        $process = Start-Process -FilePath $app `
            -ArgumentList "--index", "`"$index`"", "--query", "file000" `
            -NoNewWindow -PassThru
        Start-Sleep -Seconds 2
        $process.Refresh()
        if ($process.HasExited) { throw "feather exited before the memory sample" }

        $samples = Get-Counter "\Process(feather*)\Working Set - Private" `
            -SampleInterval 1 -MaxSamples $MemorySeconds
        $values = foreach ($sample in $samples) {
            ($sample.CounterSamples | Measure-Object -Property CookedValue -Sum).Sum / 1MB
        }
        $stats = $values | Measure-Object -Average -Maximum -Minimum
        "private_working_set_mb average={0:N2} max={1:N2} min={2:N2} samples={3}" -f `
            $stats.Average, $stats.Maximum, $stats.Minimum, $values.Count
        "private_commit_mb={0:N2} working_set_mb={1:N2}" -f `
            ($process.PrivateMemorySize64 / 1MB), ($process.WorkingSet64 / 1MB)
        Stop-Process -Id $process.Id -Force
        $process = $null
        $env:APPDATA = $oldAppData
        $oldAppData = $null
    }

    Write-Host ""
    Write-Host "PASS" -ForegroundColor Green
}
finally {
    if ($null -ne $process) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $oldAppData) {
        $env:APPDATA = $oldAppData
    }
    Pop-Location
}
