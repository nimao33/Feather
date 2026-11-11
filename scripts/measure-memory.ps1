param(
    [int] $Seconds = 30,
    [string] $ProcessName = "feather*"
)

$ErrorActionPreference = "Stop"
$processes = Get-Process $ProcessName -ErrorAction SilentlyContinue
if (-not $processes) {
    Write-Error "No process matching '$ProcessName' is running. Start Feather first."
}

$samples = Get-Counter "\Process($ProcessName)\Working Set - Private" -SampleInterval 1 -MaxSamples $Seconds
$rows = foreach ($sample in $samples) {
    $sum = ($sample.CounterSamples | Measure-Object -Property CookedValue -Sum).Sum
    [pscustomobject]@{
        Time     = $sample.Timestamp.ToString("HH:mm:ss")
        PrivateMB = [math]::Round($sum / 1MB, 2)
    }
}

$rows | Format-Table -AutoSize
$stats = $rows | Measure-Object -Property PrivateMB -Average -Maximum -Minimum
"average={0:N2} MB  max={1:N2} MB  min={2:N2} MB  samples={3}" -f `
    $stats.Average, $stats.Maximum, $stats.Minimum, $rows.Count
