# test-fpu.ps1 - regression tests for the two latent-bug fixes:
#   1. FPU/SSE state save/restore across context switches (two tasks keep
#      live XMM accumulators across preemptions and verify checkpoints)
#   2. user-pointer validation (fstest's badptr checks run on every boot)
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

Write-Host '=== M9.6-fix regression markers ==='
$content |
    Select-String -Pattern 'fpu:|fpu-test|badptr|fstest: (PASSED|FAILED)|PANIC|EXCEPTION|DOUBLE' |
    Select-Object -Last 20 | ForEach-Object { $_.Line }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Select-String -SimpleMatch 'fpu-test: task A PASSED')) { $fail += 'fpu-test A PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fpu-test: task B PASSED')) { $fail += 'fpu-test B PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: badptr rejected OK')) { $fail += 'badptr rejected OK marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
# The M1 int3 demo intentionally triggers "EXCEPTION: Breakpoint" — any
# other exception line is a real failure.
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: ALL M9.6-FIX REGRESSION TESTS PASSED' -ForegroundColor Green
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}