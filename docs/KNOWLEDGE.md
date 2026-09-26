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
