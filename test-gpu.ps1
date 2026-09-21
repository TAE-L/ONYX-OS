# test-gpu.ps1 - M10a: PCI GPU scan + kernel-controlled modesetting.
#
# Boots the image twice:
#   1. -vga std with 128 MiB of video memory (-global VGA.vgamem_mb=128): the
#      kernel must find the display device on the bus, program its chosen mode
#      (1920x1080x32) through the bochs VBE/dispi interface, map the LFB BAR
#      and prove the mapping with a canary read-back.
#   2. -vga std with QEMU's default 16 MiB: same assertions, proving the mode
#      (8.3 MiB surface) does not depend on the extra VRAM.
#
# Every boot also runs the kernel's own "fallback rehearsal": a deliberately
# bogus mode write whose read-back must be caught, and which is then restored.
# That is the graceful-fallback assertion the M10a spec allows for (a machine
# without a display device cannot be booted on this QEMU/SeaBIOS at all —
# `-vga none` never reaches the bootloader because SeaBIOS needs a VGA BIOS).
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$img  = Join-Path $root 'target\debug\images\bios.img'
$fail = @()

function Invoke-Boot([string]$tag, [string]$extra, [int]$seconds = 42) {
    $tmp = Join-Path $env:TEMP "onyx-gpu-$tag"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $image = Join-Path $tmp 'bios.img'
    $log = Join-Path $tmp 'serial.log'
    Copy-Item $img $image -Force
    Remove-Item $log -ErrorAction SilentlyContinue
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    $argsList = @('-smp', '2', '-m', '512M', '-vga', 'std', '-drive',
                  "format=raw,file=$image", '-display', 'none',
                  '-serial', "file:$log", '-no-reboot', '-snapshot')
    if ($extra -ne '') { $argsList += ($extra -split ' ') }
    $p = Start-Process -FilePath $qemu -ArgumentList $argsList `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $seconds
    $exited = $p.HasExited
    if (-not $exited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 600
    return [pscustomobject]@{ Log = (Get-Content $log -ErrorAction SilentlyContinue); Exited = $exited }
}

function Check-Boot([pscustomobject]$boot, [string]$label) {
    $c = $boot.Log
    if ($boot.Exited) { $fail += "$label`: QEMU self-exited (crash indicator)" }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] scan: display device .*1234:1111.*class=0300' })) {
        $fail += "$label`: display device (1234:1111, class 0300) not found in the PCI scan"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] scan: .*bar\d=mem:0x[0-9a-f]+/.* MiB' })) {
        $fail += "$label`: display device has no MMIO framebuffer BAR recorded"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] dispi: id=0xb0c\d' })) {
        $fail += "$label`: bochs VBE interface not detected (dispi id)"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] modeset: 1920x1080x32 programmed and read back' })) {
        $fail += "$label`: 1920x1080x32 mode not programmed/verified"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] canary: .+ read back .+ - mapping verified' })) {
        $fail += "$label`: framebuffer canary not verified (mapping broken?)"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] mode set: 1920x1080x32 .*console on the dispi framebuffer' })) {
        $fail += "$label`: console not switched to the dispi framebuffer"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] task: dispi registers from task context: 1920x1080x32 enable=1' })) {
        $fail += "$label`: dispi mode did not survive the hand-over to the scheduler"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] fallback-rehearsal: bogus mode .* detected it' })) {
        $fail += "$label`: fallback rehearsal did not prove the bogus-mode detection"
    }
    if (-not ($c | Where-Object { $_ -match '\[gpu\] fallback-rehearsal: 1920x1080x32 restored' })) {
        $fail += "$label`: fallback rehearsal did not restore and re-verify the good mode"
    }
    if ($c | Where-Object { $_ -match '\[gpu\] .*fallback to' -and $_ -notmatch 'would fall back' }) {
        $fail += "$label`: an actual fallback happened (expected the full dispi path)"
    }
    if (-not ($c | Select-String -SimpleMatch 'fstest: PASSED')) {
        $fail += "$label`: fstest PASSED marker missing (boot flow broken)"
    }
    if (-not ($c | Where-Object { $_ -match 'autoexec done - entering interactive mode' })) {
        $fail += "$label`: boot flow did not reach the interactive shell"
    }
    if ($c | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
        $fail += "$label`: unexpected exception occurred"
    }
}

Write-Host '=== boot 1: std VGA, 128 MiB VRAM ==='
$b1 = Invoke-Boot 'v128' '-global VGA.vgamem_mb=128'
Write-Host ($b1.Log | Where-Object { $_ -match '^\[gpu\] ' } | ForEach-Object { "  $_" } | Select-Object -First 12)
Check-Boot $b1 'vram128'

Write-Host '=== boot 2: std VGA, default 16 MiB VRAM ==='
$b2 = Invoke-Boot 'v16' ''
Write-Host ($b2.Log | Where-Object { $_ -match '^\[gpu\] ' } | ForEach-Object { "  $_" } | Select-Object -First 12)
Check-Boot $b2 'vram16'

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M10a GPU TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}
