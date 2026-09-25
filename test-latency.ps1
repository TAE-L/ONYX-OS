# test-latency.ps1 - M10b 3b: input->present latency probe on the virtio present
# path (damage-rect + flusher), measured with real PS/2 mouse injection.
#
# What it asserts (substring, ASCII, per the M10b bug log):
#   * the virtio present path came up (queue + adopted console), so the probe
#     has a present path to measure against;
#   * mouse injection actually moves the cursor ([mouse] pos lines);
#   * the flusher reports a latency window WITH samples - i.e. the probe closed
#     the loop input-IRQ -> damage-rect -> device-ack. "no input samples" or a
#     missing line is a failure, because a latency metric that never fires is
#     worse than none.
#
# This is the metric a latency-driven OS is judged by; every later optimization
# (hardware cursor, frame pacing) is measured against THIS number, so the probe
# itself is a tested artifact, not debug output.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$img  = Join-Path $root 'target\debug\images\bios.img'
$script:fail = @()
$monPort = 45466

function Has-Line($log, [string[]]$fragments) {
    foreach ($l in $log) {
        if ($null -eq $l) { continue }
        $ok = $true
        foreach ($f in $fragments) { if (-not $l.Contains($f)) { $ok = $false; break } }
        if ($ok) { return $true }
    }
    return $false
}

$tmp = Join-Path $env:TEMP 'onyx-latency'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$image = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'
Copy-Item $img $image -Force
Remove-Item $log -ErrorAction SilentlyContinue
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

# -vga virtio: the ONLY backend that runs the damage-rect flusher, which is
# where the latency probe lives. std-VGA keeps the dispi console (no flusher),
# and virtio-vga-gl takes the documented 2D-skip (stage 3).
$p = Start-Process -FilePath $qemu -ArgumentList @(
    '-smp','2','-m','512M','-vga','virtio',
    '-drive',"format=raw,file=$image",
    '-display','none',
    '-serial',"file:$log",
    '-monitor',"tcp:127.0.0.1:$monPort,server,nowait",
    '-no-reboot','-snapshot') -WindowStyle Hidden -PassThru

function Wait-LogMarker($path, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Get-Content $path -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern }) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

# Wait for the flusher to be scheduled (present path live) before injecting.
if (-not (Wait-LogMarker $log 'flusher scheduled' 90)) {
    $script:fail += 'latency: virtio flusher never started (present path not live)'
}

Start-Sleep -Seconds 2
# Move the mouse around for a while so the probe accumulates input->present
# samples across one or more 5 s report windows.
$mon = $null
try {
    $mon = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $mon.GetStream()
    function Send-Mon($cmd) {
        $b = [Text.Encoding]::ASCII.GetBytes("$cmd`n")
        $s.Write($b, 0, $b.Length)
        Start-Sleep -Milliseconds 120
    }
    for ($i = 0; $i -lt 30; $i++) {
        # Alternate direction each step so the cursor sweeps back and forth
        # INSTEAD of pinning at a screen edge (a cursor clamped at the border
        # stops producing damage, so a latency window can come up with no
        # samples and assert nothing). Net displacement stays ~0, so it never
        # leaves the screen.
        if ($i % 2 -eq 0) { $dx = 30; $dy = 18 } else { $dx = -30; $dy = -18 }
        Send-Mon "mouse_move $dx $dy"
    }

    # Phase 2 (M10b 5): ON-DEMAND single-glyph workload. Let the console go
    # quiet first (so the flusher's idle backoff engages), then type one
    # character at a time, well spaced. Each keystroke produces ISOLATED
    # framebuffer damage (the shell echoes it), so the damage->present response
    # measures a single clean present instead of a boot burst - this is the
    # workload the burst test could not provide, and the one a pacing A/B needs.
    Start-Sleep -Seconds 6   # let the console quiesce -> idle backoff engages
    foreach ($k in 'a', 'b', 'c', 'd', 'e') {
        Send-Mon "sendkey $k"
        Start-Sleep -Milliseconds 700   # spacing so each glyph is its own present
    }
    Start-Sleep -Seconds 4   # close the window the phase-2 presents fall into
} catch {
    $script:fail += "latency: could not talk to the QMP monitor ($($_.Exception.Message))"
} finally {
    if ($mon) { $mon.Close() }
}

# Give the flusher time to close at least one latency window, and the
# kernel-local probe time to fire. The probe waits ~8s after present, then
# needs the console IDLE for a full second before each sample, so a busy
# console can delay its first sample well past the injection phase. Wait
# generously so the probe assertion is not timing-flaky.
Start-Sleep -Seconds 20
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 500

