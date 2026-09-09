# test-block.ps1 - M9.6-A5 regression: block cache + multi-sector I/O.
#   The boot-time blk_test exercises: cold-vs-hot read speedup of
#   /SHELL.ELF, data equality across the cache, read-ahead during cold
#   reads, cache hit accounting, write-through coherence (/BLCK.TXT across a
#   flush), and a deterministic read-ahead probe. This script greps all
#   [blk] markers and fails on any FAILED / missing line.
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

# Wait for the final blk_test marker (poll; guest time varies under load).
function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}
[void](Wait-LogMarker $log 'read-ahead probe' 90)
Start-Sleep -Seconds 4   # let fstest finish
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-A5 block-cache markers ==='
$content | Where-Object { $_ -match '^\[blk\]' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Where-Object { $_ -match '^\[blk\] data equality: PASSED' })) { $fail += 'cache data equality FAILED/missing' }
if (-not ($content | Where-Object { $_ -match '^\[blk\] write-through: PASSED' })) { $fail += 'write-through coherence FAILED/missing' }
if (-not ($content | Where-Object { $_ -match '^\[blk\] read-ahead probe: PASSED' })) { $fail += 'read-ahead probe FAILED/missing' }
if (-not ($content | Where-Object { $_ -match '^\[blk\] read-ahead during cold: YES' })) { $fail += 'read-ahead did not warm the cache during the cold read' }
$speedupLine = $content | Where-Object { $_ -match '^\[blk\] cold read .*speedup x(\d+)' } | Select-Object -First 1
if (-not $speedupLine) { $fail += 'cold/hot speedup marker missing' }
elseif ($speedupLine -match 'speedup x(\d+)' -and [int]$Matches[1] -lt 100) {
    $fail += "hot read not measurably faster (speedup x$($Matches[1]) < x100 = 2x)"
}
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-A5 BLOCK CACHE TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}