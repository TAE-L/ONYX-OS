# test-lnx.ps1 - M9.7 regression: Linux-ABI (syscall shim + ET_DYN/PIE) end to end.
#   - build.rs compiles user/linuxtest with rustc --target x86_64-unknown-linux-gnu
#     and links it with rust-lld -shared -static: a REAL Linux-ABI static-PIE
#     (ET_DYN, preferred vaddrs of 0, R_X86_64_RELATIVE relocs, ONYXLNX marker).
#   - The shell's AUTOEXEC runs `run /LNXTEST.ELF`; the kernel detects the
#     Linux ABI, loads the image into the PIE region, applies the relocations,
#     and serves its raw Linux syscalls through the shim. The test probes:
#     brk, anonymous mmap (write-back), arch_prctl TLS (fs-relative load),
#     a relocated .rodata pointer (proves R_X86_64_RELATIVE was applied),
#     getpid, getrandom, clock_gettime - then exits 0 (exit_group).
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

[void](Wait-LogMarker $log '\[lnxtest\] (PASSED|FAILED)' 90)
[void](Wait-LogMarker $log 'entering interactive mode' 60)
Start-Sleep -Seconds 2
$selfExited = $p.HasExited
if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
Start-Sleep -Milliseconds 800

Write-Host "qemu self-exited (crash indicator): $selfExited"
$content = Get-Content $log -ErrorAction SilentlyContinue
Write-Host '=== M9.7 Linux-ABI shim + static-PIE ==='
$content | Where-Object { $_ -match '^(brk|mmap|tls|reloc|getpid|getrandom|clock): OK' } | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_ -match '^\[lnxtest\]' } | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_ -match '^userspace: (Linux-ABI|ET_DYN)' } | ForEach-Object { Write-Host "  $_" }
$content | Where-Object { $_ -match '^run: ' } | ForEach-Object { Write-Host "  $_" }

$fail = @()
if ($selfExited) { $fail += 'QEMU self-exited (crash)' }
if (-not ($content | Where-Object { $_ -match 'Linux-ABI ELF detected' })) { $fail += 'Linux-ABI detection missing' }
if (-not ($content | Where-Object { $_ -match 'ET_DYN load_base=0x2000000: applied [1-9]' })) { $fail += 'ET_DYN relocations not applied' }
foreach ($probe in 'brk', 'mmap', 'tls', 'reloc', 'getpid', 'getrandom', 'clock') {
    if (-not ($content | Where-Object { $_ -match "^$probe`: OK" })) { $fail += "$probe probe missing" }
}
if (-not ($content | Where-Object { $_ -match '^\[lnxtest\] PASSED' })) { $fail += '[lnxtest] PASSED marker missing' }
if ($content | Where-Object { $_ -match '^\[lnxtest\] FAILED' }) { $fail += '[lnxtest] FAILED marker present' }
if (-not ($content | Where-Object { $_ -match '^run: exit code 0' })) { $fail += 'run: exit code 0 missing' }
if ($content | Where-Object { $_ -match 'PANIC|EXCEPTION' -and $_ -notmatch 'Breakpoint' }) { $fail += 'PANIC or unexpected exception' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.7 LINUX-ABI SHIM + STATIC-PIE PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}
