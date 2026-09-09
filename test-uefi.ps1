# test-uefi.ps1 - M9: boots the UEFI GPT image headless via OVMF, greps serial
# markers, and grabs a monitor screendump of the graphical console (decoded to
# ASCII) so firmware/bootloader/kernel display state is visible. Uses -snapshot
# so the boot never writes to the source image.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$ovmf_code = Join-Path $root '.toolchain\qemu\share\edk2-x86_64-code.fd'

$tmp = Join-Path $env:TEMP 'onyx-uefi'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
# All QEMU inputs are copied into the SPACE-FREE temp dir: Start-Process
# -ArgumentList splits on spaces, so any path with a space (e.g. the project
# dir) would be torn into two arguments and QEMU would fail to open it.
$ovmf_code = Join-Path $tmp 'code.fd'
Copy-Item (Join-Path $root '.toolchain\qemu\share\edk2-x86_64-code.fd') $ovmf_code -Force
# QEMU ships no x86_64 vars file; edk2-i386-vars.fd is the documented
# varstore template for both i386 and x86_64 non-secure OVMF builds.
$ovmf_vars = Join-Path $tmp 'vars.fd'
Copy-Item (Join-Path $root '.toolchain\qemu\share\edk2-i386-vars.fd') $ovmf_vars -Force
$img = Join-Path $tmp 'uefi.img'
$log = Join-Path $tmp 'serial.log'
$err = Join-Path $tmp 'qemu-stderr.log'
Copy-Item (Join-Path $root 'target\debug\images\uefi.img') $img -Force
Remove-Item $log, $err -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $qemu `
    -ArgumentList @(
        '-m', '256M',
        '-drive', "if=pflash,format=raw,readonly=on,file=$ovmf_code",
        '-drive', "if=pflash,format=raw,file=$ovmf_vars",
        '-drive', "format=raw,file=$img,snapshot=on",
        '-display', 'none',
        '-serial', "file:$log",
        '-monitor', 'tcp:127.0.0.1:4444,server,nowait',
        '-no-reboot'
    ) `
    -WindowStyle Hidden -PassThru `
    -RedirectStandardError $err

# Fresh varstores need a longer first boot; give OVMF time to provision.
Start-Sleep -Seconds 30
$selfExited = $p.HasExited

# Screendump via the QEMU monitor before anything else.
$screen = Join-Path $tmp 'screen.ppm'
try {
    $c = New-Object Net.Sockets.TcpClient('127.0.0.1', 4444)
    $s = $c.GetStream(); $s.ReadTimeout = 3000
    $buf = New-Object byte[] 8192
    Start-Sleep -Milliseconds 500; if ($s.DataAvailable) { $null = $s.Read($buf, 0, $buf.Length) }
    $cmd = "screendump $screen`n"
    $b = [Text.Encoding]::ASCII.GetBytes($cmd); $s.Write($b, 0, $b.Length)
    Start-Sleep -Milliseconds 1500
    if ($s.DataAvailable) { $null = $s.Read($buf, 0, $buf.Length) }
    $cmd = "info status`n"; $b = [Text.Encoding]::ASCII.GetBytes($cmd); $s.Write($b, 0, $b.Length)
    Start-Sleep -Milliseconds 500
    $status = ''
    if ($s.DataAvailable) { $n = $s.Read($buf, 0, $buf.Length); $status = [Text.Encoding]::ASCII.GetString($buf, 0, $n) }
    $cmd = "quit`n"; $b = [Text.Encoding]::ASCII.GetBytes($cmd); $s.Write($b, 0, $b.Length)
    Start-Sleep -Milliseconds 800
    $c.Close()
    $flat = $status.Trim() -replace "`n", ' | '
    Write-Host "monitor: $flat"
} catch {
    Write-Host "monitor unavailable: $_"
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited: $selfExited"
if ($selfExited) {
    Write-Host '--- qemu stderr ---'
    Get-Content $err -ErrorAction SilentlyContinue | Select-Object -First 6
}
Write-Host '=== serial markers ==='
Get-Content $log -ErrorAction SilentlyContinue |
    Select-String -Pattern 'Hello, OnyxOS|M5:|M6: FAT32 mounted|M7: ext2 mounted|M9|shell|fstest|PASSED|PANIC|EXCEPTION|DOUBLE' |
    Select-Object -Last 25 | ForEach-Object { $_.Line }
Write-Host '=== last 4 serial lines ==='
Get-Content $log -ErrorAction SilentlyContinue | Select-Object -Last 4

# Decode the screendump (P6 PPM) to ASCII: sample a grid, map luminance.
if (Test-Path $screen) {
    $bytes = [IO.File]::ReadAllBytes($screen)
    # header: P6\n<ws><w> <h>\n255\n
    $pos = 2; $vals = @()
    while ($vals.Count -lt 3) {
        while ($bytes[$pos] -eq 10 -or $bytes[$pos] -eq 13 -or $bytes[$pos] -eq 32) { $pos++ }
        if ($bytes[$pos] -eq 35) { while ($bytes[$pos] -ne 10) { $pos++ }; continue }
        $v = 0
        while ($bytes[$pos] -ge 48 -and $bytes[$pos] -le 57) { $v = $v * 10 + ($bytes[$pos] - 48); $pos++ }
        $vals += $v
    }
    $pos++ # single whitespace after last token
    $w = $vals[0]; $h = $vals[1]
    Write-Host ("=== screendump {0}x{1} (ASCII) ===" -f $w, $h)
    $chars = ' .:-=+*#%@'
    $cols = 100; $rows = 34
    for ($r = 0; $r -lt $rows; $r++) {
        $line = ''
        for ($c2 = 0; $c2 -lt $cols; $c2++) {
            $px = [int](($c2 + 0.5) * $w / $cols)
            $py = [int](($r + 0.5) * $h / $rows)
            $o = $pos + ($py * $w + $px) * 3
            if ($o + 2 -lt $bytes.Length) {
                $lum = [int](0.299 * $bytes[$o] + 0.587 * $bytes[$o+1] + 0.114 * $bytes[$o+2])
                $line += $chars[[Math]::Min(9, $lum * 10 / 256)]
            }
        }
        Write-Host $line
    }
} else {
    Write-Host 'no screendump captured'
}
