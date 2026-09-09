# test-fs.ps1 - M9.6-C3 regression: file API completion.
#   - fstest (ring 3) now gates a C3 block into its PASSED result:
#       * stat  : size match + is_file/is_dir, -ENOENT / -EFAULT cases
#       * seek  : SET/CUR/END cursors, -EINVAL bad whence, -EBADF bad fd
#       * rename: source vanishes, target appears, -EEXIST, -ENOENT source
#       * unlink: file removed then -ENOENT, dir -> -EISDIR, missing -> -ENOENT
#   - Shell `stat`/`rm` smoke (autoexec seeded into the FAT image).
#   - Dual mount intact (FAT32 + ext2 at "/"), no PANIC / unexpected exception.
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

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

# Boot (~210 ms), then autoexec runs ls/mkdir/write/cat/stat/rm + spawns
# fstest. fstest ends by printing `fstest: PASSED|FAILED` — wait for that
# terminator (not a fixed wall-clock sleep) so the suite is robust to host
# speed / TCG timing. The b3/perf kernel tasks on other lanes don't gate this.
[void](Wait-LogMarker $log 'shell:' 90)
$done = Wait-LogMarker $log 'fstest: (PASSED|FAILED)' 45
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue

Write-Host '=== M9.6-C3 file API ==='
$content | Where-Object { $_ -match '^fstest:|^stat: |^rm: |^ren: ' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
# The four C3 sub-checks (each accumulates into fstest: PASSED).
if (-not ($content | Select-String -SimpleMatch 'fstest: stat OK')) { $fail += 'stat check did not print OK' }
if (-not ($content | Select-String -SimpleMatch 'fstest: seek OK')) { $fail += 'seek check did not print OK' }
if (-not ($content | Select-String -SimpleMatch 'fstest: rename OK')) { $fail += 'rename check did not print OK' }
if (-not ($content | Select-String -SimpleMatch 'fstest: unlink OK')) { $fail += 'unlink check did not print OK' }
# errno specifics (-ENOENT/-EISDIR/-EEXIST/-EBADF/-EINVAL/-EFAULT) are gated
# into the PASSED flag, so the final marker is the term of truth.
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
# Shell smoke: stat printed a size, rm reported "no such file".
if (-not ($content | Where-Object { $_ -match '^stat: /DOCS/NOTE.TXT size=\d+' })) { $fail += 'shell `stat` did not print size' }
if (-not ($content | Select-String -SimpleMatch 'rm: no such file')) { $fail += 'shell `rm` of a missing file did not report -ENOENT' }
if (-not ($content | Select-String -SimpleMatch 'M7: ext2 mounted')) { $fail += 'ext2 mount regression' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception occurred' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-C3 FILE API TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}