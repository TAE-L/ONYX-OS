# test-memory.ps1 - M9.6-A6 regression: frame allocator v2.
#   - Kernel mem-test task (through the runtime snapshot, IF=0): fresh-alloc
#     distinctness (bump path), free/realloc set identity + exact LIFO order
#     (intrusive free list), stats consistency, net-outstanding recovery.
#   - [perf] frames: line present (B5 snapshot integration of A6 stats).
#   - M2 paging test still green; no PANIC; fstest still PASSED.
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
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

[void](Wait-LogMarker $log '\[mem\] A6 frame-alloc v2 (PASSED|FAILED)' 90)
[void](Wait-LogMarker $log 'entering interactive mode' 60)
# The boot perf snapshot (which now carries the [perf] frames: line) sleeps
# 3000 APIC-ms; the LAPIC calibration variance across boots (observed
# interval 499160..1409511) can stretch that to tens of real seconds — wait
# generously, then finish.
[void](Wait-LogMarker $log '\[perf\] frames:' 150)
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue
Write-Host '=== M9.6-A6 frame allocator v2 ==='
$content | Where-Object { $_ -match '^\[mem\]' } | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_ -match '^\[perf\] frames' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Where-Object { $_ -match '^\[mem\] A6 frame-alloc v2 PASSED' })) { $fail += 'A6 PASS marker missing' }
if ($content | Where-Object { $_ -match '^\[mem\] FAIL' }) { $fail += '[mem] FAIL line present' }
if (-not ($content | Where-Object { $_ -match '^\[perf\] frames: used \d+ / free \d+ / total \d+' })) { $fail += 'perf frames line missing' }
if (-not ($content | Select-String -SimpleMatch 'paging test PASSED')) { $fail += 'M2 paging regression' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-A6 FRAME ALLOC V2 PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}