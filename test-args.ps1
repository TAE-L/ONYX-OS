# test-args.ps1 - M9.6-C1/C2 regression: process-start stack + errno.
#   - The shell's AUTOEXEC runs `run /ARGTEST.ELF alpha beta gamma`; the
#     kernel tokenizes the command line (argv[0] = path, rest = argv[1..]),
#     builds the System V process-start stack, and the ring-3 argtest reads
#     argc/argv/envp/auxv back (naked _start -> main(argc, argv, envp)).
#   - C2: fstest additionally asserts each failure class returns its specific
#     -errno (ENOENT/EBADF/EEXIST/ESRCH/ECHILD/EFAULT), because syscalls now
#     decode errors with the Linux convention instead of the old sentinel.
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
$p = Start-Process -FilePath $qemu `
    -ArgumentList @('-drive', "format=raw,file=$img", '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
    -WindowStyle Hidden -PassThru

function Wait-LogMarker($log, $pattern, $timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match $pattern })) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

[void](Wait-LogMarker $log '\[argtest\] (PASSED|FAILED)' 90)
[void](Wait-LogMarker $log 'entering interactive mode' 60)
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue
Write-Host '=== M9.6-C1 argv/envp/auxv ==='
$content | Where-Object { $_ -match '^\[argtest\]' } | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_ -match '^run: ' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] argc=4$' })) { $fail += 'argc=4 missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] argv\[0\]=/ARGTEST\.ELF' })) { $fail += 'argv[0] missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] argv\[1\]=alpha' })) { $fail += 'argv[1]=alpha missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] argv\[2\]=beta' })) { $fail += 'argv[2]=beta missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] argv\[3\]=gamma' })) { $fail += 'argv[3]=gamma missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] envc=0' })) { $fail += 'envc=0 missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] at_pagesz=4096' })) { $fail += 'at_pagesz=4096 missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] at_phent=56' })) { $fail += 'at_phent=56 missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] at_random=yes' })) { $fail += 'at_random=yes missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] at_execfn=/ARGTEST\.ELF' })) { $fail += 'at_execfn missing' }
if (-not ($content | Where-Object { $_ -match '^\[argtest\] PASSED' })) { $fail += 'argtest PASS marker missing' }
if ($content | Where-Object { $_ -match '^\[argtest\] FAILED' }) { $fail += 'argtest FAILED marker present' }
if (-not ($content | Where-Object { $_ -match '^run: exit code 0' })) { $fail += 'run: exit code 0 missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: PASSED')) { $fail += 'fstest PASSED marker missing' }
if (-not ($content | Select-String -SimpleMatch 'fstest: errno OK')) { $fail += 'fstest errno OK marker missing (C2)' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.6-C1/C2 ARGV/ENVP/AUXV + ERRNO PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}