# OnyxOS — architectural analysis for a low-latency gaming OS

*Written after M10b stage 10. This is a pause-and-assess document: the GPU/present
track is instrumented and (mostly) working, and the last two increments exposed
structural problems in the SMP/task substrate that a "make it faster" mindset
cannot fix. This lays out what is actually wrong and what the fix is.*

## TL;DR

The present path is **fast** (a full present is ~217–324 µs; a hardware-cursor
move is ~97 µs — both measured). The OS is **not** fast, because its execution
model cannot *reliably run a task to completion on a chosen core*, and because
latency is quantized to a 1 ms scheduler tick. Two structural facts dominate
everything measured:

1. **The scheduler has no real CPU affinity, and cross-CPU task execution is not
   memory-safe.** Adding one `u8` field to `Task` (stage 10, unused at runtime)
   flipped a previously-passing boot into a crash that jumps into the heap
   (RIP=`0x4444_4444_xxxx`). That is a Heisenbug: a latent use-after-free or
   racy pointer in the context-switch path, exposed by a struct-layout shift.
   A task's `sp_slot` lives in a heap box and its kernel stack / FPU area are
   re-pointed per-CPU at switch time; the two are not provably safe to touch
   from two CPUs.
2. **Every latency quantum is a 1 ms LAPIC tick.** `preempt()` is driven only by
   the timer (and reschedule IPIs). Nothing finer exists, so a 1 kHz game loop
   (1 ms/frame) is exactly at the scheduler's resolution — any jitter, any
   delayed wake, and a frame is dropped. This is why sustained pacing sits at
   5–11 fps regardless of how fast the driver is.

Neither is a GPU problem. Both are **execution-model** problems, and they cap the
frame rate long before the display hardware matters.

## Evidence (this session, all measured)

| fact | value | where |
|------|-------|-------|
| full present, kernel-local, no shell | **217–324 µs** | stage 7 probe |
| hardware-cursor move round trip | **~97 µs** | stage 3c |
| input→present via ring-3 shell | ~134 ms | stage 6 (shell-dominated) |
| sustained frame pacing | 5–11 fps, 150–950 ms gaps | stage 8 frame clock |
| `-smp 4` + a `Task` struct-field addition | **heap-entry crash** | stage 10 |
| same code at `bcd2749` (before the field) | **passes** | bisect |

## P1 root-cause — refined (after code-level tracing)

I traced every path that touches a task's saved context. The relevant
invariants and the one real gap:

- `Task.sp_slot` and `Task.fpu_area` are **separate heap boxes**
  (`Box::leak`), so `TASKS` reallocation does not invalidate the raw pointers
  `preempt()` carries — good.
- A task is only ever `Running` on one CPU: `plan_switch` sets
  `state=Running, owner_cpu=me` **under `SCHED_LOCK`**, and the Ready-scan only
  selects `State::Ready`. So a peer cannot steal a Running task, and
  `release_pending_prev` only re-arms a task on the CPU that owns it.
  Those invariants hold.
- **`preempt()` (scheduler.rs ~970-1009) is the exposure.** It computes
  `plan` (raw `sp_slot`/`fpu_area` pointers) *under* the lock, then the guard
  drops (releasing the lock and re-enabling interrupts), and only then runs
  `context_switch`. The *pick* is safe; the *switch* is not under the lock. The
  code's own comment (~line 984) claims "no other CPU can have claimed either
  context" — that holds for the incoming task, but the outgoing task's
  `sp_slot` is being written by this CPU while a peer can, in that window,
  observe and re-arm other tasks. The exact interleaving that corrupts memory
  in the field (it is intermittent — a field change, not a logic change,
  flips it) is a genuine TOCTOU between "plan computed under lock" and
  "context_switch executed after unlock", compounded by the per-task context
  being *split* between a heap `sp_slot` and a per-CPU `TSS.RSP0`/FPU area.

