# OnyxOS M0 smoke test: boots the kernel in QEMU (headless) and greps serial output.
# Usage:  powershell -ExecutionPolicy Bypass -File test.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$env:RUSTUP_HOME = "$root\.toolchain\rustup"
$env:CARGO_HOME  = "$root\.toolchain\cargo"
$env:Path = "$env:CARGO_HOME\bin;$root\.toolchain\w64devkit\bin;$env:Path"

Write-Host '[1/3] building kernel + boot images...'
cargo build
if ($LASTEXITCODE -ne 0) { Write-Host 'BUILD FAILED'; exit 1 }

# Use relative paths + WorkingDirectory to avoid the spaces-in-path bug
# (the project folder is "PROJECT OS").
$qemu   = "$root\.toolchain\qemu\qemu-system-x86_64.exe"
$imgdir = "$root\target\debug\images"
$bios   = "$imgdir\bios.img"
$log    = "$imgdir\serial_test.log"
Remove-Item $log -ErrorAction SilentlyContinue

Write-Host '[2/3] booting in QEMU (headless)...'
$args = @(
    '-display', 'none',
    '-serial', 'file:serial_test.log',
    '-drive', 'format=raw,file=bios.img',
    '-no-reboot', '-snapshot', '-no-shutdown',
    '-m', '128M'
)
$p = Start-Process -FilePath $qemu -WorkingDirectory $imgdir -ArgumentList $args -PassThru
Start-Sleep -Seconds 12
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }

Write-Host '[3/3] checking serial output...'
$content = Get-Content $log -Raw -ErrorAction SilentlyContinue
if ($content -match 'Hello, OnyxOS') {
    Write-Host "TEST PASS - serial log contained:"
    Write-Host $content
    exit 0
} else {
    Write-Host "TEST FAIL - 'Hello, OnyxOS' not found in $log"
    if ($content) { Write-Host "got: $content" }
    exit 1
}