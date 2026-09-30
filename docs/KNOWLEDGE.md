# OnyxOS — hard-won knowledge log

*Living document. Every non-obvious bug, wrong assumption, and gotcha discovered
while building this OS, so a future session (or a future me after a context
compaction) does not have to rediscover them the expensive way.*

Format per entry: **what I believed → what was true → the fix / the lesson.**

---

## 1. virtio-gpu 2D command codes (cost: ~1 session)

**Believed:** the task brief's table — `TRANSFER_TO_HOST_2D=0x0102`,
`SET_SCANOUT=0x0103`, `ATTACH_BACKING=0x0105`, `FLUSH=0x0106`.

**True:** the 2D block is a *contiguous enum* from `0x0100`
(`GET_DISPLAY_INFO=0x0100`), so:

```
0x0100 GET_DISPLAY_INFO      0x0104 RESOURCE_FLUSH
0x0101 RESOURCE_CREATE_2D    0x0105 TRANSFER_TO_HOST_2D
0x0102 RESOURCE_UNREF        0x0106 RESOURCE_ATTACH_BACKING
0x0103 SET_SCANOUT           0x0107 RESOURCE_DETACH_BACKING
```

**Symptom that exposed it:** the device refused my "attach" with `0x1205`,
*and* QEMU's own `guest_errors` log said `virtio_gpu_transfer_to_host_2d:
command size incorrect 48 vs 56` — the device had dispatched my attach to the
**transfer** handler. **Lesson:** when a device rejects a command, read the
*handler name* in the device's log; the error code alone is nearly useless.

## 2. The error-code enum is not what you guess

**Believed:** `0x1205` = `BAD_FORMAT` (a plausible-looking guess).
**True:** `0x1205` = `ERR_INVALID_PARAMETER`. The Linux UAPI header
(`include/uapi/linux/virtio_gpu.h`) is the authority:
`0x1200 UNSPEC, 0x1201 OUT_OF_MEMORY, 0x1202 INVALID_SCANOUT_ID,
0x1203 INVALID_RESOURCE_ID, 0x1204 INVALID_CONTEXT_ID, 0x1205 INVALID_PARAMETER`.

**Lesson:** never infer an error meaning. Read the UAPI header / spec. Always
log the raw code with a name from the header.

## 3. Pixel-format enum is off by one too

**Believed:** `B8G8R8X8 = 1`.
**True:** `B8G8R8A8=1, B8G8R8X8=2, A8R8G8B8=3, X8R8G8B8=4`. Sending 1 asks
for BGRA-**with alpha** — a different layout from the console's Bgr/32bpp.
Scanout uses **2** (X = no alpha); the **cursor** uses **1** (needs alpha for
transparency). **Lesson:** a format's *value* is arbitrary; read the enum.

## 4. Request lengths include the trailing padding

**Believed:** send the payload up to the last real field.
**True:** the device checks the descriptor length against the **full C struct**
(`VIRTIO_GPU_FILL_CMD` → `iov_to_buf(...) != sizeof(struct)`), so trailing
`__le32 padding` counts. Short by even 4 bytes = refused. `RESOURCE_FLUSH` is 48
(not 44), `TRANSFER_TO_HOST_2D` is 56 (not 52). **Lesson:** derive lengths
field-by-field from the UAPI struct, comment the arithmetic.

## 5. Editor tool: trailing newlines get trimmed (recurs constantly)

Inserting a block whose last line is `}` can lose that `}` — the tool trims
trailing whitespace, so a lone `}` that ends up as the final line is eaten.
This bit me repeatedly and left **stray/unbalanced braces** that looked like
"cannot find function X" or "unclosed delimiter". **Lesson:** after any
`insert_line`/append, run a brace-balance check (count `{`/`}`) before trusting
a build error. Also: a stray `}` at EOF can silently *close the module early*,
making later items "not found" — which looks like a missing function, not a
brace bug.

## 6. The stage-10 heap-jump crash: inert field, real race

**Believed:** adding `pinned_cpu: u8` to `Task` (unused at runtime) "broke"
the scheduler.
**True:** it exposed a *pre-existing* memory-corruption class. A task's context
is **split**: saved RSP in a heap `sp_slot`, kernel stack re-installed into the
running CPU's `TSS.RSP0`, FPU area elsewhere. `plan_switch` could migrate any
Ready task to any CPU, and `owner_cpu` was rewritten to `me` on every switch
(a *hint*, not a guarantee) — so two CPUs could touch one task's context. An
inert field changes struct layout/timing, which is all it takes to flip a latent
race into a jump-to-heap (`RIP` in `0x4444_4444_xxxx`). **Lesson:** when an
innocuous change breaks things, suspect a latent race, not your change.

## 7. Holding a spin lock across a context switch deadlocks

**Tried (unsound):** hold `SCHED_LOCK` across `context_switch` to close the
plan/switch TOCTOU. The switch *transfers control*; the lock guard is still live
on the outgoing frame, so the incoming task spins forever on the lock with
interrupts off. **Lesson:** never hold a lock across a context switch. Caught
this before committing and backed it out — by *reasoning about re-entry*, not by
running; the failure would have been a silent hang.

## 8. The floating-presenter vs pinned-presenter trap

Two contradictory "obvious" fixes for present pacing, both wrong:
- Pin the **presenter** to the BSP (stage 11 attempt): the BSP also runs shell +
  ring-3 + ticker; a pinned flusher is starved of them and ring-3 runs started
  faulting.
- Pin the **load** (frame clock) to an AP but leave the presenter floating:
  worked, but *only* once the affinity filter (item 9) existed.

**Lesson (now the rule):** **pin the LOAD, float the PRESENTER.** Only correct

## 9. Restoring CPU affinity = the safe floor, and what it cost

Restored the affinity filter in `plan_switch` (a task is only considered by its
`owner_cpu`) and stopped rewriting `owner_cpu` on switch. Now a task's context is
touched by exactly one CPU — safe by construction.

**What it broke and how it was fixed (both are legitimate patterns):**
- `test-avx` FXSAVE/SSE: the two fpu-test tasks both spawned on the BSP, so the
  affinity filter **serialized** them (~2x wall time) and blew the 40 s window.
  Fix: spawn fpu-test B on CPU 1. A test that *wants* parallelism must place its
  tasks on distinct CPUs explicitly now.
- `test-para`: bench workers all spawned on the BSP → `cpus=[0,0,0,0]`. It
  relied on the "claim-then-migrate" pattern. Fix: spawn worker `i` on CPU `i`.
  Result improved: **x2.65 → x4.6–4.8 speedup, 4 distinct CPUs** (affinity
  removes migration overhead).

**The honest caveat:** this is a *safety floor*, not the final design. Work
-stealing is gone; placement is explicit. The destination is the per-CPU
owned-context rewrite (P1 step A), which allows safe migration again.

## 10. The GPT parser could panic the whole kernel on a corrupt disk

**Believed:** the 48 unwrap/panic sites in the disk code were mostly fine.
**True:** `block.rs scan_gpt` read `entry_size`/`entry_count` off the GPT header
and used them to index, unvalidated: `entry_size==0` → `table[0..0]` then index
past it; `entry_size<128` → the name read `e[56..128]` out of range; `sectors as
u8` silently **truncated** a >255-sector array → under-read → index past;
`entry_count*entry_size` overflow; `last-first+1` underflow. Under `panic=abort`
a corrupt boot sector = **whole-kernel death**. Fixed with
`validate_gpt_geometry()` + chunked reads.
**Lesson:** `panic=abort` makes "unreachable" panics on *input-driven* paths into
a DoS. Validate every field read from media *before* it indexes anything.

## 11. Can't reach the GPT path by corrupting the image (why the self-test)

The GPT trigger is MBR partition 0's type byte `== 0xEE`, but on this image that
*same byte* is the **bootloader's own boot partition** (type `0x20`, LBA 1).
Setting it to `0xEE` to reach `scan_gpt` stops the kernel booting at all
(verified: zero serial output). So the validation is exercised by a boot-time
**self-test** (`block::gpt_selftest`) instead — deterministic, no crafted media.

## 12. Never put the word "panic" in a PASS log line

`test-fs` reported "PANIC or unexpected exception" while the kernel was healthy:
my gpt-selftest PASS line read `...rejected, no panic` — and test-fs greps the
serial log for `PANIC|EXCEPTION`, so it matched **my own success message**.
**Lesson:** when suites grep for error words, informational lines must avoid
those exact words.

once cross-CPU task execution is memory-safe (item 9).

## 13. Test-harness footguns (bit me repeatedly)

- **Concurrent suites kill each other.** Every `Invoke-Boot` does
  `Get-Process qemu | Stop-Process -Force`, so two overlapping suites produce
  "QEMU self-exited" or missing tail-markers. Run suites one at a time.
- **A 30 s boot window is timing-sensitive.** A busy host makes a boot miss
  `fstest: PASSED` / `autoexec done`. Re-run before believing a boot-flow failure.
- **Boot a COPY of the image.** Pointing QEMU at the shared target bios.img takes
  a write lock and the boot produces no serial output at all (looks like an
  abort; is not). Copy to $env:TEMP first.
- **PowerShell cd to the project path intermittently glitches** in long chains;
  the next command still runs. Keep commands one per invocation.
- Reading a huge file whole is slow; grep it instead.

## 14. Architecture facts that are NOT what the code suggests

- **The present path is fast; the cadence is the problem.** A single present is
  ~217-324 us; a hardware-cursor move ~97 us. Sustained frame pacing sat at
  5-11 fps from CPU scheduling/contention, not the driver. **Lesson:** measure
  the pure path separately from the end-to-end number, or you will chase the
  wrong bottleneck for days.
- **The 1 ms LAPIC tick is the only preemption source.** A 1 kHz game loop has a
  1 ms budget equal to the scheduler quantum; no headroom. Frame pacing needs a
  real deadline timer (P2).
- **No GUI layer exists** (no surface/blit/window/compositor code). The present
  path is welded to the text console. The font is 8x8 ASCII only; heap is
  16 MiB. These are the real desktop blockers.

## 15. What was rejected (do not retry)

- Same-CPU self-reschedule IPI (crash: #GP / null context).
- spawn_on_cpu pinning without a real affinity filter (crash on the non-virgl
  virtio boot).
- Holding SCHED_LOCK across context_switch (deadlock, item 7).
- Corrupting the image to reach the GPT parser (breaks boot, item 11).

---

# ROADMAP: now -> a working desktop

*Accepted direction: P1 first (scheduler safety), then desktop-first.*
Ground rules: measure before optimizing; every step keeps the full suite green.

**Step 0 - DONE.** P1 step B (CPU affinity: contexts single-owner), P3 (disk
panic hardening + self-test), repo hygiene. Present path proven sub-ms.

**Step 1 - P1 step A: per-CPU owned task contexts. (NEXT)**
Make a task context ONE allocation owned by the task, tagged with its current
CPU; a context switch touches only the owning CPU. Goal: allow SAFE migration
(work-stealing) again, so render and present can be separate threads.
Done when: a task can move between CPUs at a defined safe point, with
test-para / test-avx / test-smp / test-gpu all still green.

**Step 2 - P2: deadline timer.**
Replace the 1 ms tick as the only preemption source with a per-CPU min-heap of
deadlines armed in the LAPIC/TSC for the next due task. Unlocks real frame
pacing (60/120/240 Hz) and the compositor frame clock. Measure: jitter drops,
frame clock hits target.

**Step 3 - P6a: drawing substrate.**
A framebuffer surface an app can own (not console_bytes) + blit primitives
(filled rect, alpha blend, line) + a scalable/Unicode font (current is 8x8 ASCII
only - a hard desktop blocker). Optionally grow the 16 MiB heap.

**Step 4 - P6b: compositor.**
A kernel task owning the frame, presenting on the step-2 frame clock, reading
SYS_INPUT_READ and dispatching to a client. Start single-fullscreen-client (a
legitimate first desktop; z-ordering later).

**Step 5 - P6c: first desktop client.**
Taskbar + clock + one window, fed by the compositor. The first thing a user
would call a desktop.

**Carried caveats:** pin the LOAD / float the PRESENTER (item 8); the present
path is already sub-ms - the wins are in the scheduler (steps 1-2) and the
substrate (step 3).

---

## 16. P1 step A (owned contexts) - ATTEMPTED, REVERTED, do not redo blindly

**Goal.** Collapse a task context (saved SP + kernel stack + FPU image) into ONE
owned `TaskCtx` allocation so a context is touched by exactly one CPU and safe
migration/work-stealing can return.

**What I tried.** Added `struct TaskCtx { sp: u64, fpu: &static mut FpuArea,
stack: [u8; 64K], cpu: u8 }`, boxed it in `Task.ctx`, and pointed `sp_slot()`,
`fpu_ptr()` and the `kstack_top` TSS update at the box.

**Result: a double-fault (#DF) on the very first context switch, on every CPU
count (even -smp 1).** Reverted with `git checkout -- kernel/src/scheduler.rs`;
the base is green again. Nothing was committed, so nothing is lost.

**Two traps hit along the way (both cost time):**
- `fpu::new_area()` returns `&'static mut FpuArea` (a leaked Box), NOT an inline
  FpuArea - so the field type is a reference, and it cannot be `Box::new(...)`ed
  again. Reading it out of a `static mut Vec` needs `addr_of_mut!(..).read()` (you
  cannot form a `&mut` through a Vec index).
