# test-pci.ps1 - M9.6-B1 regression: PCI enumeration + shell `lspci`.
#   1. Boot-time scan finds QEMU's expected devices (host bridge, PIIX3
#      ISA/IDE, std VGA) with BAR decoding.
#   2. Typing `lspci` at the shell re-prints the table via SYS_LSPCI
#      (proves the syscall + shell command path).
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
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot', '-monitor', "tcp:127.0.0.1:$monPort,server,nowait") `
    -WindowStyle Hidden -PassThru

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

Start-Sleep -Seconds 4
[void](Wait-LogMarker $log 'entering interactive mode' 90)

# Count device lines before typing lspci, then type it and count again.
$before = @((Get-Content $log -ErrorAction SilentlyContinue) | Where-Object { $_ -match '^\[pci\] \d\d:\d\d\.\d ' }).Count
try {
    $c = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $c.GetStream()
    foreach ($k in @('l','s','p','c','i','ret')) {
        $b = [Text.Encoding]::ASCII.GetBytes("sendkey $k`n")
        $s.Write($b, 0, $b.Length)
        Start-Sleep -Milliseconds 150
    }
    $c.Close()
} catch { Write-Host "monitor sendkey failed: $_" }

[void](Wait-LogMarker $log '^\[pci\] functions found' 60)
Start-Sleep -Seconds 5
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-B1 PCI devices ==='
$content | Where-Object { $_ -match '^\[pci\] ' } | ForEach-Object { Write-Host "  $_" }

$pciLines = @($content | Where-Object { $_ -match '^\[pci\] \d\d:\d\d\.\d ' })
$after = $pciLines.Count

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if ($pciLines.Count -lt 4) { $fail += "too few PCI functions found ($($pciLines.Count), want >=4)" }
if (-not ($content | Where-Object { $_ -match '^\[pci\] .*1234:1111' })) { $fail += 'std VGA (1234:1111) not found' }
if (-not ($content | Where-Object { $_ -match '^\[pci\] .*class=0300' })) { $fail += 'no display-class device' }
if (-not ($content | Where-Object { $_ -match '^\[pci\] .*piix3-ide|^\[pci\] .*class=0101' })) { $fail += 'IDE controller not found' }
if (-not ($content | Where-Object { $_ -match '^\[pci\] .*440fx-host-bridge' })) { $fail += 'host bridge not found' }
if ($after -le $before) { $fail += "shell lspci did not reprint the table ($before -> $after)" }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-B1 PCI TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}