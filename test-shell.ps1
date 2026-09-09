# test-shell.ps1 - M8: boots headless and prints the ring-3 shell markers
# (shell ELF loaded from disk, AUTOEXEC script, SYS_SPAWN of FSTEST.ELF).
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-boot'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

Start-Sleep -Seconds 10
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
Write-Host '=== M8 shell markers ==='
Get-Content $log -ErrorAction SilentlyContinue |
    Select-String -Pattern 'M8|SHELL|shell|autoexec|spawn|run:|write:|mkdir:|cat|FSTEST|PANIC|EXCEPTION|DOUBLE' |
    Select-Object -Last 50 | ForEach-Object { $_.Line }

Write-Host '=== last 10 lines (liveness) ==='
Get-Content $log -ErrorAction SilentlyContinue | Select-Object -Last 10
