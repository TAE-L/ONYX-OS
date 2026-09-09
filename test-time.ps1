# test-time.ps1 - M9.6-A1 regression: high-resolution timekeeping.
#   - TSC calibrated against the PIT at boot
#   - SYS_GETTIME(0) monotonic smoke test from ring 3 (fstest)
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-boot'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'
# Kill any stale QEMU from a previous test (guards serial.log collisions).
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

Start-Sleep -Seconds 35
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-A1 markers ==='
$content |
    Select-String -Pattern 'time: TSC|time monotonic|fstest: (PASSED|FAILED)|fpu-test: task . PASSED|PANIC' |
    Select-Object -Last 12 | ForEach-Object { $_.Line }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Select-String -SimpleMatch 'time: TSC calibrated')) { $fail += 'TSC calibration marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: time monotonic OK')) { $fail += 'monotonic clock smoke test missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-A1 TIMEKEEPING TESTS PASSED' -ForegroundColor Green
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}