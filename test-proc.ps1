# test-proc.ps1 - M9.6-B3 regression: process lifecycle (waitpid/kill/zombies).
#   - Kernel-level: b3_test spawns children and verifies
#       * blocked-waitpid wakes and reaps exit code 42
#       * kill produces a zombie with exit code 137
#       * orphans are auto-reaped by the scheduler's wake pass
#   - Ring-3 E2E: shell autoexec `run /FSTEST.ELF` now blocks in SYS_WAITPID
#     and reports the child's exit code (fstest exits 0).
#   - Shell `sync` bug fix: SYS_FLUSH is 13 (was 12 = LSPCI).
#   - No PANIC / unexpected exception.
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

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

# Boot + blk_test + b3_test children + autoexec fstest + shell prompt.
# NOTE: this Windows host's QEMU TCG runs much slower than guest real-time, so
# the guest's wall-clock sleepers/children lag host time a lot. Wait for b3's
# terminators (orphan auto-reap is its last step) rather than killing QEMU on a
# fixed sleep — a fixed window here is a false failure on a loaded host.
[void](Wait-LogMarker $log 'shell:' 90)
[void](Wait-LogMarker $log '\[b3\] orphan auto-reap' 150)
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-B3 process lifecycle ==='
$content | Where-Object { $_ -match '^\[b3\]|^run: |^M7: ext2' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Where-Object { $_ -match '\[b3\] waitpid code=42 OK' })) { $fail += 'blocked waitpid did not reap exit code 42' }
if (-not ($content | Where-Object { $_ -match '\[b3\] kill code=137 OK' })) { $fail += 'kill did not produce a 137 zombie' }
if (-not ($content | Where-Object { $_ -match '\[b3\] orphan auto-reap OK' })) { $fail += 'orphan auto-reap failed' }
if ($content | Where-Object { $_ -match '\[b3\].*BAD' }) { $fail += 'a b3 check printed BAD' }
if (-not ($content | Where-Object { $_ -match 'run: pid \d+' })) { $fail += 'SYS_SPAWN did not return a child pid' }
if (-not ($content | Where-Object { $_ -match 'run: exit code 0' })) { $fail += 'shell waitpid did not report fstest exit code 0' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'M7: ext2 mounted')) { $fail += 'ext2 mount regression' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-B3 PROCESS LIFECYCLE TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}
