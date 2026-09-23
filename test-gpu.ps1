# test-gpu.ps1 - M10/M10b: PCI GPU scan, kernel-controlled modesetting,
# virtio-gpu transport probe and the graceful-fallback path.
#
# Boots the image four times:
#   1. -vga std, 128 MiB VRAM (-global VGA.vgamem_mb=128): the kernel finds the
#      display device, programs 1920x1080x32 through the bochs VBE/dispi
#      interface, maps the LFB BAR (canary-verified) and owns the console.
#   2. -vga std, QEMU's default 16 MiB: same assertions, so the mode does not
#      depend on the extra VRAM.
#   3. -vga virtio (1af4:1050 virtio-vga): the M10a assertions on the *modern*
#      device, plus the M10b transport probe (virtio capability list: four
#      regions, 2 queues, 1 scanout, feature decode) and its hand-over to the
#      scheduler task.
#   4. -vga none -device virtio-vga-gl + egl-headless,gl=on: the host-GPU 3D
#      path (VIRGL + CONTEXT_INIT advertised) - the M10c/M10d foundation.
#
# Every boot also runs the kernel's in-boot "fallback rehearsal" (a deliberately
# bogus mode write whose read-back must be caught and restored): that is the
# graceful-fallback assertion, because a machine with *no* display device cannot
# boot here at all (`-vga none` alone never reaches the bootloader - SeaBIOS
# needs a VGA BIOS).
#
# Matching is substring (.Contains) on purpose, two reasons (PLAN.md bug log):
#   * -like treats "[gpu]" as a character class, so a bracketed marker never
#     matched its own literal text;
#   * the kernel's log separators are UTF-8 punctuation PowerShell may decode
#     as replacement glyphs - plain ASCII substrings are immune to that.
# Output uses Write-Output (not Write-Host) so `*> redirect` captures it all.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$img  = Join-Path $root 'target\debug\images\bios.img'
$fail = @()

