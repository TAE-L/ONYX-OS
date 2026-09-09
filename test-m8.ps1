# test-m8.ps1 - boots headless and prints the M8 (subdirectories) markers.
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

Start-Sleep -Seconds 8
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
Write-Host '=== M8 / subdirectory markers ==='
Get-Content $log -ErrorAction SilentlyContinue |
    Select-String -Pattern 'M8|mkdir|SUBDIR|fstest|PASSED|FAILED|PANIC|EXCEPTION|DOUBLE' |
    Select-Object -Last 40 | ForEach-Object { $_.Line }

Write-Host '=== last 10 lines (liveness) ==='
Get-Content $log -ErrorAction SilentlyContinue | Select-Object -Last 10
