# test-sec.ps1 - P16-0.1: SMEP + SMAP.
#
# Boots the kernel with `-cpu max`, which is the QEMU CPU model that actually
# ADVERTISES SMEP/SMAP - the default `qemu64` does not, so the feature-detect
# path would otherwise never be exercised and this test would pass vacuously.
#
# Asserts, from the serial log:
#   1. CPUID reported support for both features;
#   2. CR4 actually has both bits set (the kernel reads CR4 back rather than
#      assuming the write landed, so this is a real verification);
#   3. the SMAP syscall entry stub is the one installed in LSTAR;
#   4. the whole boot completed with NO #PF / #DF / panic - i.e. every
#      deliberate kernel->user write is inside a declared STAC window, which is
#      the property SMAP exists to enforce.
#
# Waits for the `[signin] done:` marker rather than sleeping a fixed amount: a
# wall-clock sleep is a race against host load and its failure looks exactly like
# a kernel regression.
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-sec'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'bios.img'
$log = Join-Path $root 'logs\test-sec\serial.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
# Boot a COPY: never the shared target\debug\images\bios.img.
Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

# `-cpu max` is the point of this script: it is the only model in the default
# invocation that reports SMEP/SMAP at all. `-smp 2` additionally exercises the
# per-CPU syscall MSR programming on an AP.
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none',
        '-serial', "file:$log", '-no-reboot', '-snapshot',
        '-cpu', 'max', '-smp', '2') `
    -WindowStyle Hidden -PassThru

# Generous ceiling: sign-in self-tests are staggered behind every other boot
# test, so the marker lands last. It is a CEILING, not a fixed sleep.
$deadline = (Get-Date).AddSeconds(300)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    if (Test-Path $log) {
        if (Select-String -Path $log -Pattern '\[signin\] done:' -Quiet) { break }
    }
}

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force

if (-not (Test-Path $log)) {
    Write-Host 'FAILED: no serial log produced'
    exit 1
}
$out = Get-Content $log
$failed = $false

# 1 + 2: detected AND actually enabled in CR4.
foreach ($feat in @('SMEP', 'SMAP')) {
    $line = ($out | Select-String -Pattern "\[sec\] $feat").Line
    if (-not $line) {
        Write-Host "FAILED: no [$feat] line in the boot log"
        $failed = $true
    } elseif ($line -notmatch 'cpu=yes cr4=on') {
        Write-Host "FAILED: $feat not enabled - saw: $line"
        $failed = $true
    } else {
        Write-Host "ok: $line"
    }
}

# 3: the SMAP-aware entry stub is what LSTAR points at.
#
# `-notmatch` on an ARRAY returns the non-matching *elements* (always non-empty),
# so it is truthy even when every line matches. `Select-String -Quiet` is the
# correct "does any line match" form.
if (-not (Select-String -Path $log -Pattern 'entry=smap/stac' -Quiet)) {
    Write-Host 'FAILED: the SMAP syscall stub (stac/clac) was not installed'
    $failed = $true
} else {
    Write-Host 'ok: syscall entry stub = smap/stac'
}

# 4: SMAP is enforced from boot to the sign-in screen without faulting. This is
# the check that actually proves the STAC windows cover every deliberate
# kernel->user write: a missing one shows up as a #PF here.
$crash = $out | Select-String -Pattern 'PANIC|PAGE FAULT|DOUBLE FAULT'
if ($crash) {
    Write-Host 'FAILED: the kernel faulted while SMAP was enforced:'
    $crash | ForEach-Object { Write-Host "  $_" }
    $failed = $true
} else {
    Write-Host 'ok: no #PF / #DF / panic with SMEP+SMAP enforced'
}

# The ring-3 filesystem test is the strongest proof that the syscall path (the
# one running inside the STAC window) still works end to end.
if (Select-String -Path $log -Pattern 'fstest: PASSED' -Quiet) {
    Write-Host 'ok: fstest: PASSED (ring-3 syscalls work under SMAP)'
} else {
    Write-Host 'FAILED: fstest did not report PASSED'
    $failed = $true
}

if ($failed) { exit 1 }
Write-Host 'RESULT: SMEP/SMAP TESTS PASSED'
exit 0