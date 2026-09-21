# test-avx.ps1 - M9.8-f XSAVE/AVX regression suite.
#
# Covers three things that only a boot can prove:
#   1. CPUID/XCR0 bring-up: on a CPU with XSAVE+AVX the kernel must report
#      "fpu: XSAVE enabled (XCR0=0x7, ... AVX=yes)" and spawn the AVX task; on a
#      CPU without XSAVE it must fall back to FXSAVE and *not* run AVX code.
#   2. YMM preservation across context switches: the AVX task keeps 16 live YMM
#      lanes and verifies them after rounds that were preempted dozens of times
#      ("fpu: avx PASS round N (seed S, 16 YMM lanes intact across N switches)").
#      This is the test that caught the async-IRQ XMM clobber (see IrqFpuGuard).
#   3. Per-CPU XCR0: APs must program XCR0 themselves before taking tasks, so
#      the run is done with -smp 4.
[CmdletBinding()]
param(
    [string] $Cpu = 'max',
    [int]    $Seconds = 40
)
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'

function Run-Boot([string] $cpu) {
    $tmp = Join-Path $env:TEMP 'onyx-boot'
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $img = Join-Path $tmp 'bios.img'
    $log = Join-Path $tmp 'serial.log'
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Copy-Item (Join-Path $root 'target\debug\images\bios.img') $img -Force
    Remove-Item $log -ErrorAction SilentlyContinue

    $p = Start-Process -FilePath $qemu `
        -ArgumentList @('-smp', '4', '-cpu', $cpu, '-m', '512M', '-drive', "format=raw,file=$img",
                        '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $Seconds
    $selfExited = $p.HasExited
    if (-not $selfExited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 800
    return [pscustomobject]@{ Cpu = $cpu; SelfExited = $selfExited; Lines = (Get-Content $log -ErrorAction SilentlyContinue) }
}

$fail = @()

# --- 1. AVX-capable CPU: the XSAVE path must be live and YMM must survive. ---
$r = Run-Boot $Cpu
Write-Host "=== -cpu $($r.Cpu) (self-exited: $($r.SelfExited)) ==="
$r.Lines | Select-String -Pattern 'fpu:|fpu: avx|PANIC|DOUBLE FAULT' |
    Select-Object -Last 12 | ForEach-Object { $_.Line }

if ($r.SelfExited) { $fail += "-cpu $($r.Cpu): QEMU self-exited (crash)" }
if (-not ($r.Lines | Select-String -SimpleMatch 'fpu: XSAVE enabled (XCR0=0x7')) {
    $fail += "-cpu $($r.Cpu): XSAVE/XCR0 bring-up line missing"
}
if (-not ($r.Lines | Select-String -SimpleMatch 'AVX=yes')) { $fail += "-cpu $($r.Cpu): AVX not reported enabled" }
$passes = @($r.Lines | Select-String -Pattern 'fpu: avx PASS round')
$fails  = @($r.Lines | Select-String -Pattern 'fpu: avx FAILED')
$weaks  = @($r.Lines | Select-String -Pattern 'fpu: avx WEAK round')
if ($passes.Count -lt 3) { $fail += "-cpu $($r.Cpu): fewer than 3 verified AVX rounds ($($passes.Count))" }
if ($fails.Count -gt 0) {
    $fail += "-cpu $($r.Cpu): YMM state corruption reported ($($fails.Count) lines)"
    $r.Lines | Select-String -Pattern 'fpu: avx +lane' | Select-Object -First 8 | ForEach-Object { Write-Host "    $($_.Line)" }
}
# A WEAK round is a round that finished before a single preemption, i.e. it
# proves nothing; it is allowed occasionally, but not for most rounds.
if ($weaks.Count -gt $passes.Count) { $fail += "-cpu $($r.Cpu): most AVX rounds never got preempted (test not exercising switches)" }
# Interrupt handlers must not disturb the interrupted task (the bug this test
# found): no exception other than the intentional M1 breakpoint demo.
if ($r.Lines | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }) {
    $fail += "-cpu $($r.Cpu): unexpected exception"
}

# --- 2. CPU without XSAVE: must fall back cleanly and never run AVX. ---
$r2 = Run-Boot 'qemu64'
Write-Host "=== -cpu $($r2.Cpu) (self-exited: $($r2.SelfExited)) ==="
$r2.Lines | Select-String -Pattern 'fpu:|fpu-avx' | Select-Object -Last 5 | ForEach-Object { $_.Line }

if ($r2.SelfExited) { $fail += "-cpu qemu64: QEMU self-exited (crash)" }
if (-not ($r2.Lines | Select-String -SimpleMatch 'fpu: FXSAVE fallback')) {
    $fail += "qemu64: expected the FXSAVE fallback message"
}
if (-not ($r2.Lines | Select-String -SimpleMatch 'fpu-avx test not spawned')) {
    $fail += "qemu64: the AVX task must not be spawned without AVX"
}
if ($r2.Lines | Select-String -Pattern 'fpu: avx (PASS|FAILED)') {
    $fail += "qemu64: AVX instructions ran on a CPU without AVX"
}
if (-not ($r2.Lines | Select-String -SimpleMatch 'fpu-test: task A PASSED')) {
    $fail += "qemu64: the SSE FPU regression task did not pass on the FXSAVE path"
}

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.8-f XSAVE/AVX TESTS PASSED' -ForegroundColor Green
    exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
