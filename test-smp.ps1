# test-smp.ps1 - M9.8 regression: SMP (AP bring-up, per-CPU state + timers).
#
# Boots the same image three times and checks the multi-core contract:
#   -smp 4 : MADT lists 4 LAPICs; AP1..AP3 come online through INIT-SIPI-SIPI,
#            each with its own GDT/TSS, IDT, CR0/CR4/EFER mirror, SYSCALL MSRs
#            and LAPIC timer; each runs its own worker task in parallel; the
#            per-CPU timer tick counters advance independently; the system
#            stays alive and the earlier milestones' regressions still pass.
#   -smp 2 : exactly one AP comes online (the MADT list is followed, not a
#            hardcoded CPU count).
#   -smp 1 : NO AP is started - the single-CPU path (all earlier milestones'
#            tests) must be unchanged.
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$qemu = Join-Path $root '.toolchain\qemu\qemu-system-x86_64.exe'
$img  = Join-Path $root 'target\debug\images\bios.img'
$fail = @()

function Invoke-Boot([int]$smp, [int]$seconds, [string]$tag) {
    $tmp = Join-Path $env:TEMP "onyx-smp-$tag"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $image = Join-Path $tmp 'bios.img'
    $log = Join-Path $tmp 'serial.log'
    Copy-Item $img $image -Force
    Remove-Item $log -ErrorAction SilentlyContinue
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    $p = Start-Process -FilePath $qemu `
        -ArgumentList @('-smp', "$smp", '-m', '512M', '-drive', "format=raw,file=$image",
                        '-display', 'none', '-serial', "file:$log", '-no-reboot', '-snapshot') `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $seconds
    $exited = $p.HasExited
    if (-not $exited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 600
    return [pscustomobject]@{ Log = (Get-Content $log -ErrorAction SilentlyContinue); Exited = $exited }
}

function Count-Of($lines, [string]$pattern) {
    return ($lines | Select-String -SimpleMatch $pattern | Measure-Object).Count
}
# ---------------------------------------------------------------------------
# Case 1: -smp 4 (the full milestone)
# ---------------------------------------------------------------------------
Write-Host '=== M9.8 SMP: -smp 4 ==='
$r = Invoke-Boot 4 45 '4'
$c = $r.Log
Write-Host "qemu self-exited (crash indicator): $($r.Exited); log lines: $($c.Count)"
$c | Where-Object { $_ -match '^\[smp\]' } | ForEach-Object { Write-Host "  $_" }

if ($r.Exited) { $fail += '[-smp 4] QEMU self-exited (triple fault / reset)' }
if ((Count-Of $c '[smp] AP trampoline page reserved at') -lt 1) { $fail += '[-smp 4] trampoline page not reserved below 1 MiB' }
if ((Count-Of $c '[smp] trampoline page 0x') -lt 1) { $fail += '[-smp 4] trampoline not installed/identity-mapped' }
if ((Count-Of $c '[smp] MADT lists 4 LAPIC(s)') -lt 1) { $fail += '[-smp 4] MADT did not report 4 LAPICs' }
if ((Count-Of $c '[smp] AP: reached 64-bit ap_entry') -lt 3) { $fail += '[-smp 4] fewer than 3 APs reached 64-bit mode' }
foreach ($cpu in 1..3) {
    if ((Count-Of $c "[smp] cpu $cpu online (lapic id") -lt 1) { $fail += "[-smp 4] cpu $cpu did not come online" }
    if ((Count-Of $c "[apic] cpu $cpu LAPIC enabled") -lt 1) { $fail += "[-smp 4] cpu $cpu LAPIC timer not armed" }
    if ((Count-Of $c "[smp] cpu $cpu worker task running") -lt 1) { $fail += "[-smp 4] cpu $cpu worker task never ran" }
    if ((Count-Of $c "[smp] cpu $cpu alive: worker iters") -lt 1) { $fail += "[-smp 4] cpu $cpu worker produced no heartbeat" }
}
if ((Count-Of $c '[smp] 4 CPU(s) online after bring-up') -lt 1) { $fail += '[-smp 4] 4-CPU summary missing' }
if ((Count-Of $c 'worker FAILED') -gt 0) { $fail += '[-smp 4] an AP worker detected FPU/SSE corruption' }
# Per-CPU timer liveness: every AP's own LAPIC timer must have ticked.
foreach ($cpu in 1..3) {
    $m = $c | Select-String -Pattern "\[smp\] cpu ${cpu}: lapic id .*timer ticks (\d+)" | Select-Object -First 1
    if (-not $m -or [int]$m.Matches[0].Groups[1].Value -lt 1) {
        $fail += "[-smp 4] cpu $cpu per-CPU timer tick counter did not advance"
    }
}
# The rest of the system must survive the bring-up: regressions + liveness.
if ((Count-Of $c 'fstest: PASSED') -lt 1) { $fail += '[-smp 4] fstest PASSED marker missing' }
if ((Count-Of $c 'fpu-test: task A PASSED') -lt 1) { $fail += '[-smp 4] fpu-test A PASSED marker missing' }
if ((Count-Of $c '[argtest] argc=4') -lt 1) { $fail += '[-smp 4] argv regression missing' }
if ((Count-Of $c 'PANIC') -gt 0) { $fail += '[-smp 4] PANIC occurred' }
$unexpected = $c | Where-Object { $_ -match 'EXCEPTION' -and $_ -notmatch 'Breakpoint' }
if ($unexpected) { $fail += "[-smp 4] unexpected exception: $($unexpected[0])" }
if ((Count-Of $c '[stall]') -gt 0) { $fail += '[-smp 4] scheduler stall diagnostic fired' }
# ---------------------------------------------------------------------------
# Case 2: -smp 2 (follows the MADT count, not a hardcoded CPU count)
# ---------------------------------------------------------------------------
Write-Host '=== M9.8 SMP: -smp 2 ==='
$r2 = Invoke-Boot 2 30 '2'
$c2 = $r2.Log
Write-Host "qemu self-exited: $($r2.Exited); log lines: $($c2.Count)"
$c2 | Where-Object { $_ -match 'online \(|\[smp\] 2 CPU' } | ForEach-Object { Write-Host "  $_" }

if ($r2.Exited) { $fail += '[-smp 2] QEMU self-exited' }
if ((Count-Of $c2 '[smp] MADT lists 2 LAPIC(s)') -lt 1) { $fail += '[-smp 2] MADT did not report 2 LAPICs' }
if ((Count-Of $c2 '[smp] cpu 1 online (lapic id') -lt 1) { $fail += '[-smp 2] cpu 1 did not come online' }
if ((Count-Of $c2 'starting cpu 2') -gt 0) { $fail += '[-smp 2] tried to start cpu 2 (no LAPIC for it)' }
if ((Count-Of $c2 '[smp] 2 CPU(s) online after bring-up') -lt 1) { $fail += '[-smp 2] 2-CPU summary missing' }
if ((Count-Of $c2 'fstest: PASSED') -lt 1) { $fail += '[-smp 2] fstest PASSED marker missing' }

# ---------------------------------------------------------------------------
# Case 3: -smp 1 (single-CPU path must be untouched)
# ---------------------------------------------------------------------------
Write-Host '=== M9.8 SMP: -smp 1 (single-CPU regression) ==='
$r1 = Invoke-Boot 1 30 '1'
$c1 = $r1.Log
Write-Host "qemu self-exited: $($r1.Exited); log lines: $($c1.Count)"
$c1 | Where-Object { $_ -match '^\[smp\]' } | ForEach-Object { Write-Host "  $_" }

if ($r1.Exited) { $fail += '[-smp 1] QEMU self-exited' }
if ((Count-Of $c1 '[smp] MADT lists 1 LAPIC(s)') -lt 1) { $fail += '[-smp 1] MADT did not report a single LAPIC' }
if ((Count-Of $c1 '[smp] cpu 1 online') -gt 0) { $fail += '[-smp 1] an AP was started with -smp 1' }
if ((Count-Of $c1 '[smp] 1 CPU(s) online after bring-up') -lt 1) { $fail += '[-smp 1] 1-CPU summary missing' }
if ((Count-Of $c1 'bring-up complete:') -lt 1) { $fail += '[-smp 1] bring-up task did not finish' }
if ((Count-Of $c1 'fstest: PASSED') -lt 1) { $fail += '[-smp 1] fstest PASSED marker missing' }
if ((Count-Of $c1 'fpu-test: task A PASSED') -lt 1) { $fail += '[-smp 1] fpu-test A PASSED marker missing' }
if ((Count-Of $c1 '[argtest] argc=4') -lt 1) { $fail += '[-smp 1] argv regression missing' }
if ((Count-Of $c1 'PANIC') -gt 0) { $fail += '[-smp 1] PANIC occurred' }
if ((Count-Of $c1 '[stall]') -gt 0) { $fail += '[-smp 1] scheduler stall diagnostic fired' }

if ($fail.Count -eq 0) {
    Write-Host 'RESULT: M9.8 SMP TESTS PASSED' -ForegroundColor Green
    Exit 0
} else {
    Write-Host 'RESULT: FAILED:' -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" }
    Exit 1
}