- Repeated PowerShell `Set-Content -Encoding UTF8` edits prepend a UTF-8 BOM
  (EF BB BF) to the file and mangle em-dashes in comments. A BOM before `//!` is
  a Rust compile error waiting to happen. After heavy scripted editing, verify
  the first bytes are `47,47,33` (`//!`), not `239,187,191`.

**Lesson / how to redo it.** The double fault is a REAL bug in the restructure,
not the encoding (the base builds and runs). The likely cause to check FIRST:
the initial `sp` is written by `prepare_stack(&mut ctx.stack)` BEFORE the `Box` is
moved into `Task` - a Box move does not move its heap contents, so the sp should
stay valid, BUT the `kstack_top` (TSS.RSP0) and the saved `sp` must be derived
from the SAME box. Do it one small step at a time with a build+smp test after
each, and never do a multi-site scripted rewrite of the scheduler in one go.

### ROOT CAUSE of the #DF (found 2026-09-26, after landing the validator)

**It was almost certainly NOT a pointer/alignment bug. It was a 66 KB stack
temporary.**

`TaskCtx` held `stack: [u8; STACK_SIZE]` INLINE and was built as
`Box::new(TaskCtx { stack: [0u8; STACK_SIZE], ... })`. That struct literal
materialises the WHOLE ~66 KB struct (64 KiB stack + 2 KiB FPU) as a temporary in
the spawning frame *before* copying it to the heap. `STACK_SIZE` is 64 KiB, so
the temporary cannot fit in a 64 KiB kernel stack: the spawn smashed the stack
and the damage only became visible at the next context switch - exactly the
observed signature (dies right after `M3: spawning tasks...`, a #DF at an
unexplainable RIP, on `-smp 1` too, where nothing SMP-specific exists). The #DF
then lands on `DOUBLE_FAULT_STACK_SIZE` (16 KiB, gdt.rs), which is why it reports
like a hardware fault.

