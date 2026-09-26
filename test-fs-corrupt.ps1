# test-fs-corrupt.ps1 - M10b P3: a MALFORMED filesystem must be REJECTED, never
# panic the kernel.
#
# Why this exists: `panic = abort` in both Cargo profiles, so any panic on a
# disk-input-driven path takes the WHOLE kernel down - a corrupt boot sector has
# the same blast radius as a bug in the scheduler. The block layer's GPT parser
# used to index the partition-entry array with `entry_size` / `entry_count` read
# straight off the disk: a zero or sub-128 `entry_size` sliced off the end of
# the table (panic), and a >255 sector count was silently truncated by an
# `as u8` before the loop indexed past the bytes it had read. Those are now
# validated by `validate_gpt_geometry`, and a boot-time self-test
# (`block::gpt_selftest`) drives the real validation with hostile values.
#
# Why a SELF-TEST and not a corrupted disk image: the real trigger for
# `scan_gpt` is MBR partition 0's type byte being 0xEE, but on this image that
# same byte is the bootloader's own boot partition (type 0x20, LBA 1). Setting
# it to 0xEE to reach the GPT path stops the kernel from booting at all
# (verified: zero serial output). So the validation is exercised directly by the
# self-test instead, which is deterministic and needs no crafted media.
#
# Assertions:
#   * the self-test reports PASS: every hostile geometry was rejected and the
#     valid one accepted, with no panic,
#   * the kernel continues past the block layer (frame allocator / heap test),
#     proving nothing aborted,
#   * no EXCEPTION line anywhere.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$src  = Join-Path $root 'target\debug\images\bios.img'
$tmp  = Join-Path $env:TEMP 'onyx-fs-corrupt'
# Boot a COPY, like every other suite: pointing QEMU at the shared
# `target\...bios.img` takes a write lock on it and the boot produces NO serial
# output at all (verified - it looks like a kernel abort but is not).
$img  = Join-Path $tmp 'fs-corrupt.img'
$log  = Join-Path $tmp 'serial.log'
$fail = @()

New-Item -ItemType Directory -Force -Path $tmp | Out-Null
Copy-Item $src $img -Force
Remove-Item $log -ErrorAction SilentlyContinue
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$p = Start-Process -FilePath $qemu -ArgumentList @(
    '-smp','2','-m','512M','-vga','std',
    '-drive',"format=raw,file=$img",
    '-display','none',
    '-serial',"file:$log",
    '-no-reboot','-snapshot') -WindowStyle Hidden -PassThru
Start-Sleep -Seconds 30
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 500

$content = @(Get-Content $log -ErrorAction SilentlyContinue)
Write-Output '--- block-layer / gpt-selftest lines ---'
$content | Where-Object { $_.Contains('M5:') } | ForEach-Object { "  $_" }
Write-Output '--- survival markers ---'
$content | Where-Object { $_ -match 'block layer ready|heap allocation test' } |
    ForEach-Object { "  $_" }

function Has([string[]]$frags) {
    foreach ($l in $content) {
        if ($null -eq $l) { continue }
        $ok = $true
        foreach ($f in $frags) { if (-not $l.Contains($f)) { $ok = $false; break } }
        if ($ok) { return $true }
    }
    return $false
}

# 1. Every hostile GPT geometry was rejected by the real validation, no panic.
if (-not (Has @('gpt-selftest PASS', 'corrupt geometries rejected'))) {
    $fail += 'gpt-selftest did not report PASS (corrupt geometry not rejected cleanly)'
}
# 2. The kernel got past the block layer.
if (-not (Has @('M5: block layer ready'))) {
    $fail += 'block layer never reported ready (kernel likely aborted)'
}
# 3. It survived well past the block layer.
if (-not (Has @('heap allocation test PASSED'))) {
    $fail += 'kernel did not reach the heap test (aborted?)'
}
# 4. Nothing panicked.
if ($content | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
    $fail += 'an exception was raised (a panic aborted the kernel)'
}

if ($fail.Count -eq 0) {
    Write-Output 'RESULT: M10b P3 CORRUPT-FS TEST PASSED'
    Exit 0
} else {
    Write-Output 'RESULT: FAILED:'
    $fail | ForEach-Object { Write-Output "  - $_" }
    Exit 1
}
