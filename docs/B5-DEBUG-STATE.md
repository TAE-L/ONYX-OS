# B5 snapshot-hang — RESOLVED (2026-09-09)

Milestone: M9.6-B5 (perf instrumentation). Blocking bug: `test-perf.ps1`
hangs/fails ~1-in-4..1-in-6 runs at the ~3 s boot snapshot. **Root cause
found and fixed; verification runs below.**

## Root cause

`framebuffer::console_bytes` took the `CONSOLE` and `FB` spinlocks with
interrupts ENABLED, while ring-3 `SYS_WRITE` echoes and `keyboard::take_line`'s
typed-key echo re-acquire the same locks from **IF=0** syscall context:

1. Task A (e.g. the perf snapshot) holds `CONSOLE.lock()` mid-line-draw (~
   100–500 µs of pixel writes) with IF=1.
2. The LAPIC timer preempts A → A is Ready, still holding `CONSOLE`.
3. The next task (shell/fstest — constantly polling `SYS_READ(0)` via
   `take_line`, or echoing via `SYS_WRITE`) runs **at IF=0** and calls
   `console_bytes` → spins on `CONSOLE` forever.
4. IF=0 means the timer never fires again: A can never resume to release the
   lock → kernel-wide permanent freeze.

This is why the death point moved between runs (any perf line, or any
`SYS_WRITE` echo, could be the preempted holder) and why IRQs appeared alive
one instant (pit-diag) and gone the next. The codebase had already fixed the
identical pattern for `draw_clock` (framebuffer.rs: "a spinning IRQ on a lock
held by the task it just interrupted can never be released") and documented
the discipline in `serial.rs` ("the lock must never be held across a
preemption") — `console_bytes` just never got the treatment.

## Fix

`kernel/src/framebuffer.rs` `console_bytes`: the whole mirror — the
`CONSOLE` critical section AND the `refresh_cursor_save()` call (which takes
`FB`, also taken by the mouse IRQ's `move_cursor`) — is now wrapped in
`x86_64::instructions::interrupts::without_interrupts`. Invariant restored:
**FB/CONSOLE locks are only ever held at IF=0**, so no IRQ or IF=0 task can
ever spin on a lock held across a preemption. Per-call IF=0 window is one
glyph line (sub-ms); the LAPIC periodic timer coalesces ticks landing in it
(same as the pre-existing serial/draw_clock behaviour).

## Diagnostic scaffolding from the hunt

- `scheduler.rs` `stall_tick`: keep — two one-shot serial dumps (runaway
  non-RT keep ≥ 150 ticks; Normal task kept with nothing Ready ≥ 8 ticks).
  RT keeps excluded by design. Never fired for the real bug (correct: the
  scheduler was never stuck).
- `perf.rs` `print_snapshot` `[perfd] ok{i}` markers: REMOVE once the fix is
  confirmed over a long batch (kept during verification so a recurrence
  would pinpoint the line).

## Verification (all with the fix)

- Post-fix batch: **12/12** `test-perf.ps1` runs clean (`fixcheck` batch).
- Probe removed + final batch: **10/10** clean, 0 failures. Total post-fix:
  **22/22** with zero hangs, against a pre-fix rate of ~1-in-4..1-in-6.
- Timer perf hooks restored; `[perfd]` probes removed from `print_snapshot`.

## Follow-ups (non-blocking)

1. Timer-late histogram (avg ≈ 764 µs, p99 ≈ 2 ms, max > 10 ms at ~3 s) —
   review IRQ delivery lateness under serial load.
2. apic_ms vs pit_ticks drift within one boot (pit 100/apic 125 early →
   pit 1000/apic 3296 late): LAPIC ticks coalesce during long IF=0 windows.
   After this fix the remaining IF=0 windows are serial writes, line draws
   and the FS path; re-measure drift before deciding anything more.

## Historical artifacts

Failing logs: `%TEMP%\onyx-boot\stall-fail-5.log`, `stall3-fail-3.log`
(pre-fix evidence, including the `[stall]` dump false-positive on the
designed 300 ms RT burst that led to the RT-exclusion in `stall_tick`).

## Repro / run commands

```powershell
cd "c:\Users\ASUS\Desktop\PROJECT OS"
$env:RUSTUP_HOME = "$pwd\.toolchain\rustup"; $env:CARGO_HOME = "$pwd\.toolchain\cargo"
$env:PATH = "$pwd\.toolchain\cargo\bin;$pwd\.toolchain\rustup\toolchains\nightly-x86_64-pc-windows-gnu\bin;$env:PATH"
cargo build
foreach ($i in 1..10) {
  $out = powershell -ExecutionPolicy Bypass -File test-perf.ps1 2>&1
  Write-Host "REP$i exit=$LASTEXITCODE"
  if ($LASTEXITCODE -ne 0) { Get-Content "$env:TEMP\onyx-boot\serial.log" -Tail 30; break }
}
```