The existing code avoids this by accident: it uses
`vec![0u8; STACK_SIZE].into_boxed_slice()` + `Box::leak`, and `vec![]` allocates
on the HEAP. The trap is specifically "put the big array in a struct literal".

**Consequence for step 2b:** a single-allocation context must NOT be built from an
inline array in a struct literal. Either allocate the box first and initialise it
in place through the `Box` (e.g. `alloc::alloc::alloc_zeroed(Layout::new::<TaskCtx>())`
then write fields via raw pointers), or keep the stack as a separate heap slice
and accept 2 allocations instead of 3 (still a real improvement, and safe). Do
NOT "fix" it by enlarging stacks - the temporary IS the bug.

**RESOLVED - the diagnosis was correct and step 2b landed on `6ca1ba9`.**
`new_ctx()` does exactly the prescribed in-place construction
(`alloc_zeroed(Layout::new::<TaskCtx>())` + raw-pointer field writes) and the
one-allocation context now boots, schedules and switches with zero `#DF`. So the
66 KB stack temporary was the whole cause, not a red herring.

The same trap bit a SECOND time in miniature: `fpu::seed_in_place()` originally
did `area.bytes = src.bytes`, and array assignment of a 2048-byte array can build
a temporary. It now uses `copy_from_slice` (a `memcpy` straight from one heap
location to another). **Rule of thumb for this kernel: any copy of a
stack-sized or FpuArea-sized value is suspect - use `memcpy`-style APIs and
write through pointers.**

