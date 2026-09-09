# test-sched.ps1 - M9.6-A3 regression: scheduler v2.
#   1. RT priority: no Normal/Idle task line may appear between
#      "[sched] rt start" and "[sched] rt end" (RT owns the CPU for its burst).
#   2. Sleep queue: "[sched] sleeper woke ms=N" lands on a ~500 ms cadence.
#   3. Blocking SYS_READ(0): typing "echo a3" at the shell (QEMU monitor
#      sendkey) proves the blocked shell wakes on input and returns the line.
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

$monPort = 45456
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

# Wait until at least `minCount` lines matching `pattern` have appeared. Used
# for the sleeper cadence: its markers accumulate on the guest's wall clock,
# so on a slow TCG host we must wait for them rather than sleep a fixed window
# then kill QEMU (which produces flaky "too few" failures).
function Wait-LogCount($log, $pattern, $minCount, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $cnt = (Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern }).Count
        if ($cnt -ge $minCount) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

# Wait until the shell is live and blocked on input (interactive prompt).
if (-not (Wait-LogMarker $log 'entering interactive mode' 90)) { Write-Host 'shell did not reach interactive mode in time' }

# Type "echo a3" + Enter: the blocked shell must wake and execute it.
try {
    $c = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $c.GetStream()
    foreach ($k in @('e','c','h','o','spc','a','3','ret')) {
        $b = [Text.Encoding]::ASCII.GetBytes("sendkey $k`n")
        $s.Write($b, 0, $b.Length)
        Start-Sleep -Milliseconds 150
    }
    $c.Close()
} catch { Write-Host "monitor sendkey failed: $_" }

# Wait for the shell to execute the command (blocking SYS_READ wakes), then for
# enough sleeper cadence markers to evaluate (>= 3 gives >= 2 deltas). The
# sleepers advance on the guest wall clock, which lags wall-clock here, so a
# count-based wait (not a fixed sleep) avoids killing QEMU too early.
[void](Wait-LogMarker $log '^\[sched\] rt burst PASSED' 120)
[void](Wait-LogCount $log '\[sched\] sleeper woke' 3 150)
Start-Sleep -Seconds 2   # settle for the interactive "a3" echo evidence
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-A3 markers ==='
$content | Where-Object { $_ -match '^\[sched\]' } | Select-Object -First 14 | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_.Trim() -eq 'a3' -or $_ -match 'fstest: (PASSED|FAILED)' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }

# --- RT priority: nothing else inside [rt start, rt end] ---
$startIdx = -1; $endIdx = -1
for ($i = 0; $i -lt $content.Count; $i++) {
    if ($content[$i].Trim() -eq '[sched] rt start') { $startIdx = $i }
    if ($content[$i].Trim() -eq '[sched] rt end (no normal task should have run between)' -and $startIdx -ge 0) { $endIdx = $i; break }
}
if ($startIdx -lt 0 -or $endIdx -lt 0) {
    $fail += 'RT burst markers missing'
} else {
    $intruder = $content[($startIdx + 1)..($endIdx - 1)] |
        Where-Object { $_ -match '^\[ticker\]|^\[clock\]|^\[mouse\]|^\[sched\] sleeper|^fstest:|^shell|^onyx>' }
    if ($intruder) { $fail += 'RT burst was preempted by a Normal/Idle task: ' + ($intruder[0]) }
}

# --- Sleep cadence: sleeper ms deltas ~500 (allow <=20% TCG-load outliers) ---
$sleeperMs = @($content | Where-Object { $_ -match '^\[sched\] sleeper woke' } | ForEach-Object {
    if ($_ -match 'ms=(\d+)') { [int]$Matches[1] } else { -1 }
})
if ($sleeperMs.Count -lt 2) { $fail += "sleeper woke markers too few ($($sleeperMs.Count))" }
else {
    $deltas = @()
    for ($i = 1; $i -lt $sleeperMs.Count; $i++) { $deltas += ($sleeperMs[$i] - $sleeperMs[$i - 1]) }
    $outliers = @($deltas | Where-Object { $_ -lt 400 -or $_ -gt 700 }).Count
    # The first delta includes boot/RT-burst offset; drop it from the count.
    $effective = [Math]::Max(1, $deltas.Count - 1)
    if ($outliers -gt [Math]::Ceiling($effective * 0.2)) {
        $fail += "sleeper cadence broken ($outliers outliers of $($deltas.Count)): $($sleeperMs -join ', ')"
    }
}

# --- Blocking input: shell printed the echo result ---
if (-not ($content | Where-Object { $_.Trim() -eq 'a3' })) { $fail += 'blocking SYS_READ(0) failed: shell did not print "a3"' }

if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch '[sched] rt burst PASSED')) { $fail += 'rt burst PASSED marker missing' }
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-A3 SCHEDULER TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}