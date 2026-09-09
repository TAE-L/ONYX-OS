# test-raw.ps1 - M9.6-B4 regression: raw input event ring (SYS_INPUT_READ).
#   - Kernel-level `input_test` (main.rs): wire layout (24-byte records, hand-
#     decoded against plain integer ops), drop-on-full (256 kept / 44 dropped),
#     and the scheduler block-on-raw wake (a child pushes an event after 150 ms;
#     the parent blocked in the real wake path consumes it).
#   - Ring-3 E2E `EVTEST.ELF`: the shell runs it (B3 run blocks in waitpid);
#     we inject REAL mouse moves/buttons + keyboard presses via QEMU's monitor;
#     it must print [ev] K and [ev] M lines with ns timestamps, summarize PASSED
#     and exit with code 0 (the shell reports run: exit code 0).
#   - No PANIC / unexpected exception; fstest still PASSED.
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

# Boot + blk_test + input_test kernel regression + autoexec (fstest) + interactive shell.
if (-not ((Wait-LogMarker $log '\[b4\] block-on-raw wake: n=24 OK' 90))) { Write-Host 'kernel input_test did not reach the block-on-raw wake check in time' }
[void](Wait-LogMarker $log 'entering interactive mode' 60)
$mon = $null
try {
    $mon = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $mon.GetStream()
    function Send-Mon($cmd) {
        $b = [Text.Encoding]::ASCII.GetBytes("$cmd`n")
        $s.Write($b, 0, $b.Length)
    }
    # Type "run /EVTEST.ELF" at the shell (evtest spawns, blocks in raw input).
    # NB: QEMU sendkey wants symbolic names for punctuation ('slash', 'dot').
    Write-Host '--- typing run /EVTEST.ELF ---'
    foreach ($k in @('r','u','n','spc','slash','e','v','t','e','s','t','dot','e','l','f','ret')) {
        Send-Mon "sendkey $k"
        Start-Sleep -Milliseconds 120
    }
} catch {
    Write-Host "monitor access failed: $_"
} finally {
    if ($mon) { $mon.Close() }
}
Start-Sleep -Seconds 2 # let evtest spawn and block
$mon = $null
try {
    $mon = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $monPort)
    $s = $mon.GetStream()
    function Send-Mon2($cmd) {
        $b = [Text.Encoding]::ASCII.GetBytes("$cmd`n")
        $s.Write($b, 0, $b.Length)
    }
    # Inject REAL mouse motion + clicks, then REAL key presses.
    Write-Host '--- injecting mouse moves + clicks + keys ---'
    foreach ($d in @(@('33','0'), @('0','25'), @('-15','0'), @('0','-10'))) {
        Send-Mon2 "mouse_move $($d[0]) $($d[1])"
        Start-Sleep -Milliseconds 600
    }
    Send-Mon2 'mouse_button 1'
    Start-Sleep -Milliseconds 400
    Send-Mon2 'mouse_button 0'
    Start-Sleep -Milliseconds 400
    foreach ($k in @('a','b','c','spc','ret')) {
        Send-Mon2 "sendkey $k"
        Start-Sleep -Milliseconds 200
    }
} catch {
    Write-Host "monitor access failed: $_"
} finally {
    if ($mon) { $mon.Close() }
}

# evtest prints [ev] lines + summary, then exits; the shell reports run: exit code 0.
[void](Wait-LogMarker $log '\[ev\] summary key=' 60)
[void](Wait-LogMarker $log 'run: exit code 0' 30)
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-B4 raw input event ring ==='
$content | Where-Object { $_ -match '^\[b4\]|^\[ev\]|^run: |^M7: ext2|fstest:|^\[input\]' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
# Kernel-level regression checks (main.rs input_test):
if (-not ($content | Where-Object { $_ -match '\[b4\] layout checks: mouse=OK key=OK' })) { $fail += 'input_test wire layout check failed' }
if (-not ($content | Where-Object { $_ -match '\[b4\] drop-on-full: kept=256 dropped_delta=44 OK' })) { $fail += 'input_test drop-on-full check failed' }
if (-not ($content | Where-Object { $_ -match '\[b4\] block-on-raw wake: n=24 OK' })) { $fail += 'input_test scheduler block-on-raw wake failed' }
# Ring-3 E2E checks:
$evLines = @($content | Where-Object { $_ -match '^\[ev\]' })
if ($evLines.Count -lt 2) { $fail += "raw [ev] lines missing ($($evLines.Count) total" }
if (-not ($content | Where-Object { $_ -match '^\[ev\] M ' })) { $fail += 'no raw mouse event observed by ring-3' }
if (-not ($content | Where-Object { $_ -match '^\[ev\] K ' })) { $fail += 'no raw key event observed by ring-3' }
if (-not ($content | Where-Object { $_ -match '^\[ev\] summary key=.* mouse=.* PASSED' })) { $fail += 'evtest summary did not PASS' }
if (-not ($content | Where-Object { $_ -match '^run: exit code 0' })) { $fail += 'shell waitpid did not report evtest exit 0' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'M7: ext2 mounted')) { $fail += 'ext2 mount regression' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-B4 RAW INPUT TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}