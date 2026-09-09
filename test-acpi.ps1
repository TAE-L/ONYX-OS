# test-acpi.ps1 - M9.6-B2 regression: ACPI core (RSDP/RSDT/MADT/HPET).
#   - RSDP found (bootloader-provided)
#   - RSDT directory validated (checksum) + >=3 tables decoded
#   - MADT: LAPIC + IOAPIC + IRQ-overrides parsed
#   - Cross-check: MADT IOAPIC addr matches the live A2 APIC wiring
#   - fstest still passes (nothing broken)
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

[void](Wait-LogMarker $log '\[acpi\] done:' 90)
Start-Sleep -Seconds 4
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-B2 ACPI ==='
$content | Where-Object { $_ -match '^\[acpi\]' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Select-String -SimpleMatch '[acpi] RSDP at')) { $fail += 'RSDP not found' }
if (-not ($content | Select-String -SimpleMatch '[acpi] RSDT at')) { $fail += 'RSDT directory not found' }
if (-not ($content | Select-String -SimpleMatch '[acpi] done: ')) { $fail += 'ACPI init did not complete' }
if (-not ($content | Select-String -SimpleMatch '[acpi] MADT:')) { $fail += 'MADT not parsed' }
if (-not ($content | Select-String -SimpleMatch '[acpi] MADT: IRQ 0 -> GSI 2')) { $fail += 'IRQ0->GSI2 override missing (A2 wiring cross-check)' }
if (-not ($content | Select-String -SimpleMatch '[acpi] cross-check: MADT IOAPIC addr matches A2 wiring')) { $fail += 'MADT/A2 IOAPIC cross-check failed' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception occurred' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-B2 ACPI TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}