$content = @(Get-Content $log -ErrorAction SilentlyContinue)
Write-Output '--- [vgpu] latency + response + pacing + flush lines ---'
$content | Where-Object { $_ -match '\[vgpu\] (latency|response|pacing|flush)' } | ForEach-Object { "  $_" }
Write-Output '--- [mouse] pos lines (cursor actually moved) ---'
$content | Where-Object { $_.Contains('[mouse] pos') } | Select-Object -First 4 | ForEach-Object { "  $_" }

if (-not (Has-Line $content @('[vgpu] queue: controlq size=', 'DRIVER_OK'))) {
    $script:fail += 'latency: control queue not brought up on the virtio present path'
}
if (-not (Has-Line $content @('[vgpu] present: console adopted'))) {
    $script:fail += 'latency: console not adopted onto the virtio surface'
}
# M10b 3c: the hardware cursor must be live on queue 1 (a device without a
# cursor queue is allowed to fall back to software, so this is asserted only
# as "either hardware-cursor is live, OR the documented fallback is logged").
if (-not (Has-Line $content @('[vgpu] cursor: hardware cursor live'))) {
    if (-not (Has-Line $content @('[vgpu] cursor:', 'software cursor kept'))) {
        $script:fail += 'latency: neither the hardware cursor nor the documented software-cursor fallback was reported'
    }
}
if (-not (Has-Line $content @('[mouse] pos=('))) {
    $script:fail += 'latency: mouse injection did not move the cursor (no [mouse] pos line)'
}
# The probe must have CLOSED at least one window with real samples. WHICH probe
# is the correct one depends on who owns the cursor:
#   * software cursor -> a mouse move dirties the framebuffer, so the
#     input->present probe is the signal to require;
#   * hardware cursor -> a mouse move produces NO framebuffer damage (it is a
#     MOVE_CURSOR command), so input->present may legitimately be empty and
#     input->MOVE_CURSOR is the signal to require instead.
$hwLive = Has-Line $content @('[vgpu] cursor: hardware cursor live')
# "no input samples" is NOT a failure on its own: with the hardware cursor live
# a mouse move produces no framebuffer damage, so the present probe correctly
# sees nothing, and a quiet window legitimately reports zero. It is only a
# problem if EVERY window is empty while input was definitely injected, which
# is checked below by requiring a real sample from the correct probe.
if ($hwLive) {
    if (-not (Has-Line $content @('[vgpu] latency: input->MOVE_CURSOR', 'n=', 'avg='))) {
        $script:fail += 'latency: hardware cursor is live but no input->MOVE_CURSOR latency was reported'
    }
} elseif (-not (Has-Line $content @('[vgpu] latency: input->present', 'n=', 'avg='))) {
    $script:fail += 'latency: no input->present sample reported (probe did not close the loop)'
}
# M10b 4: the adaptive present must be reported with its cadence, and the
# frame-interval (pacing) stats must appear once the console has presented
# more than once.
if (-not (Has-Line $content @('[vgpu] present: flusher scheduled', 'adaptive cadence'))) {
    $script:fail += 'latency: flusher did not report the adaptive cadence (M10b 4 pacing)'
}
if (-not (Has-Line $content @('[vgpu] pacing:', 'presents', 'interval'))) {
    $script:fail += 'latency: no frame-interval (pacing) stats reported (M10b 4)'
}
# M10b 7: the kernel-local probe isolates PURE present-path latency (no
# ring-3 shell/input in the loop). Require at least one probe sample.
if (-not (Has-Line $content @('[vgpu] probe: kernel-local draw->present', 'n=', 'avg='))) {
    $script:fail += 'latency: no kernel-local probe sample (pure present latency not measured)'
}
# M10b 8: the kernel frame clock drives a continuous animated load; the pacing
# report must include an achieved-fps figure.
if (-not (Has-Line $content @('[vgpu] pacing:', 'presents', 'fps', 'interval'))) {
    $script:fail += 'latency: no frame-pacing fps/jitter report (M10b 8 frame clock)'
}
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
    $script:fail += 'latency: unexpected exception occurred'
}

if ($script:fail.Count -eq 0) {
    Write-Output 'RESULT: M10b 3b LATENCY PROBE PASSED'
    Exit 0
} else {
    Write-Output 'RESULT: FAILED:'
    $script:fail | ForEach-Object { Write-Output "  - $_" }
    Exit 1
}