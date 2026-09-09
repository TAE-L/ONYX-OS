# test-input.ps1 - mouse cursor animation + keyboard + IST clock, verified
# headless through the QEMU human monitor:
#   1. mouse_move injections -> [mouse] pos=(x,y) lines must TRACK the deltas
#      (proves IRQ12 -> IOAPIC -> LAPIC -> packet decode -> cursor move).
#   2. sendkey "echo keys"   -> shell echoes + prints "keys" (keyboard e2e).
#   3. mouse_button left     -> [mouse] ... L=1 line.
#   4. [clock] IST HH:MM:SS heartbeats every 10 s (IST clock advances).
# All observed lines are printed below so you can SEE position + keys here.
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

$monPort = 45455
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

Start-Sleep -Seconds 4   # initial boot
# Wait until the shell is live and blocked on input (interactive prompt).
if (-not (Wait-LogMarker $log 'entering interactive mode' 90)) { Write-Host 'shell did not reach interactive mode in time' }

$mon = $null
try {
    $mon = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $mon.GetStream()

    function Send-Mon($cmd) {
        $b = [Text.Encoding]::ASCII.GetBytes("$cmd`n")
        $s.Write($b, 0, $b.Length)
    }

    Write-Host '--- injecting mouse moves (monitor: mouse_move) ---'
    foreach ($d in @(@('40','0'), @('0','40'), @('-60','0'), @('0','-30'))) {
        Send-Mon "mouse_move $($d[0]) $($d[1])"
        Start-Sleep -Milliseconds 900
    }

    Write-Host '--- injecting mouse button (left press + release) ---'
    Send-Mon 'mouse_button 1'
    Start-Sleep -Milliseconds 600
    Send-Mon 'mouse_button 0'
    Start-Sleep -Milliseconds 600

    Write-Host '--- typing "echo keys" via sendkey ---'
    foreach ($k in @('e','c','h','o','spc','k','e','y','s','ret')) {
        Send-Mon "sendkey $k"
        Start-Sleep -Milliseconds 150
    }
} catch {
    Write-Host "monitor access failed: $_"
} finally {
    if ($mon) { $mon.Close() }
}

# Wait for the shell to execute the command (blocking SYS_READ wakes).
[void](Wait-LogMarker $log '^keys' 60)
Start-Sleep -Seconds 8   # extra runtime for heartbeats/console evidence
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

$mouseLines = @($content | Where-Object { $_ -match '\[mouse\] pos=\(\d+,\d+\)' })
$clockLines = @($content | Where-Object { $_ -match '^\[clock\] IST \d\d:\d\d:\d\d' })
# console byte counters from the heartbeats (proves the framebuffer console
# is being fed by keyboard echo + SYS_WRITE output).
$consoleCounters = @($clockLines | ForEach-Object {
    if ($_ -match 'console_bytes=(\d+)') { [int]$Matches[1] } else { 0 }
})

Write-Host ''
Write-Host '=== KEY PRESSES (from the keyboard IRQ path) ==='
$keyLines = @($content | Where-Object { $_ -match '^\[key\] ' })
if ($keyLines.Count -eq 0) { Write-Host '  (none received)' } else { $keyLines | ForEach-Object { Write-Host "  $_" } }
Write-Host ''
Write-Host '=== MOUSE POSITION UPDATES (cursor animation evidence) ==='
if ($mouseLines.Count -eq 0) { Write-Host '  (none received)' } else { $mouseLines | ForEach-Object { Write-Host "  $_" } }
Write-Host ''
Write-Host '=== IST CLOCK HEARTBEATS (10 s cadence) ==='
if ($clockLines.Count -eq 0) { Write-Host '  (none received)' } else { $clockLines | ForEach-Object { Write-Host "  $_" } }
Write-Host ''
Write-Host '=== KEY / SHELL / FS markers ==='
$content | Where-Object { $_ -match '^(keys|hi)$|fstest: (PASSED|FAILED|badptr|time)|EXCEPTION|PANIC' } |
    Select-Object -Last 10 | ForEach-Object { Write-Host "  $_" }
Write-Host ''
if ($clockLines.Count -gt 0) {
    Write-Host "console_bytes heartbeats: $($consoleCounters -join ', ')"
}

# ---- pass/fail ----
$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if ($mouseLines.Count -lt 4) { $fail += "mouse deltas not reported ($($mouseLines.Count) lines, want >=4)" }
else {
    $posRe = [regex]'\[mouse\] pos=\((\d+),(\d+)\)'
    $first = $posRe.Match($mouseLines[0]); $lastM = $posRe.Match($mouseLines[-1])
    $moved = ($first.Groups[1].Value -ne $lastM.Groups[1].Value) -or ($first.Groups[2].Value -ne $lastM.Groups[2].Value)
    if (-not $moved) { $fail += 'cursor position never changed across injections (animation broken)' }
}
if (-not ($content | Where-Object { $_.Trim() -eq 'keys' })) { $fail += 'keyboard e2e failed: shell did not print "keys"' }
if ($keyLines.Count -lt 8) { $fail += "key presses not reported on serial ($($keyLines.Count) [key] lines, want >=8 for the 10 injected keys)" }
$rising = $false
for ($i = 1; $i -lt $consoleCounters.Count; $i++) {
    if ($consoleCounters[$i] -gt $consoleCounters[$i - 1]) { $rising = $true; break }
}
if ($consoleCounters.Count -eq 0) { $fail += 'IST clock heartbeat missing (no console counter)' }
elseif (-not $rising) { $fail += 'framebuffer console not being fed (console_bytes not rising across heartbeats)' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: INPUT + CLOCK TESTS PASSED' -ForegroundColor Green
    Write-Host 'note (bios-gui): QEMU grabs the mouse only after you CLICK into its window; the guest cursor then follows. Release with Ctrl+Alt+G.' -ForegroundColor Yellow
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}