# test-signin.ps1 - P6c: the sign-in screen (avatar, name, Sign in, power menu).
#
# Runs the kernel headless and asserts on the `[signin]` self-test lines. The
# self-test prints one PASS/BAD line per check, so this asserts on EVERY check
# rather than just "the task ran" - a self-test that only proves it started is
# the same mistake as a metric that only proves it printed.
#
# The power ACTIONS are never executed by the self-test (a real restart would
# reboot the VM mid-assertion). What is asserted instead is that each menu row
# hit-tests to its own index, which is the part that can silently be wrong: a
# bug returning row 0 for every y would make "Shut down" reboot the machine.
#
# Waits for the final `[signin] done:` marker instead of sleeping a fixed
# amount: a wall-clock sleep is a race against host load and its failure looks
# exactly like a kernel regression.
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-signin'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $root 'logs\test-signin\serial.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
# Boot a COPY: never the shared target\debug\images\bios.img.
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

# The sign-in screen runs at PRIO_NORMAL behind the boot-critical tests and the
# other P6 self-tests, so it finishes LAST. The ceiling is generous for that
# reason; it is a CEILING, not a fixed sleep - the loop exits the moment the
# marker lands, so a fast host is not penalised.
$deadline = (Get-Date).AddSeconds(300)
$markers = @('\[signin\] done:')
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    if (Test-Path $log) {
        $all = $true
        foreach ($m in $markers) {
            if (-not (Select-String -Path $log -Pattern $m -Quiet)) { $all = $false }
        }
        if ($all) { break }
    }
}

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force

if (-not (Test-Path $log)) {
    Write-Host 'FAILED: no serial log produced'
    exit 1
}
$out = Get-Content $log

# Both completion markers must be there, and neither report a BAD check.
$failed = $false
foreach ($m in $markers) {
    if (-not ($out -match $m)) {
        Write-Host "FAILED: $m marker never appeared"
        $failed = $true
    }
}
$bad = $out | Select-String -Pattern '\[signin\] BAD '
if ($bad) {
    Write-Host 'FAILED: the sign-in self-test reported BAD checks'
    $bad | ForEach-Object { Write-Host "  $_" }
    $failed = $true
}
if ($failed) {
    $out | Select-String -Pattern '\[signin\]' | ForEach-Object { Write-Host "  $_" }
    exit 1
}

# Report the counts so the log shows the test really exercised the checks.
foreach ($m in $markers) {
    Write-Host (($out | Select-String -Pattern $m).Line)
}
Write-Host 'PASSED: sign-in self-test'
exit 0