Third instance of the same *class* of bug, worth remembering: a `#DF` or silent
corruption after a "safe looking" refactor is usually a large value passing
through the stack, not a pointer/alignment mistake. Check for big temporaries
first.

### Facts established by the validator (all measured, not assumed)

- **A saved RSP is only guaranteed 8-byte aligned.** Its 16-byte parity VARIES
  with the preemption point: observed both `...4c0` (0 mod 16) and `...868`
  (8 mod 16) on healthy runs. The familiar "rsp % 16 == 8" rule describes a
  function entry after a `call` pushed a return address - it does not apply to a
  raw saved RSP. Nothing on a resumed task may assume 16-byte stack alignment
  (this matters for the future compositor/SSE spill paths).
- **`sp_slot` is a pointer TO the slot.** `t.sp_slot as u64` yields the slot's
  own heap address, not the saved SP; use `t.sp_slot.read_volatile()`. The tell
  was saved "sp" values spaced exactly 16 bytes apart in the heap.
- **`&raw const task.field` measures the FIELD, not what it points at.** For
  `fpu_area: Box<FpuArea>`, `&raw const t.fpu_area` is the address of the Box
  inside the `Task` struct in the Vec (arbitrary alignment); the image address
  needs `(*b).as_ref()` / `(*b).as_mut()`, mirroring `fpu_ptr`.

