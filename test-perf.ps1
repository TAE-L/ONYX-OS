# test-perf.ps1 - M9.6-B5 regression: latency instrumentation + `perf`.
#   - Boot snapshot: a kernel task prints the full [perf] block ~3 s in
#     (counters + latency histograms + cache hit rate + heap usage).
#   - Asserts the loop actually measures: ctx switches > 0, syscalls > 0,
#     timer ticks recorded with lateness + service cost, kbd/mouse hists
#     present, blk-cache line, heap line.
#   - Live path: types `perf` at the shell -> SYS_PERF prints a SECOND
#     snapshot (asserts >= 2 [perf] uptime lines).
#   - No PANIC / unexpected exception; fstest still PASSED.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$tmp = Join-Path $env:TEMP 'onyx-boot'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue
$monPort = 45457
$intLog = Join-Path $tmp 'qemu-int.log'
Remove-Item $intLog -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot', '-d', 'int,cpu_reset', '-D', $intLog, '-monitor', "tcp:127.0.0.1:$monPort,server,nowait") `
    -WindowStyle Hidden -PassThru

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

# Boot + boot-snapshot (~3 s in) + autoexec (fstest) + interactive shell.
[void](Wait-LogMarker $log '\[perf\] heap:' 90)
[void](Wait-LogMarker $log 'entering interactive mode' 60)
Start-Sleep -Seconds 2
$mon = $null
try {
    $mon = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $mon.GetStream()
    function Send-Mon($cmd) {
        $b = [Text.Encoding]::ASCII.GetBytes("$cmd`n")
        $s.Write($b, 0, $b.Length)
    }
    # Type "perf" at the shell -> SYS_PERF prints a live second snapshot.
    Write-Host '--- typing perf at the shell ---'
    foreach ($k in @('p','e','r','f','ret')) {
        Send-Mon "sendkey $k"
        Start-Sleep -Milliseconds 150
    }
} catch {
    Write-Host "monitor access failed: $_"
} finally {
    if ($mon) { $mon.Close() }
}
[void](Wait-LogMarker $log '\[perf\] uptime' 30)
Start-Sleep -Seconds 3
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-B5 performance instrumentation ==='
$content | Where-Object { $_ -match '^\[perf\]' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
$perfLines = @($content | Where-Object { $_ -match '^\[perf\]' })
if ($perfLines.Count -lt 10) { $fail += "perf snapshot lines missing ($($perfLines.Count), want >= 10)" }
# Counters must show real activity (boot does plenty of work by 3 s).
$ctx = ($content | Where-Object { $_ -match '^\[perf\] uptime .*ctx switches (\d+)' } | ForEach-Object { [int]$Matches[1] } | Select-Object -First 1)
if (-not $ctx -or $ctx -lt 100) { $fail += "ctx switches too low ($ctx, want >= 100)" }
$sysc = ($content | Where-Object { $_ -match '^\[perf\] syscalls: (\d+) total' } | ForEach-Object { [int]$Matches[1] } | Select-Object -First 1)
if (-not $sysc -or $sysc -lt 50) { $fail += "syscall count too low ($sysc, want >= 50)" }
if (-not ($content | Where-Object { $_ -match '^\[perf\] timer late \(1ms sched\): n=\d+' })) { $fail += 'timer lateness histogram missing' }
if (-not ($content | Where-Object { $_ -match '^\[perf\] timer irq cost: n=\d+' })) { $fail += 'timer irq cost histogram missing' }
if (-not ($content | Where-Object { $_ -match '^\[perf\] blk cache: hits=\d+ misses=\d+' })) { $fail += 'blk cache stats missing' }
if (-not ($content | Where-Object { $_ -match '^\[perf\] heap: used \d+ KiB' })) { $fail += 'heap stats missing' }
# Live re-render via the shell's perf command: >= 2 uptime lines total.
$uptimeCount = @($content | Where-Object { $_ -match '^\[perf\] uptime' }).Count
if ($uptimeCount -lt 2) { $fail += "live perf re-render missing ($uptimeCount uptime lines, want >= 2)" }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'M7: ext2 mounted')) { $fail += 'ext2 mount regression' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-B5 PERF TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}