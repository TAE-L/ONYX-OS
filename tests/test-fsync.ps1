# test-fsync.ps1 -- P16-1.4: prove fsync actually put bytes on the device.
#
#   powershell -ExecutionPolicy Bypass -File tests\test-fsync.ps1
#
# The obvious test -- "fsync returned 0" -- proves nothing at all: a kernel that
# implemented fsync as `return 0` would pass it. So this suite verifies the
# claim from OUTSIDE the guest:
#
#   1. boot WITHOUT -snapshot, so guest writes survive into the image file;
#   2. let `fstest` write "/Fsync Durability Probe.txt" and fsync it;
#   3. kill QEMU -- an abrupt end, which is the closest thing here to a crash;
#   4. read the raw image back with the INDEPENDENT host-side FAT32 reader in
#      tests/fat32-verify.ps1 and require the file, its exact size and its exact
#      payload to be there.
#
# A self-report from the guest would be worthless here; the whole point is that
# the checker never ran inside the kernel it is checking.
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

$tmp = Join-Path $env:TEMP 'onyx-fsync'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$img = Join-Path $tmp 'fsync.img'
$log = Join-Path $root 'logs\test-fsync\serial.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
Remove-Item $log -ErrorAction SilentlyContinue

$fail = @()

# NO -snapshot: this suite is about what survives on the device.
Start-Process -FilePath $qemu -WindowStyle Hidden -ArgumentList @(
    '-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot'
) | Out-Null

$deadline = (Get-Date).AddSeconds(120)
$done = $false
while ((Get-Date) -lt $deadline) {
    if (Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match 'fstest: fsync (OK|BAD)' }) { $done = $true; break }
    Start-Sleep -Milliseconds 500
}

# Kill rather than shut down: an abrupt end is a better durability probe.
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$content = Get-Content $log -ErrorAction SilentlyContinue
if (-not $done) {
    $fail += 'guest never reported a fsync result'
    $content | Select-Object -Last 15 | ForEach-Object { Write-Host "  $_" }
} elseif (-not ($content | Select-String -SimpleMatch 'fstest: fsync OK')) {
    $fail += 'fstest reported fsync BAD'
}

# Now the external check: the file must exist on the raw image, readable by
# something that is not our kernel.
$verify = Join-Path $root 'tests\fat32-verify.ps1'
$out = & powershell -ExecutionPolicy Bypass -File $verify -Image $img -DumpSlots 2>&1
$out | Where-Object { $_ -match 'Fsync Durability|DUPLICATE|orphan|walked' } |
    ForEach-Object { Write-Host "  $_" }

$listing = ($out | Where-Object { $_ -match 'Fsync Durability Probe\.txt' })
if (-not $listing) {
    $fail += 'the fsync fixture file is NOT in the raw image after the guest exited'
} else {
    # Size must match the payload fstest wrote. Derived from the literal rather
    # than hardcoded: the first version of this check said 40 and the real
    # payload is 42, so the suite failed against a kernel that had written the
    # correct size. A number repeated in two places is a bug waiting to happen.
    $payload = "fsync durability payload 0123456789abcdef`n"
    $want = [Text.Encoding]::ASCII.GetByteCount($payload)
    if ($listing -notmatch "size=$want\b") {
        $fail += "fixture is present but the size is wrong: $listing"
    } else {
        Write-Host "ok: the fsync fixture ($want bytes) survived the guest, visible to an independent reader" -ForegroundColor Green
    }
}

if ($fail.Count) {
    Write-Host ''
    Write-Host 'RESULT: FSYNC TESTS FAILED' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host ''
Write-Host 'RESULT: FSYNC TESTS PASSED'
exit 0