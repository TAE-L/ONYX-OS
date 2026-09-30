# test-surface.ps1 - P6a stage 1: surfaces (create/read/damage/destroy).
#
# Runs the kernel headless and asserts the `[surface]` self-test lines. The
# self-test prints one PASS/BAD line per check, so this asserts on EVERY check
# rather than just "the task ran" - a self-test that only proves it started is
# the same mistake as a metric that only proves it printed.
#
# Waits for the final `[surface] done:` marker instead of sleeping a fixed
# amount: a wall-clock sleep is a race against host load and its failure looks
# exactly like a kernel regression.
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-surface'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $tmp 'serial.log'

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
# Boot a COPY: never the shared target\debug\images\bios.img.
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

# Poll for all FOUR completion markers (surface, blit, font, compositor), with a
# ceiling. The ceiling is 150 s because the P6 self-tests now run at PRIO_IDLE
# (so they cannot steal the CPU from boot-critical work) and therefore finish
# LAST - a 90 s budget was not enough for all four in sequence on this host.
# The budget is a CEILING, not a fixed sleep: the loop exits the moment every
# marker lands, so a fast host is not penalised.
$deadline = (Get-Date).AddSeconds(150)
$markers = @(
    '\[surface\] done:',
    '\[blit\] P6a stage 2 self-test done',
    '\[font\] P6a stage 3 self-test done',
    '\[comp\] done:'
)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    if (Test-Path $log) {
        $all = $true
        foreach ($m in $markers) {
            if (-not (Select-String -Path $log -Pattern $m -Quiet)) { $all = $false; break }
        }
        if ($all) { break }
        if ($p.HasExited) { break }
    }
}
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

$lines = Get-Content $log -ErrorAction SilentlyContinue
Write-Host "qemu self-exited (crash indicator): $selfExited"
Write-Host '=== surface self-test (stage 1) ==='
$lines | Select-String -Pattern '\[surface\]' | ForEach-Object { $_.Line }
Write-Host '=== blit self-test (stage 2) ==='
$lines | Select-String -Pattern '\[blit\]' | ForEach-Object { $_.Line }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }

# Every check the self-tests can print. Asserting the full list is the point:
# a missing line means the check never ran, which is a FAILURE, not a skip.
$required = @(
    'PASS create',
    'PASS zeroed',
    'PASS rejects',
    'PASS write',
    'PASS damage: corner writes unioned',
    'PASS damage: take_damage clears',
    'PASS destroy',
    'PASS reuse',
    '\[surface\] done:',
    # stage 2
    'PASS fill clip',
    'PASS fill: pixel written inside',
    'PASS blit rect',
    'PASS ramp',
    'PASS damage: destination marked',
    'PASS rejects: partial-source blit clamped',
    '\[blit\] P6a stage 2 self-test done',
    # stage 3
    'PASS parse:',
    'PASS cmap:',
    'PASS raster 16px',
    'PASS antialias',
    'PASS scale:',
    '\[font\] P6a stage 3 self-test done',
    # P6b
    'PASS backdrop:',
    'PASS window: interior',
    'PASS title text:',
    'PASS taskbar:',
    'PASS focus ring:',
    'PASS clipping:',
    '\[comp\] done:'
)
foreach ($r in $required) {
    if (-not ($lines | Select-String -Pattern $r -Quiet)) {
        $fail += "missing surface marker: $r"
    }
}
# No FAILED line may appear, from any stage.
if ($lines | Select-String -Pattern '\[(surface|blit|font|comp)\] FAILED') {
    $lines | Select-String -Pattern '\[(surface|blit|font|comp)\] FAILED' | ForEach-Object { Write-Host "  $($_.Line)" }
    $fail += 'a P6 self-test reported FAILED'
}
# Corruption guards, same as every other suite.
if ($lines | Select-String -Pattern 'PANIC|DOUBLE FAULT') { $fail += 'kernel fault reported' }
if ($lines | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
    $fail += 'unexpected exception'
}

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: P6a SURFACE TESTS PASSED' -ForegroundColor Green
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}