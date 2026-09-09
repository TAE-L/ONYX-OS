# test-apic.ps1 - M9.6-A2 regression: APIC/IOAPIC + LAPIC timer @ 1000 Hz.
#   1. APIC markers (LAPIC enabled, timer calibrated, IOAPIC routed, PIC masked)
#   2. PIT liveness through the IOAPIC ([ticker] keeps advancing at 100 Hz)
#   3. REAL keyboard end-to-end: QEMU monitor sendkey types "echo hi" into the
#      guest PS/2 controller -> IRQ1 -> IOAPIC GSI1 -> vector 0x21 -> LAPIC
#      EOI -> keyboard ring -> shell -> prints "hi" on serial.
#   4. Full M9.6 regression set (fstest, badptr, time, fpu tasks).
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-boot'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'
# Kill any stale QEMU from a previous test (guards serial.log + monitor port).
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$monPort = 45454
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot', '-monitor', "tcp:127.0.0.1:$monPort,server,nowait") `
    -WindowStyle Hidden -PassThru

Start-Sleep -Seconds 10   # boot + autoexec (fstest) + shell prompt

# Type "echo hi" + Enter through the QEMU human monitor.
$kbSent = $false
try {
    $c = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $c.GetStream()
    foreach ($k in @('e', 'c', 'h', 'o', 'spc', 'h', 'i', 'ret')) {
        $b = [Text.Encoding]::ASCII.GetBytes("sendkey $k`n")
        $s.Write($b, 0, $b.Length)
        Start-Sleep -Milliseconds 200
    }
    $c.Close()
    $kbSent = $true
} catch {
    Write-Host "monitor sendkey failed: $_"
}

Start-Sleep -Seconds 12   # let the OS run (fpu checkpoints, ticker, ...)
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-A2 markers ==='
$content |
    Select-String -Pattern 'apic:|fstest: (PASSED|FAILED|badptr|time)|fpu-test: task . PASSED|PANIC' |
    Select-Object -Last 22 | ForEach-Object { $_.Line }

$tickerLines = @($content | Where-Object { $_ -match '^\[ticker\] t=\d+' })
Write-Host ("ticker samples: {0} (last: {1})" -f $tickerLines.Count, ($tickerLines | Select-Object -Last 1))

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Select-String -SimpleMatch 'apic: LAPIC at')) { $fail += 'LAPIC enabled marker missing' }
if (-not ($content | Select-String -SimpleMatch 'apic: timer calibrated')) { $fail += 'LAPIC timer calibration marker missing' }
if (-not ($content | Select-String -SimpleMatch 'apic: IOAPIC routed')) { $fail += 'IOAPIC routing marker missing' }
if (-not ($content | Select-String -SimpleMatch 'apic: PIC fully masked')) { $fail += 'PIC-masked marker missing' }
if ($kbSent -and -not ($content | Where-Object { $_.Trim() -eq 'hi' })) { $fail += 'keyboard e2e failed: shell did not print "hi"' }
if ($tickerLines.Count -lt 5) { $fail += 'PIT/ticker not alive through the IOAPIC (<5 samples)' }
if (-not ($content | Select-String -SimpleMatch 'fstest: badptr rejected OK')) { $fail += 'badptr marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: time monotonic OK')) { $fail += 'time monotonic marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fpu-test: task A PASSED')) { $fail += 'fpu-test A PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fpu-test: task B PASSED')) { $fail += 'fpu-test B PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-A2 APIC TESTS PASSED' -ForegroundColor Green
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}