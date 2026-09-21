# test-para.ps1 - M9.8 parallelism benchmark: does SMP shorten wall-clock work?
#
# Boots the same image twice (kernel `bench::task` runs in both):
#   -smp 1 : the 4 CPU-bound workers serialize on the BSP  -> baseline elapsed
#   -smp 4 : the workers distribute across the cores via task migration +
#            work stealing                                 -> must be faster
#
# Accelerator: WHPX (Windows Hypervisor Platform) is required for a real
# speedup measurement — software TCG time-shares one emulation thread across
# the vCPUs, so each core runs at ~1/N speed and SMP can at best break even.
# Measured here: WHPX 701 -> 156 ms = x4.5 (4 workers / 4 cores). If WHPX is
# not available the harness falls back to TCG (thread=multi) with relaxed
# assertions: correctness + distribution + no-slower-than-serial.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$img  = Join-Path $root 'target\debug\images\bios.img'
$fail = @()

function Invoke-Boot([int]$smp, [int]$seconds, [string]$tag, [string]$accel) {
    $tmp = Join-Path $env:TEMP "onyx-para-$tag"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $image = Join-Path $tmp 'bios.img'
    $log = Join-Path $tmp 'serial.log'
    Copy-Item $img $image -Force
    Remove-Item $log -ErrorAction SilentlyContinue
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    $accArgs = if ($accel -eq 'whpx') { @('-accel', 'whpx') } else { @('-accel', 'tcg,thread=multi') }
    $p = Start-Process -FilePath $qemu `
        -ArgumentList (@('-smp', "$smp") + $accArgs + @('-m', '512M',
                        '-drive', "format=raw,file=$image", '-display', 'none',
                        '-serial', "file:$log", '-no-reboot', '-snapshot')) `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $seconds
    $exited = $p.HasExited
    if (-not $exited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 600
    return [pscustomobject]@{ Log = (Get-Content $log -ErrorAction SilentlyContinue); Exited = $exited }
}

function Parse-Done([string[]]$lines) {
    # "[para] bench done elapsed=156 ms ok=1 cpus=[3,0,0,0]"
    foreach ($l in $lines) {
        if ($l -match '\[para\] bench done elapsed=(\d+) ms ok=(\d+) cpus=\[([0-9,]+)\]') {
            return [pscustomobject]@{
                Elapsed = [long]$Matches[1]
                Ok      = [int]$Matches[2]
                Cpus    = $Matches[3].Split(',') | ForEach-Object { [int]$_ }
            }
        }
    }
    return $null
}

# Probe: does WHPX work? A quick 1-CPU boot must produce the bench marker.
$accel = 'whpx'
$probe = Invoke-Boot 1 25 'probe' $accel
if ($null -eq (Parse-Done $probe.Log)) {
    Write-Host 'WHPX unavailable or bench missing - falling back to TCG (thread=multi); only relaxed assertions apply'
    $accel = 'tcg'
}

Write-Host "=== M9.8 parallelism benchmark ($accel): -smp 1 (serial baseline) ==="
$r1 = Invoke-Boot 1 30 "1-$accel" $accel
$d1 = Parse-Done $r1.Log
if ($null -eq $d1) { $fail += "[-smp 1][$accel] bench never completed (no done marker)" }
else { Write-Host ("  baseline: {0} ms, ok={1}, cpus=[{2}]" -f $d1.Elapsed, $d1.Ok, ($d1.Cpus -join ',')) }
if ($d1 -and $d1.Ok -ne 1) { $fail += '[-smp 1] worker checksums wrong (ok=0)' }

Write-Host "=== M9.8 parallelism benchmark ($accel): -smp 4 (migration + work stealing) ==="
$r4 = Invoke-Boot 4 40 "4-$accel" $accel
$d4 = Parse-Done $r4.Log
if ($null -eq $d4) { $fail += "[-smp 4][$accel] bench never completed (no done marker)" }
else {
    $dist = ($d4.Cpus | Sort-Object -Unique).Count
    $speedup = if ($d1 -and $d4.Elapsed -gt 0) { [math]::Round($d1.Elapsed / $d4.Elapsed, 2) } else { 0 }
    Write-Host ("  parallel:  {0} ms, ok={1}, cpus=[{2}] (distinct={3}, speedup x{4})" -f
                $d4.Elapsed, $d4.Ok, ($d4.Cpus -join ','), $dist, $speedup)
    if ($d4.Ok -ne 1) { $fail += '[-smp 4] worker checksums wrong (ok=0)' }
    if ($dist -lt 2) { $fail += "[-smp 4] workers ran on only $dist CPU(s) - no distribution" }
    # WHPX = hardware vCPUs: demand a real, close-to-linear win.
    # TCG  = software emulation: time-shared vCPUs can at best break even;
    #        assert only that 4 cores are not materially slower than 1.
    $minSpeedup = if ($accel -eq 'whpx') { 2.0 } else { 0.85 }
    if ($speedup -lt $minSpeedup) {
        $fail += "[-smp 4][$accel] speedup x$speedup < x$minSpeedup (no real parallel gain)"
    }
}

if ($r1.Exited -or $r4.Exited) { $fail += 'QEMU self-exited (triple fault / reset)' }

Write-Host ''
if ($fail.Count -eq 0) {
    Write-Host "RESULT: M9.8 PARALLELISM BENCHMARK PASSED (accel=$accel)"
    exit 0
} else {
    Write-Host 'RESULT: FAILED:'
    foreach ($f in $fail) { Write-Host "  - $f" }
    exit 1
}