### Test-harness trap (cost a false regression report)

Launching two `test-gpu.ps1` runs back-to-back makes them fail EVERY variant
("boot flow broken") because each script kills all QEMU processes on start. A
serial log that simply STOPS mid-boot (no #DF, no exception, truncated) is the
signature. Wait for the previous run's processes to be gone before starting the
next - the "run strictly sequentially" rule is not optional.

### The LAPIC "closed-loop" calibration is fighting a 5x clock skew (M10b P2, measured)

`calibrate_task` in `apic.rs` measures how many LAPIC ticks actually fire per 50
PIT ticks and rescales the interval. It compares that count `dm` against **50**.
But 50 PIT ticks at 100 Hz is **500 ms**, and `dm` counts ticks *in that window*,
so the correct divisor is 500. As written, `first_guess * dm / 50` MULTIPLIES the
interval by ~`dm/50` instead of dividing by the error ratio.

Observed live (`test-latency`): `249 ticks/500ms -> interval 2474761 (first guess
496940)` - a **4.98x LONGER** period. So the nominal "1 ms" preemption tick was
really running at ~5 ms, and every `sleep_current` deadline, `slice_left` quantum
and `fresh_slice` unit is denominated in that stretched tick. Fixing the divisor
to 500 makes the correction move the right way (`495447 -> 249046`).

Note what this does and does NOT explain: the ~200 ms `wake/schedule` figure is
NOT explained by the slow tick alone - a 5x-stretched tick predicts ~5 ms of
wake granularity, not 200 ms, and the same ~200 ms appears on BOTH sides of the
A/B. So the tail has some other dominant cause, and this divisor is a real
correctness wart worth fixing on its own merits - but it is not the P2 answer.

**But do not ship that fix blind - it measurably COSTS throughput.** A/B on the
same host, `test-para.ps1` (this is the clean, repeatable measurement):

| | serial baseline | -smp 4 speedup |
|---|---|---|
| as-is (`/50`) | 641-710 ms | **x4.08 - x4.58** |
| fixed (`/500`) | 2617-2944 ms | **x1.6 - x1.77** (test-para FAILS its x2 gate) |

Making the timer tick ~5x more often multiplies preemption/interrupt overhead and
made the serial baseline ~4x slower, collapsing the parallel speedup below the
x2 threshold. Reverted; the as-is numbers were re-confirmed afterwards.

**Correction - a latency claim I had to retract.** My first note here claimed the
fix took pacing from ~6 fps to 53-80 fps. That comparison was INVALID: the ~6 fps
figure was sampled from the IDLE phase of the window (idle console, ~1 clock
redraw/sec) while the 53-80 fps figure came from the ACTIVE phase. Re-running
`test-latency` on the UNMODIFIED kernel also shows 74-82 fps in its active phase
and ~0.8 fps in its idle phase. So the apparent 13x latency win was an artifact
of comparing two different phases, not a real effect. The only conclusion
supported by repeatable A/B is the test-para throughput regression above.

**The lesson:** the whole scheduler is calibrated in "LAPIC ticks", so this one
divisor silently rescales every duration in the kernel. Any change to tick rate
must be A/B'd against BOTH `test-latency` (latency) and `test-para` (throughput)
- optimising either one alone silently destroys the other. And always compare like
with like: latency phases must be compared against the SAME phase, or the
conclusion is fiction. The eventual P2 answer is probably a *decoupled* deadline
timer (fine-grained wakeups for sleepers) while leaving the *preemption* tick
coarse, rather than making one tick serve both jobs.

After the folder rename `PROJECT OS` -> `PROJECT_OS`, a clean rebuild
(`Remove-Item -Recurse -Force target` + `build.ps1`, EXIT=0) was followed by
`test-smp.ps1` reporting **FAILED** on all three `-smp` settings. Every single
failure was a *missing marker* (`fstest: PASSED`, `fpu-test: task A PASSED`,
`[argtest] argc=4`), never a corruption report: zero `#DF`, zero `[ctxcheck]`,
zero `PANIC`, and the only `EXCEPTION` was the `Breakpoint` the script already
excludes. A 90 s diagnostic boot showed the markers present at lines 311 / 323 /
**513** - the kernel was completely healthy, it was just slower than the flat
`Start-Sleep -Seconds 30/45` the harness allowed. This box is 4 logical cores
and TCG multiplies the cost, so boot time scales with whatever else is running.

**The lesson that generalises:** a test that asserts "the log contains marker X"
must also *wait* for marker X. A wall-clock sleep is a race against host load, and
when it loses, the failure looks exactly like a kernel regression - which is the
worst possible failure mode, because it invites "fixing" healthy code. Before
concluding that a refactor/rename broke something, check whether the symptom is
*missing output* (too slow) or *corrupt output* (actually broken); only the
latter is a kernel bug. `Invoke-Boot` now polls the log every 5 s until all its
`-Wait` markers appear, with the old duration kept only as a ceiling, so a busy
host can no longer manufacture a red suite.

### The `wake/schedule` tail was a MEASUREMENT bug, then a real one

`flush_loop` stamped `let wake_ns = now_ns()` at the TOP of its loop, **before**
`sleep_kernel(sleep_ms)` - while the comment directly above it said "stamp the
moment we come back from sleep". So the reported `wake/schedule = wake_ns -
damage_marked` was really *"the previous iteration's sleep quantum + the previous
submit"*: the flusher's own idle back-off, charged to the scheduler. That is the
phantom ~200 ms tail that got attributed to tick granularity. The stamp now lives
immediately after `sleep_kernel` returns, so the metric measures what it claims.

Worth stating plainly: **fixing the metric did not shrink the tail.** Post-fix
serial still shows `wake/schedule n=2 avg=90958 us max=181864 us` and pacing
~6.4 fps. That is the useful outcome - the number is now honest, and the tail is
GENUINE scheduling latency, not a measurement artifact and not the LAPIC tick
(which cannot explain ~200 ms from a ~1-5 ms tick anyway).

Where the time actually goes, from reading the code:

- `mark_dirty_rect` only calls `wake_present_on_damage()` on the
  **empty -> non-empty transition** (`framebuffer.rs:230`). A burst keeps the box
  non-empty, so it wakes ONCE; the rest of the burst is picked up on the flusher's
  next quantum. So most damage does not get an event-driven wake at all.
- `wake_task_now` promotes Sleeping -> Ready and calls `smp::kick_others()`, which
  IPIs *other* CPUs only. It deliberately does NOT reschedule the local CPU
  (`scheduler.rs:817-825`, a conscious choice to avoid a #GP from an IPI landing
  inside the scheduler lock). So a same-CPU damage wake waits out a full tick.
- The affinity filter (`plan_switch:1279`) pins the flusher to its birth CPU, so
  the local-CPU case is the COMMON case, not the rare one.

**The lesson:** fix the instrument before you tune against it, and re-measure
after - a corrected metric that does NOT move the number is a strong result,
because it eliminates a whole class of hypothesis. Also: a metric computed
one line away from where its comment says it is computed will quietly measure the
wrong thing forever, and the wrong thing will look plausible enough to build a
whole theory on.

### Instrumented park time: tick quantisation is REAL, local-wake is NOT

Added `[vgpu] sleep:` - requested vs ACTUAL `sleep_kernel` duration in the
flusher. This is the direct test of "is the tail tick quantisation?":

```
[vgpu] sleep: n=50 req avg=1900 us, ACTUAL avg=3640 us max=31410 us (2x overshoot)
```

So quantisation IS real: a 1-16 ms request actually costs ~2x, and the 16 ms
idle back-off really costs up to ~40-54 ms. My earlier claim that "a ~1-5 ms tick
cannot explain a 200 ms tail" was right about magnitude and wrong to use as a
dismissal - the overshoot compounds on every back-off sleep.

But it is **not sufficient**: max park (~40 ms) is still well under max
`wake/schedule` (~180 ms), so something else contributes too. This is why the
P2 deadline timer is *necessary but not sufficient*.

**Tried and REVERTED: self-IPI on a same-CPU wake.** The theory was strong - the
affinity filter pins the flusher to its birth CPU, so the local-CPU case is the
COMMON one, and `wake_task_now` deliberately declined to reschedule locally
(`scheduler.rs:817`), so a damage wake should wait out a full tick. Added
`smp::kick_self()` (an IPI, not a synchronous `preempt`, so it cannot abandon a
caller's critical section) and called it from `wake_task_now`.

Measured result: **worse, not better.** avg actual park rose ~3770 us ->
~4750 us, with no pacing improvement (6.4 -> 6.5 fps, noise). The extra IPI per
wake is pure overhead. Reverted the call; `kick_self` stays in `smp.rs` unwired,
since it is the correct SHAPE for a safe local reschedule if a future change ever
needs one. So the local-wake gap is NOT the tail either.

That leaves the tail unexplained by either mechanism, which is the honest state.
The next lead is `mark_dirty_rect`: it only wakes the flusher on the
empty -> non-empty transition, so a burst of damage gets ONE wake and the rest
waits for a poll quantum. Under a 2x-quantised tick that compounds.

**The lesson:** a plausible mechanism plus a strong prior is still not evidence.
Both of the obvious fixes here (tick divisor, local self-IPI) were wrong, and
only the instrument separated them from the truth in one boot each. Also: a
reverted experiment is a RESULT - write down what was tried and what it measured,
or the next session will re-try it.

### RETRACTED: "hop B is zero, the wake is never fired" was MY OWN BUG

The wake-latency split first reported `A(no-wake) avg=39611..101177 us` and
`B(sched) avg=0 us` - a clean, confident, and **completely wrong** conclusion
that the scheduler was not the bottleneck and the wake never fired. It was an
artifact of the order of two statements in `mark_dirty_rect`:

```rust
if was_empty { wake_present_on_damage(); }   // stamped T1
DIRTY_GEN.fetch_add(1, ...);
if DIRTY_MARK_NS.load(..) == 0 { DIRTY_MARK_NS.store(now_ns(), ..) }  // stamped T2
```

The damage stamp (`T2`) was taken AFTER the wake stamp (`T1`), so the guard
`wns > damage_marked` was never true, every sample fell into the else-branch,
and hop B was **structurally forced to zero**. Reordering the stamps (damage
stamp first, then generation, then wake) gives the mirror-image answer:

```
[vgpu] response: wake split n=2 | A(no-wake) avg=2 us max=3 us | B(sched) avg=82611 us max=165151 us
[vgpu] response: wake split n=4 | A(no-wake) avg=1 us max=2 us | B(sched) avg=53491 us max=213808 us
```

**The truth: the wake fires in 1-4 us, and essentially the ENTIRE tail
(avg 53-82 ms, max ~214 ms) is HOP B - the flusher waiting to get the CPU
back after being woken.** So the scheduler/reschedule path IS the bottleneck
after all.

This also explains the earlier confusing pair of results. The self-IPI
(`kick_self`) was aimed at hop B, which is the right target - but it
*increased* total IPI load and measured worse, so "worse" there was not
evidence that hop B is unimportant; it was evidence that *that particular*
implementation was bad. The tick divisor is likewise aimed at hop B (it
shortens the quantum the flusher waits out) and DID regress throughput - a
real latency-vs-throughput trade, not a wrong target.

**The lesson, and it is the important one:** a metric whose guard depends on the
ORDER of two timestamps can silently report a hard-coded zero, and a zero is
the most convincing-looking result there is - it reads as "this dimension is
eliminated", which is exactly the kind of claim that stops further
investigation. This is the SAME failure shape as the `wake_ns` stamp one commit
earlier (taken before the sleep instead of after). Before believing any
"dimension is zero" result, ask what would make it zero *by construction*, and
confirm the metric can print a NON-zero value at all.