function Invoke-Boot([string]$tag, [string]$extra, [int]$seconds = 30, [string]$vga = 'std', [string]$display = 'none') {
    $tmp = Join-Path $env:TEMP "onyx-gpu-$tag"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $image = Join-Path $tmp 'bios.img'
    $log = Join-Path $tmp 'serial.log'
    Copy-Item $img $image -Force
    Remove-Item $log -ErrorAction SilentlyContinue
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    $argsList = @('-smp', '2', '-m', '512M', '-vga', $vga, '-drive',
                  "format=raw,file=$image", '-display', $display,
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

# True when some single log line contains ALL of the given ASCII fragments.
function Has-Line($log, [string[]]$fragments) {
    foreach ($l in $log) {
        if ($null -eq $l) { continue }
        $ok = $true
        foreach ($f in $fragments) {
            if (-not $l.Contains($f)) { $ok = $false; break }
        }
        if ($ok) { return $true }
    }
    return $false
}

# Shared M10a assertions, run once per boot. `$script:fail` (not `$fail`) on
# purpose: a plain `$fail +=` inside a function writes a *local* copy and the
# top level would never see the failures - another bug from the log.
function Check-Boot([pscustomobject]$boot, [string]$label, [string]$devPattern) {
    $c = $boot.Log
    if ($boot.Exited) { $script:fail += "$label`: QEMU self-exited (crash indicator)" }
    if (-not (Has-Line $c @('[gpu] scan: display device ', $devPattern, 'class=0300'))) {
        $script:fail += "$label`: display device ($devPattern, class 0300) not found in the PCI scan"
    }
    if (-not (Has-Line $c @('[gpu] scan:', 'bar0=mem:0x'))) {
        $script:fail += "$label`: display device has no MMIO framebuffer BAR recorded"
    }
    if (-not (Has-Line $c @('[gpu] dispi: id=0xb0c'))) {
        $script:fail += "$label`: bochs VBE interface not detected (dispi id read failed)"
    }
    if (-not (Has-Line $c @('[gpu] modeset:', 'programmed and read back'))) {
        $script:fail += "$label`: mode not programmed and verified by register read-back"
    }
    if (-not (Has-Line $c @('[gpu] fallback-rehearsal:', 'read-back check detected it'))) {
        $script:fail += "$label`: fallback rehearsal: the deliberately bogus mode was NOT caught"
    }
    if (-not (Has-Line $c @('[gpu] fallback-rehearsal: 1920x1080x32 restored and re-verified'))) {
        $script:fail += "$label`: fallback rehearsal: good mode not restored after the bogus write"
    }
    if (-not (Has-Line $c @('[gpu] mapping:', '0x040000000000'))) {
        $script:fail += "$label`: LFB not mapped at the dedicated 0x040000000000 slot"
    }
    if (-not (Has-Line $c @('[gpu] canary:', 'mapping verified'))) {
        $script:fail += "$label`: framebuffer canary did not read back (mapping broken)"
    }
    if (-not (Has-Line $c @('[gpu] mode set: 1920x1080x32'))) {
        $script:fail += "$label`: console did not switch to the 1920x1080x32 surface"
    }
    if (-not (Has-Line $c @('[gpu] task: dispi registers from task context: 1920x1080x32 enable=1'))) {
        $script:fail += "$label`: modeset did not survive the boot->scheduler hand-over"
    }
    if (-not (Has-Line $c @('[gpu] task: dispi registers', 'fallbacks=0 rehearsals=1'))) {
        $script:fail += "$label`: live boot took an unexpected fallback (fallbacks must be 0)"
    }
    if (-not (Has-Line $c @('[gpu] task: LFB probe byte @', 'mapping translates'))) {
        $script:fail += "$label`: task-context LFB probe missing (mapping broken after migration)"
    }
    if (-not (Has-Line $c @('[gpu] task: mode 1920x1080x32 on ', $devPattern))) {
        $script:fail += "$label`: task mode summary missing or reports the wrong device"
    }
    if (-not (Has-Line $c @('fstest: PASSED'))) {
        $script:fail += "$label`: fstest PASSED marker missing (boot flow broken)"
    }
    if (-not (Has-Line $c @('autoexec done - entering interactive mode'))) {
        $script:fail += "$label`: boot flow did not reach the interactive shell"
    }
    if ($c | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
        $script:fail += "$label`: unexpected exception occurred"
    }
}

Write-Output '=== boot 1: std VGA, 128 MiB VRAM ==='
$b1 = Invoke-Boot 'v128' '-global VGA.vgamem_mb=128'
Write-Output ($b1.Log | Where-Object { $_.Contains('[gpu]') } | ForEach-Object { "  $_" } | Select-Object -First 12)
Check-Boot $b1 'vram128' '1234:1111'

Write-Output '=== boot 2: std VGA, default 16 MiB VRAM ==='
$b2 = Invoke-Boot 'v16' ''
Write-Output ($b2.Log | Where-Object { $_.Contains('[gpu]') } | ForEach-Object { "  $_" } | Select-Object -First 12)
Check-Boot $b2 'vram16' '1234:1111'

# Boot 3: the modern device (virtio-vga, 1af4:1050). The M10a path must stay
# correct on it AND the M10b transport probe must decode its capability list.
Write-Output '=== boot 3: virtio-vga (modern backend candidate) ==='
$b3 = Invoke-Boot 'vgpu' '' 30 'virtio'
Write-Output ($b3.Log | Where-Object { $_.Contains('[vgpu]') } | ForEach-Object { "  $_" } | Select-Object -First 10)
Check-Boot $b3 'virtio-vga' '1af4:1050'
if (-not (Has-Line $b3.Log @('[vgpu] caps: common bar', 'notify bar', 'isr bar', 'device bar'))) {
    $fail += 'virtio-vga: M10b probe did not decode all four virtio capability regions'
}
if (-not (Has-Line $b3.Log @('[vgpu] backend note: virtio-gpu transport probed: modern (VIRTIO_F_VERSION_1 negotiated)', '2 queue(s), 1 scanout(s)'))) {
    $fail += 'virtio-vga: M10b transport probe did not negotiate VERSION_1 / report 2 queues + 1 scanout'
}
if (-not (Has-Line $b3.Log @('[vgpu] probe: features lo=', 'GPU bits(want):'))) {
    $fail += 'virtio-vga: M10b probe did not decode the device feature bits'
}
if (-not (Has-Line $b3.Log @('[vgpu] task: virtio-gpu transport: modern', 'queues 2 scanouts 1'))) {
    $fail += 'virtio-vga: M10b transport probe did not survive the hand-over to the scheduler task'
}

# Boot 4: the *3D* path. `-vga none` so QEMU does not also instantiate the
# legacy std-VGA (which would shadow the virtio device as primary display);
# `-device virtio-vga-gl` + `egl-headless,gl=on` is the only configuration here
# where the host GPU is reachable (VIRGL + CONTEXT_INIT) - the M10c/M10d
# foundation, asserted rather than assumed.
Write-Output '=== boot 4: virtio-vga-gl (virgl 3D path) ==='
$b4 = Invoke-Boot 'vgl' '-device virtio-vga-gl' 30 'none' 'egl-headless,gl=on'
Write-Output ($b4.Log | Where-Object { $_.Contains('[vgpu]') } | ForEach-Object { "  $_" } | Select-Object -First 8)
Check-Boot $b4 'virtio-vga-gl' '1af4:1050'
if (-not (Has-Line $b4.Log @('[vgpu] probe: features lo=', 'virgl=1', 'ctx=1'))) {
    $fail += 'virtio-vga-gl: the host-GPU 3D path (VIRGL + CONTEXT_INIT) was not negotiated'
}
if (-not (Has-Line $b4.Log @('[vgpu] task: virtio-gpu transport: modern', 'queues 2 scanouts 1'))) {
    $fail += 'virtio-vga-gl: M10b task-context transport report missing on the 3D-capable device'
}

if ($fail.Count -eq 0) {
    Write-Output 'RESULT: M10 GPU TESTS PASSED'
    Exit 0
} else {
    Write-Output 'RESULT: FAILED:'
    $fail | ForEach-Object { Write-Output "  - $_" }
    Exit 1
}