**Why this is architectural, not a typo:** the split ownership (a task's saved
RSP lives in one heap box while its kernel stack pointer is re-installed into
*each* CPU's TSS as it migrates) is only safe if migration and switch are
mutually exclusive. They are not — the switch is outside the lock. A correct
design makes a task's context **owned by one CPU at a time, immutable while
running**, so a switch is a single-owner operation with no peer able to touch
the same bytes. That is item (2) in the fix below, and it is the minimum
change that makes cross-CPU scheduling *safe by construction* rather than by
argument.

## The structural problems

### P1 — Cross-CPU task execution is not memory-safe (critical)
- `Task.sp_slot` is a heap box (`Box::leak`) holding the saved RSP;
  `Task.stack` and the FXSAVE area are re-pointed into the *current* CPU's
  TSS.RSP0 / syscall slot at every switch.
- `plan_switch` steals Ready tasks across CPUs freely (the M9.8-(d)/(e) affinity
  filter was removed). A task's context is therefore written on one CPU and
  consumed on another with no ownership discipline.
- Consequence: an inert `u8` field flips a clean boot into a jump-to-heap. This
  is memory corruption, not a logic bug, and it is the *reason* every affinity
  experiment (stages 9–11) crashed.
- This is the thing to fix first. Until a task's context is CPU-safe, no amount
  of pinning, RT priority, or present optimization is trustworthy.

### P2 — 1 ms is the only clock (critical for 1000 FPS)
- Preemption is exclusively `apic_timer_handler` → `preempt()` at 1000 Hz, plus
  reschedule IPIs (which I showed are unsafe to self-send).
- A 1000 FPS game has a 1 ms budget. The scheduler's own quantum is 1 ms, and
  `PRIO_RT` gives a *1-tick slice* that rotates among RT tasks. So the frame
  timer and the scheduler quantum are the same size — there is no headroom, and
  any delayed wake is a dropped frame.
- A real-time/preemptible design needs either a faster tick (or a
  tickless/deadline timer per RT class) or a genuinely event-driven "run this
  RT task now" path that is provably safe.

### P3 — Everything is one global lock + one global task table (high)
- `SCHED_LOCK` guards one `TASKS` Vec; `ksl` serializes the kernel service
  layer. The present flusher, console, input, and shell all contend for these.
- This is why the frame clock, shell, and flusher on one core collapse pacing,
  and why moving the load off the core helped. The design scales badly with
  core count because the global lock is the serializer.

### P4 — No preemption point that is safe *and* immediate (high)
- Self-IPI to force a same-CPU reschedule crashed (arbitrary interrupt state vs.
  scheduler lock). The scheduler has no "defer-safe-point reschedule" (set a
  flag, act at the next known-safe boundary). Every immediate-wake idea I tried
  hit this.

### P5 — Memory model / no protection between kernel subsystems (medium, latent)
- Kernel subsystems are separated by a few spinlocks and the KSL, but there is no
  enforced isolation; a stray write in one path corrupts another. P1 is a
  concrete instance. For a gaming OS that will run game code plus a display
  driver, this needs to be a deliberate boundary, not a convention.

### P6 — Driver is welded to the console (medium, product-level)
- The present path, damage rects, and the "renderer" are all the *text console*
  (framebuffer.rs `TextConsole`, the cursor sprite, the 3/4/7/8-stage probes
  drive it). There is no general drawing/blit surface API. A game cannot "draw"
  except by abusing `console_bytes`. This is a product gap, not a latency bug,

## What I'd do: one new approach that fixes P1–P4 together

The four critical/high problems share a root: **the execution model is
"global task table + global lock + 1 ms tick + unsafe cross-CPU context."**
Rather than patch them one at a time, replace that core with a small, correct,
real-time-friendly scheduler. Concretely, an **isolated, per-CPU, preemptible
kernel with owned contexts**:

1. **Per-CPU task tables + per-CPU run queues** (kill P3's global lock). Each
   CPU owns its tasks; migration is explicit and opt-in, never implicit steal. A
   task is *affine* by default and only migrates at a defined safe point.
2. **Owned, immutable-while-running contexts** (kill P1). A task's kernel stack,
   saved-RSP slot, and FPU area live in one allocation owned by the task, tagged
   with the CPU it currently runs on. A context switch touches exactly one CPU's
   data; there is no shared mutable pointer two CPUs can race on. This is the
   single most important change — it makes affinity *safe*, which retro-enables
   everything in P1/P4.
3. **A deadline timer, not a 1 ms poll** (kill P2). Replace "wake on the next
   1 ms tick" with a per-CPU min-heap of deadlines armed in the LAPIC/TSC timer
   for the *next* due task. A 1 kHz frame loop then wakes exactly on time
   (sub-microsecond scheduling error), not on a 1 ms grid.
4. **A safe immediate-reschedule primitive** (kill P4). `resched_local()` sets a
   per-CPU "reschedule at next safe point" flag; the flag is acted on at the
   scheduler's own safe boundaries (never from arbitrary IRQ state). Same-CPU wake
   then becomes prompt *and* provably safe — what stages 9–11 could not achieve.

Then, only after the execution model is correct and measurable:
5. **A real GPU surface/blit API** (P6) so a game has something to draw with,
   and the present path is driven by a compositor that owns frames (frame pacing,
   optional tearing) rather than the text console.

This is "one approach, not five": items 1–4 are a single coherent rewrite of the
execution core (per-CPU ownership + deadline timer + safe resched), and they
jointly unblock the two things the vision needs — a *reliable* 1 kHz frame loop
and *trustworthy* CPU isolation for the display path.

## What I am NOT claiming

- This is not a claim that 1000 FPS is achieved. It is a claim that the two
  things preventing it (unsafe cross-CPU contexts, 1 ms quantization) are
  architectural and have a known fix, and that the present path is already fast
  enough not to be the bottleneck.
- The crash on `master` (`9b21b7a`) was a real regression introduced by stage 10
  and is now **reverted** (commit `184997c`): the `pinned_cpu` field + frame-clock
  pin were removed, and the tree is green again (full suite + latency test pass).
  The P1 memory-corruption class it exposed is documented above and is the first
  thing to fix — not by re-adding the pin, but by making task contexts owned.

## When is the desktop?

Short answer: **the desktop is item (5), and it is blocked on (1)-(4) — but
not as far as it looks.** Here is the honest dependency chain and what a
minimal desktop actually needs.

**What a desktop is here.** Not a window manager bolted onto the text console.
It is a **compositor**: a kernel task that owns a framebuffer surface, draws
arbitrary shapes/text into it, and *presents* it on a frame clock. The good
news: we already have every hard part of that except the "draw arbitrary
shapes" and "own the frame clock" parts — the present path (damage rect,
transfer+flush, sub-ms) and the cursor are done and measured. A desktop is
mostly a **drawing API + a frame loop** on top of what already works.

**The dependency chain:**
1. **P1 fix (safe task contexts)** — blocking. A compositor/UI is exactly the
   kind of code that stresses task migration and CPU affinity (render thread,
   event thread, present thread). Shipping a UI on the current unsafe cross-CPU
   context path would reproduce the crash on demand.
2. **P2 fix (deadline timer)** — blocking for *smooth* UI. A compositor on a
   1 ms tick can hit ~60 fps at best and jitters badly; a 240 Hz desktop needs
   a real deadline timer. The present path being fast doesn't help if the
   compositor can't wake on time.
3. **P6 (surface/blit API)** — this is the actual "make the desktop" work:
   `surface_create/blit/fill/present`, a font blitter, and moving the console
   onto the same surface API so text and UI share one present path.

So the realistic order is **execution model (P1-P2) → surface API (P6) →
compositor/desktop**. That is not "the desktop is far away" — it is "the desktop
is one major subsystem (P6) away, but two foundations underneath it must be
solid first, and they are the same work a gaming OS needs anyway."

**A pragmatic, high-value milestone** (if you want pixels on screen sooner than
the full rewrite): a **minimal single-task compositor** — one kernel task that
owns a full-screen surface, draws a few rectangles and a blitted font each
frame, and presents via the existing damage-rect path, at a fixed cadence. It
needs P6 (the blit API) but can run pinned to one CPU, so it largely sidesteps
P1/P2 while still proving the whole "draw → present" loop. That gives a real
desktop *look* and a real frame loop early, and it becomes the testbed the
later scheduler work makes fast.

## Immediate next actions (in order)

1. **Bisect the P1 crash** on a debug build: instrument `sp_slot` / stack writes,
   add a canary to the per-task context, and get a deterministic repro. This is
   P1 and it is the root of everything.
2. Decide: fix P1 in place (per-task owned contexts) vs. the clean per-CPU
   rewrite. Given it is a memory-corruption class bug, per-task owned contexts is
   the minimum safe fix; the per-CPU rewrite is the destination.
3. Only then resume the present/frame-rate work — the current measurements
   (sub-ms present) will remain valid, and the deadline timer will let the frame
   clock actually run at its target.

  but it blocks the actual vision.
