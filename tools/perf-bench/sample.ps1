# orangeDesk performance sampler (D6: budgets are CI gates, not marketing targets)
# Samples the resident process CPU / memory into CSV + JSON summary.
# Threshold enforcement lands once the baseline is established (see thresholds.json).
param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,
    [int]$DurationSec = 30,
    [int]$IntervalMs = 500,
    [string]$OutDir = "./perf-results"
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

if (-not (Test-Path $BinaryPath)) {
    throw "binary not found: $BinaryPath"
}

Write-Host "[perf] starting $BinaryPath for $DurationSec s (sample every $IntervalMs ms)"
$proc = Start-Process -FilePath $BinaryPath -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 1500   # let cold start settle

$rows = [System.Collections.Generic.List[object]]::new()
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$lastCpu = $proc.TotalProcessorTime.TotalMilliseconds
$lastWall = [System.Diagnostics.Stopwatch]::GetTimestamp()

while ($sw.Elapsed.TotalSeconds -lt $DurationSec -and -not $proc.HasExited) {
    Start-Sleep -Milliseconds $IntervalMs
    $proc.Refresh()
    if ($proc.HasExited) { break }

    $nowWall = [System.Diagnostics.Stopwatch]::GetTimestamp()
    $cpuMs = $proc.TotalProcessorTime.TotalMilliseconds
    $cpuPercent = [math]::Round(100 * ($cpuMs - $lastCpu) / [math]::Max(1, ($nowWall - $lastWall) / [System.Diagnostics.Stopwatch]::Frequency / 1000.0), 3)
    $lastCpu = $cpuMs
    $lastWall = $nowWall

    $rows.Add([pscustomobject]@{
        t_ms      = [int]$sw.Elapsed.TotalMilliseconds
        cpu_pct   = $cpuPercent
        working_set_mb = [math]::Round($proc.WorkingSet64 / 1MB, 2)
        private_mb     = [math]::Round($proc.PrivateMemorySize64 / 1MB, 2)
        handle_count   = $proc.HandleCount
    })
}

if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }

$csvPath = Join-Path $OutDir "samples.csv"
$rows | Export-Csv -NoTypeInformation -Path $csvPath

# Summary（对照 §4 预算表；多核机器上 cpu_pct 为全机占比）
$summary = [pscustomobject]@{
    samples        = $rows.Count
    duration_sec   = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    cpu_pct_avg    = [math]::Round((($rows | Measure-Object -Property cpu_pct -Average).Average), 3)
    cpu_pct_max    = [math]::Round((($rows | Measure-Object -Property cpu_pct -Maximum).Maximum), 3)
    ws_mb_max      = [math]::Round((($rows | Measure-Object -Property working_set_mb -Maximum).Maximum), 2)
    ws_mb_last     = $rows[-1].working_set_mb
    handle_count_last = $rows[-1].handle_count
    note           = "baseline collection; gate thresholds pending (tools/perf-bench/thresholds.json)"
}
$summaryPath = Join-Path $OutDir "summary.json"
$summary | ConvertTo-Json | Set-Content -Path $summaryPath

Write-Host "[perf] wrote $csvPath"
Write-Host ($summary | ConvertTo-Json)
