# OnyxOS — Plan & Vision

> Living document. Everything runs only inside QEMU on this machine; no
> hardware boots. Scope/ordering updated to match the vision.

---

## Vision

**OnyxOS is a performance-first x86_64 OS, built for gaming and low latency.**

Two signature goals beyond "a working hobby OS":

1. **Split personality filesystem & binaries** — read/write both Linux files
   (ext2) and Windows files (FAT32, then read-only NTFS), and *ultimately run
   Windows `.exe` / `.dll` binaries*.
2. **Max performance, minimal latency** — low-latency interrupt handling,
   preemptive multitasking, a graphics/GPU path from day one. The full GPU
   driver is the closing milestone of the project.

---

## Core principles (why the order changed)

- **Syscalls/userspace come BEFORE the filesystem drivers.**
  The FS drivers (FAT32/ext2) exist to serve ring-3 programs via the VFS +
  syscalls. If we write them from ring 0 only, we end up re-testing and
  re-plumbing them behind syscalls later — wasted rework. Nail down the
  syscall boundary early (M4), then implement filesystems cleanly against it
  and test them through the ring-3 shell.
- **Everything QEMU-only.** No writing to any real disk/USB/firmware on this
  machine.
- **Dual-boot-friendly outputs, not dual-boot itself.** BIOS today; UEFI (GPT
  + OVMF) in M9 so the same kernel can boot both ways on real hardware.

---

## Milestones

| # | Milestone | Status / Note |
|---|-----------|----------------|
| M0 | Bootable kernel (BIOS): "Hello, OnyxOS!" VGA + serial | ✅ done |
| M1 | Interrupts: GDT/IDT, exception handlers, TSS/double-fault stack | ✅ done |
| M2 | Memory: frame allocator, page-table mapper, kernel heap (Vec/Box) | ✅ done |
| M3 | Preemptive multitasking + keyboard/mouse — timer IRQ, task switcher, low-latency scheduler groundwork | ✅ done |
| **M4** | **Syscalls + ring-3 userspace + ELF loader** — nail the kernel/app boundary *before* writing FS drivers | ✅ done |
| M5 | Block layer: ATA PIO, MBR/GPT partition parsing | ✅ done |
| M6 | VFS + **FAT32** read/write (Windows files) — tested from ring 3 | ✅ done |
| M7 | **ext2** read/write (Linux files) — tested from ring 3 | ✅ done |
| **M7.5** | **Path-aware VFS (subdirectories)**: `/`-separated paths in every FS driver, `SYS_MKDIR`, subdir listings on both mounts — FAT32 dir cluster chains with `.`/`..`, ext2 dir inodes with `.`/`..` (`test-m8.ps1`) | ✅ done |
| **M8** | **ELF loader from disk + ring-3 shell**: `SYS_SPAWN` loads ELFs from the VFS (region-checked against loaded programs in the shared address space), `SYS_READ(0)` keyboard line discipline, `/SHELL.ELF` + `AUTOEXEC.TXT` seeded into the FAT32 image, shell with `ls/mkdir/cat/write/run/echo/help/exit` — fstest now launched by the shell's autoexec via `SYS_SPAWN` (`test-shell.ps1`) | ✅ done |
| M9 | **UEFI boot** (GPT image + OVMF) + graphical framebuffer console: `uefi.img` with GPT + ESP + hybrid protective-MBR data partitions, OVMF-verified incl. GOP framebuffer + full M8 shell flow (`test-uefi.ps1`) | ✅ done |
| **M9.5** | **Graphical desktop userspace**: per-process address spaces (CR3), window-server graphics syscalls, IPC + input delivery to ring 3, wallpaper + windows | **DEFERRED** (revisited later) |
| **M9.6** | **Core hardening + missing subsystems** — A: upgrades (TSC ns timekeeping ✅, APIC/IOAPIC + LAPIC timer ✅, scheduler v2 ✅, FPU/SIMD save-restore ✅, block cache ✅, frame alloc v2 ✅) · B: missing subsystems (PCI ✅, ACPI ✅, process lifecycle ✅, raw input ring ✅, `perf` instrumentation ✅) · C: ABI/file-API foundation (argv/envp/auxv ✅, user-pointer validation ✅, errno ✅, mount table) | **done** — A1–A6, B1–B5, C1–C3 all complete; M9.6 regressions pass on BIOS + UEFI (test-fs, test-sched, test-proc, test-block, test-memory, test-pci, test-acpi, test-raw, test-input, test-fpu, test-time, test-args). |
| **M9.7** | **Linux ABI compat — run static Linux ELFs**: syscall-number shim, argv/envp/auxv, `arch_prctl` TLS, mmap/brk, PIE/relocations | ✅ done |
| **M9.8** | **SMP — multi-core** (its own stage, per decision): MADT-driven AP startup, per-CPU data, per-CPU run queues + IPIs | ✅ done — GS-base per-CPU blocks, INIT-SIPI-SIPI AP bring-up through a hand-assembled low-page trampoline, per-CPU GDT/TSS + IDT + LAPIC timers, reschedule IPI, per-CPU RSP/syscall slots, boot context restored as a task, kernel-service lock (`ksl`) + input/keyboard/mouse locking, **task migration with work stealing**, stall diagnostic re-based on provable starvation, and the `xsave64`/`xrstor64` EDX:EAX mask bug fixed (AVX/YMM now survives switches under migration); `test-smp.ps1` passes at `-smp 1/2/4`, `test-avx.ps1` at `-smp 4 -cpu max` (200+ rounds, zero failures), all 25 suites green |
| **M10** | **GPU driver system — staged, from basic to decent**: M10a PCI GPU scan + modesetting (kernel-controlled framebuffer, replace the bootloader-fixed one); M10b render-surface API (`surface_create/blit/present`) + compositor stub + 2D blits; M10c real acceleration path toward a decent driver (hardware blit/fill where QEMU exposes it, dirty-rect present, vsync-ish pacing) | M10a ✅ **done** (dispi modeset driver, PCI BAR sizing, canary-verified mapping, graceful fallback — `test-gpu.ps1` ×2 green, all suites green); M10b stage 1 ✅ **done** (virtio-gpu transport probe: four capability regions, VERSION_1 negotiated, 2 queues/1 scanout, `virgl=1 ctx=1` on virtio-vga-gl — `test-gpu.ps1` 4 boots ×2 green, all suites green); M10b stage 2 ✅ **done** (control virtqueue engine + GEM-lite resource: queue → DRIVER_OK → CREATE_2D/ATTACH_BACKING → canary → SET_SCANOUT/TRANSFER+FLUSH → console adopt + 100 ms flusher on `-vga virtio`; graceful no-transport fallback on std-VGA, documented virgl 2D-skip on virtio-vga-gl — `test-gpu.ps1` 4 boots ×2 green, all suites green, QEMU `guest_errors` empty); M10b stage 3a ✅ **done** (damage-rect present: dirty bbox tracked in `framebuffer.rs`, only what the console drew is transferred; measured ~90 % DMA saved, idle console costs zero — all suites green); M10b stage 3b ✅ **done** (input→present latency probe: mouse IRQ stamps the input, the flusher times device-ack, power-of-two histogram + n/avg/min/max report, under test in `test-latency.ps1`; measured ~64 ms software-cursor baseline — the number every later optimization is judged against); M10b stage 3c ✅ **done** (hardware cursor on queue 1: sprite uploaded once as a B8G8R8A8 2D resource, every move a 56-byte MOVE_CURSOR with zero framebuffer damage; measured input→MOVE_CURSOR avg=130 µs vs ~57 ms software — a ~440× pointer-latency win; the flusher is scheduled first so the optional cursor can never block present); M10b stage 4 ✅ **done, caveat** (adaptive present cadence: 1 ms latency quantum while active, 16 ms idle backoff, damage-generation change signal; new `response: damage->present` and `pacing: interval` metrics — but the boot-burst test workload can't cleanly A/B the cadence, so no measured win is claimed, only the removal of the structural 100 ms wait); M10b stage 5 ✅ **done — a finding, not a win** (on-demand single-glyph workload + response/wake attribution: ~70% of framebuffer-present latency is SCHEDULER wake latency ~170ms of ~239ms, NOT the pacing quantum — the 1ms quantum/16ms idle are working; the next optimization should attack task wake-up (event-driven present / scheduler priority), not pacing. No latency win claimed from stage 4). M10b stage 6 ✅ **done, partial win** (event-driven present: damage path wakes the flusher on the empty→non-empty transition via a new scheduler primitive that also sends the reschedule IPI; flusher promoted to RT. Measured single-glyph present latency 239 → 134 ms, a real ~44% win; residual ~105 ms is upstream ring-3-shell scheduling, not the present path). M10b stage 7 ✅ **done** (kernel-local present probe: a task writes one 4x4 marker rect straight into the framebuffer, bypassing the ring-3 console path, so the only thing between draw and device is the present path. Measured **~340-430 us (sub-millisecond)** draw->present. This settles stages 4-6: the virtio present path is NOT the input latency bottleneck - the ~130 ms the sendkey workload shows is ring-3 shell task scheduling upstream of the draw. A kernel/RT-priority draw path sees the sub-ms number). M10b stage 8 ✅ **done — a finding** (kernel frame clock: an animated strip redrawn per tick + achieved-fps/jitter reporting. Pure present stays sub-ms (~217-324 us) under load, but sustained frame pacing is only ~5-7 fps with 150-950ms intervals on this 2-cpu TCG VM — the present PATH is fast, cadence is limited by scheduling/CPU availability (damage coalesces between presents). Redirects the next step to scheduling/CPU-affinity for the present task, not more driver work). M10b stage 9 ✅ **done — two unsafe fixes rejected** (scheduler/SMP investigation: a same-CPU self-reschedule-IPI and pinning the frame clock to CPU 1 both lifted or aimed at pacing but each crashed the kernel with a null-context jump, so both are backed out and documented. Finding: the present path is sub-ms (217-324 us, well under a 1 ms budget), so the driver is NOT the ceiling; sustained pacing on this 2-cpu TCG VM is limited by CPU contention + the removed CPU-affinity filter. The correct fix — opt-in per-task CPU affinity so the present flusher owns a core — is a **scheduler** change, scoped to the scheduler track as stage 10, not the GPU track). M10b/c direction: **virtio-gpu is the primary backend** (DMA resources, command virtqueues), the dispi driver stays as the legacy fallback backend; present path proven sub-ms, so a game's frame rate is gated by CPU scheduling/affinity and the emulated environment, not the driver — M10b P1 step B ✅ **done** (task-context memory safety: restored the CPU-affinity filter in `plan_switch` and stopped rewriting `owner_cpu` on switch, so a task's whole context — heap `sp_slot` + kernel stack/`TSS.RSP0` + FPU area — is only ever touched by ONE CPU, closing the corruption class stage 10's inert struct-field change exposed. Consequence: migration is gone, so bench workers and fpu-test B are spawned explicitly per-CPU; test-para went from x2.65 to **x4.6–4.8 speedup on 4 distinct CPUs** (faster — no migration overhead). Rejected: holding SCHED_LOCK across context_switch (unsound, deadlocks the incoming task; backed out). Caveat: safety floor, not final design — the destination is the per-CPU owned-context rewrite, which restores safe work-stealing); M10b repo hygiene ✅ **done** (untracked ~7MB of build artifacts: 3× 2.4MB LLVM IR + logs; `.gitignore` now covers `*.ll/*.err/*.out/*.pid/*.txt`). Next: the FS panic audit (Priority 3), then the per-CPU owned-context rewrite. |
| M11 | NTFS read-only + multi-drive mounting | extra Windows compat |
| **M12** | **PE foundation**: parse `.exe` / `.dll` (PE/COFF), relocations, DLL imports groundwork | solid foundation only |
| M13 | GUI: window manager + compositor + built-in apps (terminal, file manager) | apps on the M9.5 base |
| **M14** | **VISION: run Windows `.exe` / `.dll` binaries** (via M12 + syscall emulation layer) | vision capstone #1 |
| **M15** | **Full GPU driver** (accel via VM-based GPU, Vulkan-lite API) | vision capstone #2 — the last |

---

## P16: architecture audit + modernisation roadmap (2026-10-01)

> ### ⚠️ NOT SHIPPED — read this first
>
> Everything in this section was **attempted or proposed and did NOT land in the
> kernel**. It is recorded here because an audit that only lists successes is
> misleading. Nothing below is a claim about the running system.
>
> **Written but reverted (the code exists only as a comment explaining why not):**
>
> | # | Change | Why it did not land |
> |---|---|---|
> | 0.4 | `surface::blit` same-format → `copy_nonoverlapping` fast path | **Failed the existing ramp self-test**: `31/32 blitted pixels wrong`, every destination reading back as `0` while the returned rect was correct. `copy_nonoverlapping` asserts non-overlap and nothing in `Surface` enforces it; and the loop's accesses are `volatile` (mapped windows, not Rust-owned memory), so non-volatile bulk moves are an unanswerable ordering question here. The per-pixel formatted loop still ships, unchanged in behaviour. |
> | 0.6 | `static mut TABLE` → `SyncUnsafeCell` in `surface.rs` | **Blocked on the toolchain.** On this nightly `SyncUnsafeCell::get_mut` takes `&mut self`, so it cannot back a `static` without a wrapper — and `LOCK` is already held by every caller, so a second lock nests or deadlocks. Two real options remain: wait for `get_mut(&self)` to stabilise, or fold `LOCK` and the table into one `spin::Mutex<[Option<Surface>; MAX_SURFACES]>` (the honest end state, too large for Phase 0). |
>
> **Also NOT done — identified in the audit, never started:**
>
> * **No networking at all** (finding B): no NIC, no stack, no `SYS_SOCKET`. `virtio.rs` probes one device class, GPU. Phase 4.
> * **No process isolation** (finding A): every ring-3 program shares one address space. Phase 3.1.
> * **No `mmap`/`munmap`/`mprotect`/`madvise`**, no shared memory (finding F). Phase 3.2/3.3.
> * **No `fsync`/`fdatasync`** (finding G): acknowledged writes are not durable, and with an unjournaled ext2 that is a corruption path. Phase 1.4.
> * **No huge pages** (finding L): `Size2MiB` is unused; 4-level paging only. Phase 2.1.
> * **Scheduler migration still disabled** (finding M): `Task::owner_cpu` is still the single CPU authority. Phase 3.4.
> * **KPTI never implemented** (finding C, partial): SMEP/SMAP shipped in 0.1, but Meltdown's page-table leak is unmitigated. No plan slot — it needs per-process address spaces (3.1) before it means anything.
> * **The STAC window around `dispatch` is coarse.** It covers the whole dispatcher rather than per-`copy_to_user`, so SMAP is disabled for more of the syscall path than necessary. Correct today, weaker than it should be.
>
> **Unmeasured claims:** the 0.3 `fill_row` speedup is *reasoned*, not
> benchmarked. No number in this document should be quoted as a measurement
> unless it says "measured".

---

### Findings, by severity
A full audit of the 16,110-line kernel against the stated vision (*modern
gaming OS, modern standards, daily use*) found **no craftsmanship problems and
several architectural ones**. The code quality is genuinely good — comments
explain *why*, failures are documented rather than hidden, self-tests read real
pixels. The issue is **layering**: the kernel was built bottom-up (boot →
interrupts → paging → filesystems → graphics), which is right for learning and
wrong for a product. The result is ~4,700 lines of graphics work sitting on top
of a layer that does not exist yet.

### Findings, by severity

**Blocking the vision**

| # | Finding | Evidence |
|---|---|---|
| A | **No process memory isolation.** All ring-3 programs share one address space | `userspace.rs:26` — *"Per-process CR3 switching retires this in a later milestone"* |
| B | **No networking whatsoever** — no NIC, no stack, no `SYS_SOCKET` | `socket/tcp/udp/ethernet/virtio-net` = 0 hits; `virtio.rs` probes GPU only |
| C | **No SMEP / SMAP / KPTI** — ring-3 may execute kernel pages | `SMEP/SMAP/KPTI/clac` = 0 hits |
| D | **ext2 is pre-1.0 and unjournal** — a power cut corrupts the volume | `ext2.rs:1-16` (rev 1, 1 KiB blocks, single group, no journal, no extents) |

**Modern-standards gaps**

| # | Finding | Evidence |
|---|---|---|
| E | Block I/O is **ATA PIO, LBA28 only** (128 GiB ceiling); no NVMe/AHCI/DMA | `ata.rs:1-20` |
| F | **No `mmap`/`munmap`/`mprotect`/`madvise`**, no shared memory | 22 syscalls, none of these |
| G | **No `fsync`/`fdatasync`** — acknowledged writes are not durable | syscall table |
| H | **FAT32 cannot write long filenames** — breaks Windows round-trip | `fat.rs:7` |
| I | **Graphics: `dispi` is live, virtio-gpu is not.** dispi maps the LFB directly, which is why full-screen repaints are visible top-to-bottom | serial log: `[gpu] mapping: 8100 KiB at 0x040000000000 -> 0xfd000000` |
| J | **surface damage and present damage are separate accumulators** — a composited frame never reaches scanout | `Surface::take_damage` vs `framebuffer::take_dirty_rect` |
| K | **`blit` is per-pixel formatted, not a memcpy** | `surface.rs:365-382` |
| L | No huge pages (`Size2MiB` = 0 hits); 4-level paging only | — |
| M | Scheduler **migration disabled**; `Task::owner_cpu` still authoritative | `scheduler.rs:88` |

### Why this order

The phases below are ordered **fastest to implement first, hardest last** —
the opposite of severity order. That is deliberate: nothing in Phases 0–1 is
throwaway, and the cheap work de-risks everything after it. The honest cost is
that **after Phase 2 the OS is still not safe to ship to a daily user**; Phases
3–4 are what buy "safe to ship", not "capable of more development".

#### Phase 0 — Free wins (~1 week, no dependencies)

- **0.1 SMEP + SMAP** — ✅ **done** (see below)
- **0.2 unsound `&'static mut` page-table view** — ✅ **done**
- **0.3 `fill_row` per-pixel → bulk write** — ✅ **done**
- **0.4 `blit` same-format fast path** — ❌ **reverted** (see below)
- **0.5 duplicate `SYS_INPUT_READ` doc** — ✅ **done**
- **0.6 `static mut` → `SyncUnsafeCell`** — ❌ **blocked** (see below)

##### Phase 0.1 result — SMEP + SMAP (finding C)

New `kernel/src/sec.rs`; detection via `CPUID.07H:EBX[7|20]`, enable via
`Cr4::read()` + `Cr4::write()` (preserves the bootloader's PE/PAE), then **CR4 is
read back** so "enabled" is verified rather than assumed. Three `[sec]` lines in
the boot log; `test-sec.ps1` asserts them under `-cpu max`.

**It was not 20 lines, and that is the interesting part.** SMAP immediately
faulted the boot — proving the feature was needed by finding four kernel writes
to USER_ACCESSIBLE pages that had never been declared anywhere:

| Site | What it writes |
|---|---|
| `userspace::init` | the `EB FE` ring-3 exit trampoline |
| `load_elf` | ELF PT_LOAD segments + the zeroed BSS tail |
| `build_initial_stack` | argv/envp/auxv strings and the process-start block |
| `apply_relative_relocations` | R_X86_64_RELATIVE/GLOB_DAT/JUMP_SLOT fixups |

Each now runs inside an explicit `sec::with_stac` window (`STAC`/`CLAC`, which
toggle *only* RFLAGS.AC — a `pushfq`/`popfq` pair would round-trip every other
flag and could resurrect bits the kernel cleared). The syscall entry stub was
split in two: `syscall_entry` and `syscall_entry_smap` (`STAC` … `CLAC` around
`dispatch`), because `stac` raises `#UD` on a CPU without SMAP, so it must not
merely be skipped — it must not be *present*.

**Verified** (`test-sec.ps1`, `-cpu max -smp 2`): `SMEP cpu=yes cr4=on`,
`SMAP cpu=yes cr4=on`, `entry=smap/stac`, `fstest: PASSED`, no `#PF`/`#DF`/panic.

**Two known gaps, deliberately not hidden:**

1. The STAC window around `dispatch` is **coarse** — it covers the whole
   dispatcher, not per-`copy_to_user`. Narrowing it is planned work.
2. QEMU's default `qemu64` does **not** advertise SMEP/SMAP, so on the default
   test invocation the bits are detected as absent and nothing is enabled. That
   is correct behaviour, not a silent failure — which is exactly why
   `test-sec.ps1` runs `-cpu max`.

##### 0.2 — page-table aliasing (soundness)

`active_level_4_table` returned `&'static mut PageTable`. The level-4 table is
ONE allocation that `OffsetPageTable` already holds a `&mut` to for the kernel's
whole life, so `page_flags` minting a second `&'static mut` to the same bytes was
two mutable references to one place — UB even though both callers are careful.

Now `active_level_4_ptr` / `table_ptr` hand out **raw pointers**, and each caller
takes the weakest reference its access needs: `&mut` only for the one-time
`OffsetPageTable::new`, and a **shared `&`** for the read-only walk in
`page_flags`. (The walk needed explicit `let p: &PageTable = &*p;` bindings —
`&(*p)[i]` is an *implicit autoref*, which is a hard error precisely because it
can quietly create a borrow that outlives what a raw pointer guarantees.)

##### 0.3 — `fill_row` bulk write

Was 1920 calls to `set_pixel` per row — each redoing four bounds checks and a
`PixelFormat` match for a colour that is constant across the row. The sign-in
backdrop is 1920×1920 = 3.7 M of those. Now the row base and channel bytes are
hoisted and pixels are written directly through a `chunks_exact_mut` loop; the
guard is computed once. The format handling is intentionally duplicated from
`set_pixel` and the comment marks it as the thing that must not drift.

*(Not benchmarked — this is a reasoned change, not a measured one. It should be
measured on the host before anyone quotes a speedup.)*

##### 0.4 REVERTED — the `blit` fast path does not work

The obvious optimisation: when `s.format == d.format` the two `Format` matches
in the per-pixel loop are no-ops and the byte pattern is identical, so the copy
should collapse to `copy_nonoverlapping`. **It was written and it failed the
existing ramp self-test: `31/32 blitted pixels wrong`, every destination reading
back as 0** while the returned rect was correct.

Two reasons it is unsafe as written, which is why this is a revert and not a
fix-in-place:

1. `copy_nonoverlapping` **asserts** the ranges do not overlap. Two `Surface`s
   are separate allocations today, but nothing in the types enforces it — and an
   overlapping "blit" is exactly the UB that yields silent zeroes.
2. The loop's reads/writes are `volatile` (the surfaces are mapped windows, not
   Rust-owned memory), and swapping them for non-volatile bulk moves is an
   ordering question this file has no answer for.

The per-pixel formatted loop stays, with a comment saying so. A correct bulk
path needs the backing-store/overlap invariant stated first — which is the same
surface→scanout bridge as Phase 2.2.

##### 0.6 BLOCKED — `SyncUnsafeCell` on this toolchain

`static mut TABLE` in `surface.rs` is correctly guarded (every access is under
`LOCK`) but nothing in the type system says so, and `static mut` hands out
`&mut` to the whole array even for read-only access. The correct replacement
(`SyncUnsafeCell`) was attempted and abandoned: on this nightly
`SyncUnsafeCell::get_mut` takes `&mut self`, so it cannot back a `static`
without a wrapper — and `LOCK` is already held by every caller, so a second lock
would nest or deadlock. Either wait for `get_mut(&self)` to stabilise, or fold
`LOCK` and the table into one `spin::Mutex<[Option<Surface>; MAX_SURFACES]>`.
The reason is recorded in the source so it is not re-attempted blindly.

**Phase 0 gate — met.** `test-sec` PASSED · `test-signin` 17/17 ·
`test-surface` PASSED · `test-input` PASSED.

*Boot-to-signin-verified is ~20 s on a quiesced host* (measured 2026-10-01).
Every earlier "timeout" was a leftover QEMU from a previous run eating the CPU:
every `test-*.ps1` starts with `Stop-Process qemu-system-x86_64`, so two scripts
at once kill each other's VM and produce failures that look exactly like
regressions. Run them sequentially, and check nothing is still running first.

#### Phase 1 — Cheap correctness (~2–3 weeks)

- **1.1 ATA LBA48 — `u64` LBA end-to-end** — ✅ **done**
- **1.2 FAT32 LFN write** — ✅ **done**
- 1.2.1 `rename` that rewrites the LFN run — ✅ **done** (see the result below; it
  also uncovered a data-loss bug in `create_entry`)
- 1.3 File timestamps in `FileStat` — ✅ **done** (see the result below)
- **1.5 Independent FAT32 interop verification** — ✅ **done, and it found two
  shipped-breaking bugs** (see the result below). This was inserted ahead of 1.4
  precisely because it could invalidate work already marked complete.
- 1.4 **`fsync`/`fdatasync` + cache coherency** — the durability hole — ✅ **done**
  (see the result below)

> Everything in this roadmap that is marked done but not finished — deliberate
> deferrals, known-bad upstream tooling, and the "reported but not enforced"
> findings — is tracked separately in **`docs/OPEN_ISSUES.md`**. That file is the
> handoff list; this one is the plan.

##### Phase 1.4 result — `fsync`, and why the test had to run outside the guest

The obvious test — call `fsync`, check it returned 0 — proves nothing. A kernel
that implemented `fsync` as `return 0` would pass it, because the only thing
asked is whether the kernel remembers what it already knew. So
`tests/test-fsync.ps1` verifies the claim **from outside the guest**: boot
**without `-snapshot`**, have `fstest` write `/Fsync Durability Probe.txt` and
sync it, kill QEMU (an abrupt end is the closest thing here to a power cut),
then read the raw image back with the independent PowerShell FAT32 reader and
require the exact name, size and date to be present. The checker never ran
inside the kernel it is checking.

The stack is `SYS_FSYNC`(22) / `SYS_FDATASYNC`(23) → `sys_fsync` →
`vfs::fsync` → `Fs::fsync`. Two decisions worth keeping:

- **The fd is resolved back to its path**, because the fd table stores a name
  and a position, not a filesystem — the path is the only thing that selects a
  backend. Bad fd → `-EBADF`, unresolvable path → `-ENOENT`.
- **`fdatasync` deliberately shares `fsync`'s body.** There is no separate
  metadata cache to skip yet; the distinction only earns its keep once a
  filesystem does journal replay. One implementation, not two.

The trait default is `Ok(())`, with the limit stated in the code rather than
implied: this makes a write durable *to the device*, it does **not** make an
unjournaled ext2 crash-consistent. A power cut mid-metadata-update still
corrupts the volume; only 2.4 (ext4) fixes that.

**Two bugs, both in my own code, both caught by taking the test seriously:**

1. **`fstest` called a function that did not exist.** The new block used
   `sayln(...)`, but that binary only defines `say` and every caller embeds the
   newline itself. `cargo build` failed. Small, but it is the reason nothing was
   committed before the build was green: a broken tree is not a working version.
2. **The durability test hardcoded the wrong payload size** — it asserted
   `size=40` while the payload is 42 bytes (`fsync durability payload ` = 25,
   `0123456789abcdef` = 16, `\n` = 1). The kernel had written the **correct**
   size, so the suite failed against correct code. The size is now derived from
   the payload literal instead of repeated by hand. The generalisable version:
   a number duplicated in two places is a bug waiting to happen, and a failing
   test is not automatically evidence the code is wrong — it can equally be
   evidence the checker is.

**Result:** `test-fsync` passes, and the host-side listing shows
`lfnSlots=2 size=42` — the long name and the exact byte count survived an
abrupt guest termination, confirmed by a reader that is not our kernel.

**Limit carried forward:** this proves durability across a QEMU process kill,
which flushes nothing special — the image file is the device. It does **not**
prove anything about a real power cut on real hardware, and it does not make
the metadata path crash-consistent on ext2. Both are recorded in
`docs/OPEN_ISSUES.md`.

##### Phase 1.5 result — the checker that was not the kernel, and what it caught

1.2 and 1.2.1 were both "done" with green tests, and 1.2 shipped a real bug that
no test in the project could see. `tests/fat32-verify.ps1` is a second
implementation of the FAT32 on-disk format, in PowerShell, written from the
specification, sharing no code with the kernel. `tests/test-fatinterop.ps1` boots
the guest **without `-snapshot`** so its writes survive, then hands the image to
that reader.

**RETRACTED, kept so the mistake is visible:** this section originally claimed
two shipped bugs, the first being "the LFN checksum was not the checksum".
**That claim was false** â€” the checksum is a rotate and always was â€” and the
fix it prompted would have broken every long filename. See the corrected list
below, and `docs/OPEN_ISSUES.md` A2.

**One real bug found, one false alarm — and the false one is the more useful.**

1. **LFN chunks were stored in reverse (REAL, fixed).** `lfn_slots` put the *last*
   13 characters in the highest-numbered slot and `lfn_name` sorted ascending to
   match. The spec has sequence *N* — physically furthest from the short entry —
   holding characters `[(N-1)*13 .. N*13)`, i.e. the *first* 13, confirmed against
   a real on-disk dump in the FAT documentation. Again perfectly self-consistent,
   and again invisible: any name over 13 characters would have appeared in Windows
   **transposed**. The verifier surfaced it by decoding
   `InteropTwentyCharsLong` as `CharsLong`.
2. **The checksum was reported as broken, and was not (reverted).** The first
   version of the verifier computed the LFN checksum as a plain sum of the 11
   short-name bytes, "found" the kernel's value mismatched, and the kernel was
   changed to match it. **That change was wrong.** The FAT checksum is the original
   MS-DOS `ChkSum` — *"NOTE: The operation is an unsigned char rotate right"* —
   and both the Linux kernel and the `fatfs` crate implement the rotate. The
   kernel has been reverted to it and the verifier corrected. Had this shipped,
   every long filename would have been unreadable on real hardware.

**A third oracle settled it.** `tools/fatfs-verify` reads an image with the
third-party `fatfs` crate — which `mkfat` also uses to build our volumes. It
agreed with neither variant until the rotate was restored. It carries a
`--selftest` mode that makes the crate write and re-read its own multi-slot names,
so the oracle is proven valid before its verdict is trusted.

**Result: 13 checks passed** on the PowerShell verifier, which enforces sequence
order, `LAST_LONG` placement, checksum and attribute **on kernel-written entries
only**.

**Unresolved, and deliberately left open:** the `fatfs` oracle reads our
single-slot long names but returns only the 8.3 alias for multi-slot ones, even
though it reads back its own. Checksum and chunk order are both confirmed correct,
so something else in the slots still differs. Tracked as **A5** in
`docs/OPEN_ISSUES.md`; the oracle's verdict is reported loudly rather than
enforced, because enforcing a checker we know disagrees with us is worse than
saying so.

**Known limits of 1.5 — all recorded in `docs/OPEN_ISSUES.md`, not fixed here:**

- **Windows itself still has not read these images.** `Mount-DiskImage` refuses
  the raw image on this host. "Interop verified" currently means *a second
  implementation agrees*, which is much better than it was and is **not** the
  same claim.
- **`mkfat` (the `fatfs` crate) writes non-conformant LFN entries** — every
  seeded file has a bad checksum and attribute `0x00`. Reported, not enforced;
  it is upstream, and our reader already tolerates it.
- **ext2 has no equivalent verifier.** The bug class fixed twice here is still
  unproven absent in `ext2.rs`.
- **A duplicate directory entry was observed** (`InteropVeryLong…` twice, both
  aliased `INTERO~2`). Reported, not enforced: it is not a format error and needs
  a separate debugging pass.
- The verifier checks the **root directory only**, and validates *structure* —
  it does not follow data chains to confirm file contents.

##### Phase 1.1 result — LBA48, and the estimate was wrong

The plan said "~0.5 day, 50 lines". It was not: LBA was `u32` in **five** modules,
so lifting the ceiling meant widening all of them — `ata` (the driver), `blkcache`
(the **cache key**, plus `last_seq_end`, which would otherwise wrap and make
sequential detection silently wrong above 128 GiB), `block`, `ext2`
(`part_start`), and `fat` (`part_start`, `cluster_lba`, FAT-sector maths). A
half-measure that only touched `ata.rs` would have added the capability while the
ceiling stayed in place everywhere else — i.e. it would have looked done and
changed nothing.

Two bugs found on the way, both by the existing suites rather than by me:

1. **The EXT path broke every filesystem.** Taking the 48-bit route whenever the
   drive advertised it sent `SET HIGH LBA` + `CMD_READ_EXT` even for LBA 0 on the
   40 MiB test image, and the whole stack stopped finding its partition table
   (`M5: LBA0 has no 0x55AA signature`). Fixed by taking the 48-bit path **only
   when the address actually exceeds 28 bits** — the EXT sequence is a second,
   less-exercised path that added risk for zero benefit under 128 GiB, and this
   way a bug in it can only affect disks that actually need it.
2. **The register layout was wrong anyway.** The high-half writes used
   `(lba>>32)/(lba>>40)/(lba>>44)` and put `LBA[24:28]` in the device/head
   nibble. ATA-6 is `LBA_LO←[39:32]`, `LBA_MID←[47:40]`,
   `LBA_HI←[51:48]`, device/head bits 3:0 ← `LBA[44:40]`. Corrected, with the
   reason left in the source.

Also removed a now-wrong clamp: `block.rs` pinned the write-verify to
`LBA28_MAX`, which existed only because the driver could not address higher.

**Verified:** `test-fs` PASSED (stat/seek/rename/unlink all OK) ·
`test-block` PASSED (cache hits 172 / misses 4, read-ahead +672× cold/hot) ·
`test-fs-corrupt` PASSED · `test-surface` PASSED · `test-sec` PASSED.

**Still not done — this is address width, not transfer mechanism.** The drive is
still **PIO**: every byte goes through CPU port I/O. LBA48 means "a 4 TB disk is
addressable", not "a 4 TB disk is fast". Replacing PIO is Phase 3.5 (virtio-blk),
and until then large-disk performance is worse than useless.

##### Phase 1.3 result — timestamps, and a rule about what `0` means

**The change.** `rtc::DateTime` gained `to_unix()` and a free `from_unix()`
(Hinnant's `days_from_civil` / `civil_from_days`, exact integer arithmetic over
the whole proleptic calendar rather than only FAT's 1980–2107 window). Both
drivers now stamp and read real times:

| | FAT32 | ext2 |
|---|---|---|
| create | `ctime`+`mtime`+access-date | `i_ctime`/`i_mtime`/`i_atime` |
| write | write-time, write-date, access-date | `i_mtime`+`i_ctime`+`i_atime` |
| rename | **carried over verbatim** | n/a (rename is FAT-only today) |

FAT byte layout, which is the part that invites mistakes: create time 14..16,
create date 16..18, access date 18..20, **write time 22..24, write date 24..26** —
with the low cluster at 26..28 sitting *between* them. The fields are not
contiguous, which is exactly the shape of bug that writes a cluster number into a
date and still round-trips through our own reader.

**Three decisions worth stating, because they are the ones a later reader will
otherwise "fix" wrongly:**

1. **`0` means "no timestamp recorded", never "1970-01-01".** `to_unix()`
   returns `Option`, and a zeroed RTC yields `None` rather than a number, so the
   failure mode is an honest blank instead of a plausible lie. Every test
   therefore asserts a *range* (2020-09-13 … 2096) rather than "non-zero" — a
   range chosen so a missing RTC, a 1970 default and FAT's 2107 ceiling all fail
   it.
2. **A rename is not an edit.** Rename copies the times across untouched. It is
   tempting to stamp "now" because rename writes a brand-new directory slot, and
   the self-test would still pass if mtime only ever moves forward — so the test
   asserts `mtime_after_rename == mtime_after_write` exactly.
3. **The `stat` ABI grew from 16 to 40 bytes** (size, atime, mtime, ctime,
   flags). It had to: the old layout spent 7 bytes on padding with `is_dir` as a
   single byte at +8. `is_dir` is now flags bit 0 at +32. There were no external
   consumers, and this is far cheaper to pad out now than to remove later.

FAT's 2-second time granularity is a format limit, not a rounding bug, so the
self-test claims `mtime_after_write >= mtime_before` rather than a strict
increase — an advance inside the same tick is genuinely invisible.

**Verification.** Boot self-test now **9 passed, 0 bad**; `test-fs` PASSED
(including a new `fstest: timestamps OK` gate that checks the range from ring 3);
`test-block`, `test-fs-corrupt`, `test-m7` (ext2), `test-shell`, `test-lnx`,
`test-sec`, `test-signin`, `test-surface`, `test-input`, `test-time`, `test-rtc`
and `test-proc` all PASS.

**Known limits of 1.3:**

- **Still never validated against an outside reader.** Everything here is our
  encoder meeting our decoder. A host-side `dir /T` on a written image is the
  check that is still missing, and 1.3 is precisely the phase where it becomes
  checkable — the FAT date/time words are now the only thing standing between us
  and a real interop verdict.
- **FAT cannot represent odd seconds** (2-second granularity) and has no
  separate ctime, so creation doubles as ctime and rename cannot record that it
  happened.
- **Directory timestamps are only stamped at creation.** A file added to or
  removed from a directory does not bump the directory's mtime, so a recursive
  "what changed" scan will miss it. Real `ls -lt` on both filesystems needs this.
- **ext2 atime only moves on write, not on read.** Updating it per read would
  mean a directory-entry write on every read; the usual compromise is a once-a-
  day atime, and neither is implemented.
- **`utimensat`/`futimens` do not exist**, so a user cannot set a time, only read
  one. Everything above is kernel-driven.
- Still no durability (1.4) and no Windows-verified interop.

##### Phase 1.2.1 result — rename keeps the long name, and a silent data-loss bug

Closing a one-line inconsistency (`rename` degraded any long target to 8.3) turned
into the most valuable bug find of the phase, because rename is the only operation
that **allocates a new directory slot adjacent to existing ones**.

**What changed.** `create_entry` and `rename` now share three helpers —
`derive_name` (short alias + LFN slots, with the collision probe able to *exclude
the entry's own alias*), `alloc_run` (reserve N contiguous free slots), and
`write_run`. Split out of the old `delete_entry`, `remove_dir_slots` frees the
directory slots **without** touching the data chain — rename has to move the slot
and keep the clusters, and calling `delete_entry` after writing the new slot would
have freed clusters the file is still using.

**Three bugs found, the last one pre-existing and serious:**

1. **`rename(a, a)` returned `EEXIST`.** The existence check rejected the target
   because the target *is* the source. POSIX says a no-op success. Any caller that
   renames-before-writing churned its own alias on every call. Now compared by
   *slot identity* (`dir_cluster` + `dir_index`), not path string, so a
   case-differing path still counts as the same entry.
2. **An idempotency trap in the alias probe.** Re-applying the same long name saw
   the entry's *own* `~1` alias as a collision and bumped it to `~2`. This is why
   `derive_name` takes an `except_short` argument.
3. **Data loss: creating a file into a freed slot destroyed the *next* entry.**
   `write_run` "re-terminated" the directory by writing `0x00` into the slot after
   the new entry, unconditionally. `alloc_run` takes the **lowest** free slot, so
   with `[A][B][C]`, deleting `B` and creating `D` puts `D` in `B`'s slot and then
   zeroes `C` — `C` disappears from the directory with its data clusters still
   allocated and now unreferenced. This was **not** introduced by rename; rename
   only made it reachable, because it is the first operation to allocate a run in
   front of a live entry. The write is now gone entirely: it was never needed,
   because `alloc_run` only hands out slots that are already `0x00` or `0xE5`.
   It is worth naming as a class — *every* filesystem bug of this phase was a
   **writer/reader pair agreeing with each other while both disagreeing with the
   spec or with the neighbours on disk.** A round-trip test cannot see any of them.

**Verification.** Boot self-test now **8 passed, 0 bad**, adding three cases:
rename preserves a long filename; rename to the same long name is idempotent;
creating into a freed slot keeps the neighbouring entry. The last one is
deterministic precisely because `alloc_run` takes the lowest free slot, so A, B and
C land on three consecutive slots and D is forced back into the middle one.

`test-fs` PASSED · `test-block` PASSED (hits 174 / misses 6) · `test-fs-corrupt`
PASSED · `test-sec` PASSED · `test-signin` 17/0 · `test-surface` PASSED ·
`test-input` PASSED. Full sweep re-run after the final edit, sequentially.

**Known limits of 1.2.1:**

- **Rename still moves the directory slot, not the clusters.** Only the slot moves,
  so it stays O(1) in file size, but the entry's `dir_index` changes. Anything that
  caches a `RawEntry` across a rename is now stale where before it stayed valid.
- **Still same-directory only.** Cross-directory moves need copy + delete and are
  rejected.
- **The new run is written before the old slots are freed.** A failure between the
  two leaves the file reachable under both names. The opposite order would leave it
  with no slot at all, so this is the safer of the two, but it is not atomic.
- Everything 1.2 still carried: zero timestamps (1.3), no durability (1.4),
  Latin-1 aliases, and no host-side validation by Windows.

##### Phase 1.2 result — long filenames, and the reader was as broken as the writer

Shipping the *write* was the easy half. The surprise is that FAT32 LFN **read**
had been silently wrong the whole time, in a way that hid itself: `scan_dir`
read its characters from `slot[14..28]` and `slot[28..30]`. The real layout is
5 units at `1..11`, **6 at `14..26`**, and **2 at `28..32`** — so the old read
pulled the entry's own first-cluster bytes in as a character and dropped the
13th. Every name read back with a garbage character spliced in at the tail.
Windows-created LFNs were never actually verified, so nothing caught it; the
only reason userspace did not fall apart is that our tests use 8.3 names.

What shipped, all in `fat.rs`:

| Item | Role |
|---|---|
| `needs_lfn` | Decides 8.3-vs-LFN from the same acceptance rule as `make_short_name`, so a legal short name never gets redundant LFN slots. |
| `make_unique_short` | Derives `NAME~1.EXT` probing the directory, so two long names never collide on one short entry. |
| `lfn_slots` | Builds slots in physical reverse order (`N…1`) with `LAST_LONG` on 1, plus the rolling checksum in byte 13. |
| `create_entry` | Now allocates `1 + lfn.len()` **contiguous** slots (a reader scans backwards from the short entry, so a gap loses the name) and re-marks the following slot `0x00` to keep the directory terminated. |
| `scan_dir` | Fixed byte offsets; validates the run's checksum; **clears the pending run on `LAST_LONG`** so an orphaned run in front of a new entry cannot merge two names into one. |
| `delete_entry` | Reclaims the entry's LFN slots, not just the short one. |
| `lfn_self_test` | Boot self-test via a new `FileSystem` trait hook, run in `vfs::init`. |

Five bugs found on the way, all of which a unit test would have missed:

1. **The self-test itself was wrong first.** It called `write_at` with bare
   names (`"SHORTNAME.TXT"`); the VFS path contract is absolute, so all three
   cases returned `NotSupported` before the driver ran. Worth recording: a test
   that passes a bad path proves nothing about the path.
2. **The LFN reader's byte offsets were wrong** — the subject of this section.
3. **`lfn_checksum` overflow-panicked.** `(sum & 1) << 7` plus the byte exceeds
   `u8`. It is a mod-256 sum, so it is computed in `u16` and truncated once.
4. **`LAST_LONG` was written to the wrong byte — and no test could have caught
   it.** Byte 12 is the LFN *type* byte (0x00); the flag is bit 6 of the
   **sequence** byte, i.e. byte 0. Our reader only ever took the order from
   byte 0's low bits and never looked at bit 6, so writer and reader agreed
   perfectly and the round-trip passed. Any *other* reader — Windows scanning
   backwards for the flag — would never stop. This is the clearest argument in
   the phase for checking against something other than ourselves: the layout is
   now cross-checked against two independent implementations
   (`monogon/osbase`, `vfat-rs`) and matches them field for field — sequence +
   `0x40`, 5 chars at `1..11`, attr `0x0F` at 11, type at 12, checksum at 13,
   6 chars at `14..26`, zero cluster at `26..28`, 2 chars at `28..32`.
5. **`delete_entry` orphaned its LFN slots**, which then corrupted an unrelated
   later create (the self-test's own cleanup surfaced it). Fixed by reclaiming
   the run, and made survivable in the reader.

One trap worth writing down, because the obvious fix is the wrong one: the
forward scan cannot simply *reset* on `LAST_LONG`. Order 1 is stored **last** in
a run, so clearing there discards the very run it terminates — and every
multi-slot name breaks. `scan_dir` instead restarts the run whenever the
descending sequence breaks, which is what makes an orphaned run harmless.

**Verified:** boot self-test `[lfn] done: 5 passed, 0 bad` (11-char one-slot,
20-char two-slot reverse order, lowercase round-trip, colliding-base aliases,
and rename-drops-the-stale-name) · `test-fs` PASSED (stat/seek/rename/unlink) ·
`test-shell` PASSED (`fstest` + `lnxtest` from ring 3) · `test-block` PASSED
(hits 174 / misses 6) · `test-fs-corrupt` PASSED · `test-sec` PASSED (SMEP/SMAP).

**Known limits of this phase:**

- **`rename` cannot give a file a long name.** Superseded by 1.2.1, which does.
  What is still true: rename is same-directory only, and it moves the directory
  slot rather than the clusters.
- **Directory timestamps are only stamped at creation**, so a long-named file has
  a usable creation time but a recursive "what changed" scan will miss files
  added to or removed from a directory. Partly 1.3.
- **Validated against a second implementation, not against Windows.** 1.5 added
  `tests/fat32-verify.ps1`, an independent host-side reader — and it immediately
  found that our checksum and chunk ordering were both wrong. Windows itself
  still has not opened an image we wrote (`Mount-DiskImage` refuses it on this
  host). So "the spec is satisfied" now rests on a second implementation, not on
  the vendor. See `docs/OPEN_ISSUES.md` A3.
- **Latin-1 only.** Names are UTF-16 in and UTF-16 out, but the alias generator
  filters to ASCII, so a purely non-ASCII name degrades to `FILE~1`.

### P16.1 — Phase limits: what each stage does **NOT** give you

A plan that only lists what a phase adds is misleading. Each phase below is
followed by the thing it is routinely mistaken for — the capability that is
*still missing* once the phase is green. Read these before believing a phase
"finished".

#### Phase 0 (DONE) — hardening and tidying

**You get:** SMEP/SMAP enforced; sound page-table aliasing; a bulk `fill_row`.

**You do NOT get:**
- **Process isolation.** All ring-3 programs still share one address space and
  can read/write each other's memory *and the kernel's*. SMAP/SMEP stop a
  program *executing* kernel code and stop *accidental* kernel reads of user
  pages — they do nothing about a kernel that is handed a bad pointer. This is
  finding A and it is Phase 3.1.
- **KPTI.** Meltdown's page-table side channel is completely unmitigated.
- **Durability.** Nothing written is guaranteed to survive a power cut.
- **Speed.** The 0.3 change is *reasoned*, not benchmarked. No number here is a
  measurement.
- **A working `blit` fast path** — reverted, see above.

#### Phase 1 — cheap correctness (1.1, 1.2 done)

**You get (after 1.1, 1.2):** disks above 128 GiB are addressable; FAT32 names
round-trip through the real on-disk layout, so files Windows made with long
names are visible under those names, not just their 8.3 aliases.

**You do NOT get:**
- **Any speed from the address width.** Still PIO — see the note above. A 4 TB
  disk is *reachable* and *unusably slow*.
- **Filesystem integrity.** ext2 is still revision 1, single block group, **no
  journal**. 1.4 adds `fsync`, which makes writes durable *to the disk*; it does
  **not** make the filesystem crash-consistent. A power cut mid-metadata-update
  still corrupts ext2. Only ext4 (2.4) fixes that.
- **Durability at all.** 1.2 makes a *name* survive; it does not make its
  *contents* survive a power cut. Nothing is fsynced until 1.4.
- **Long names on rename.** Fixed in 1.2.1: rename now writes the LFN run and moves
  the slot. Still same-directory only, and it moves the directory slot rather than
  the cluster chain.
- **Timestamps.** Read and written on both filesystems since 1.3, but kernel-driven
  only (no `utimensat`), directory mtime does not move when its contents change,
  and nothing outside this kernel has ever read them back.
- **Windows-verified interop.** Superseded by 1.5: a second, independent reader
  now validates what we write. Windows itself still has not, so read this as
  "spec-conformant by two implementations", not "confirmed by the vendor".
- **A modern FS.** ext2 has no extents and no 64-bit; it does have timestamps now.

## Repository layout (2026-10-02)

Tests and generated output were both loose in the repo root, which made the root
hard to read and hid which files were actually source. Now:

```text
tests/            every test-*.ps1 suite (33), plus run-all.ps1
logs/<suite>/     serial.log per suite  (git-ignored)
logs/misc/        old build captures, LLVM IR and stray logs, moved out of the root
```

Three rules the move established, all mechanical and worth keeping:

- **`$root` is the parent of `tests/`.** Each script derives it from its own path,
  so it is `Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)`.
  Getting this wrong points a suite at a non-existent `target\debug\images` and it
  fails for a reason that has nothing to do with the code under test.
- **Logs live in the repo, scratch images do not.** The serial log is small,
  useful, and the artifact you actually want after a failure. The 40 MB disk
  image is disposable and stays in `%TEMP%`.
- **Suites run sequentially, and `tests/run-all.ps1` enforces it.** Every suite
  begins with `Stop-Process qemu-system-x86_64`, so two at once kill each other's
  VM and produce failures indistinguishable from real regressions. The runner
  kills QEMU between suites, summarises to `logs/run-all/summary.log`, and
  supports `-Only fs,block,m7` to run a subset.

#### Phase 2 — graphics and filesystems (planned)

**Intended to give:** damage that actually reaches scanout; huge pages; virtio-gpu
as the live backend; a journalled filesystem.

**Known limits to expect:**
- **Bridging surface damage to scanout will NOT make the compositor fast.** It
  makes it *correct*. The present path is still a software copy through
  `TRANSFER_TO_HOST_2D` with no hardware acceleration and no real vsync — that is
  Phase 2.3's job and, ultimately, M15.
- **ext4 without a journal replay policy is still a corruption risk** on power
  loss. `fsync` + `jbd2` ordering gets most of the way, but ext4's defaults
  (`data=ordered`) are a compromise, not a guarantee.
- **Giant unwritten extents / delayed allocation** interact badly with a
  write-through cache that has no `fsync`. 1.4 has to land *first* or 2.4 will
  look fine and lose data.
- **Huge pages do nothing for the present path** — they help TLB pressure on the
  CPU side, not DMA.
- **Finishing virtio-gpu will not make dispi disappear.** dispi stays as a
  fallback, which means two code paths must keep working forever.

#### Phase 3 — the process model (planned)

**Intended to give:** per-process address spaces; `mmap`; migration; virtio-blk.

**Known limits to expect:**
- **Per-process CR3 without a proper VM subsystem is isolation, not virtual
  memory.** Address spaces will be forked/copied wholesale. No COW, no
  swap, no ballooning, no THP-backed anonymous memory.
- **Enabling migration does NOT make the scheduler good.** It removes the
  affinity filter; it does not add a load balancer, does not fix the measured
  ~134 ms wake latency, and P2's deadline timer (still open) remains the actual
  pacing fix.
- **KPTI becomes *possible* but is not automatic.** Isolation makes a full
  per-process CR3 table affordable; it does not build one.
- **virtio-blk ends the PIO problem but is still not a real storage stack** —
  no NCQ, no write barrier, no TRIM/discard, and `blkcache` stays a
  write-through cache in front of it.
- **`mmap` without `madvise`/`mremap` tuning is a blunt instrument**, and file
  mapping without page-cache coherency (`msync` semantics) will be subtly wrong.

#### Phase 4 — connectivity and Windows binaries (planned)

**Intended to give:** a network stack; running PE binaries.

**Known limits to expect:**
- **A hand-written TCP is not a hardened TCP.** There are decades of
  state-machine, congestion-control and reassembly attacks that a from-scratch
  implementation has to survive. "It handshakes" is not "it is safe on the
  internet".
- **Running Windows `.exe` requires syscall emulation, and Windows is not
  POSIX.** The hard parts are the Win32/NT syscall surface, the loader's
  relocations and imports, and — not optional — **no memory safety**. A PE
  binary gets raw pointer access into the kernel's address space unless Phase 3.1
  isolation is genuinely solid. Phase 4 without Phase 3 is the single worst
  ordering mistake available in this plan.
- **No GPU acceleration.** Even with a network and PE loader, rendering is CPU
  compositing. M15 is the capstone that makes it a gaming OS, and it is last for
  a reason.

#### The uncomfortable summary

The fastest-to-slowest ordering means the **biggest** limitations land last.
Concretely: after Phase 2 the OS will look the most complete and be the least
safe. A green Phase 2 is not a shippable OS — Phases 3 and 4 are what turn "a
demo that boots" into "something you could daily-drive", and Phase 4.7 in
particular should not be started until 3.1 is genuinely done.

#### Phase 2 — Big but self-contained (~6–10 weeks)

- 2.1 2 MiB huge pages
- 2.2 **surface→scanout damage bridge** — unblocks every future app
- 2.3 finish virtio-gpu, demote dispi to fallback
- 2.4 **ext4 driver** (journal + extents + 64-bit + feature flags)

#### Phase 3 — The hard rework (~2–3 months)

- 3.1 **per-process CR3 + address spaces** (finding A)
- 3.2 `mmap`/`munmap`/`mprotect`/`madvise` (needs 3.1)
- 3.3 shared memory (needs 3.1)
- 3.4 **re-enable scheduler migration** — falls out of 3.1
- 3.5 virtio-blk, replace ATA PIO

#### Phase 4 — The monsters (~4–6 months)

- 4.1 virtio-net · 4.2 Ethernet + ARP · 4.3 IPv4/IPv6 · 4.4 UDP
- 4.5 **TCP** — the single largest item in the project
- 4.6 socket syscall API
- 4.7 PE `.exe`/`.dll` loader (M12)

### The good news in the plan

`TaskCtx` (`scheduler.rs:88`) was already refactored into one owned allocation
specifically so migration could be re-enabled safely — *"That is what makes safe
migration possible again."* Items 3.1 and 3.4 are therefore mostly **unblocking
existing design**, not inventing new architecture.

### Milestone gates

- **End P0** → all suites green; SMEP/SMAP asserted in the serial log
- **End P1** → new `test-fsync.ps1`: kill mid-write, verify no corruption
- **End P2** → `test-surface.ps1` proves damage reaches scanout; ext4 mounts a
  64-bit image
- **End P3** → new `test-proc-isolation.ps1`: proc A cannot read proc B
  (today it **can**)
- **End P4** → `test-net.ps1`: loopback + UDP + TCP handshake

---

## UEFI (M9 — done)

**Status:** ✅ the same kernel binary now boots **both ways**: BIOS/MBR
(`bios.img`) and UEFI/GPT (`uefi.img` under QEMU's bundled OVMF firmware,
`share\edk2-x86_64-code.fd` + `edk2-i386-vars.fd` varstore template).

**How it's wired:**
1. The `bootloader` crate's `uefi` feature is re-enabled; `build.rs` assembles
   `uefi.img` next to `bios.img` (both land in `target\debug\images\`).
2. `build.rs` writes our **FAT32 (0x0C) + ext2 (0x83) data partitions into the
   protective MBR's free entry slots** ("hybrid" layout): the GPT entry array
   only carries the firmware's ESP, so the kernel's `block.rs` picks these up
   in `scan_gpt` and mounts them exactly like the BIOS MBR path.
3. The kernel's partition scan prints `M9: UEFI boot — GPT + N hybrid
   protective-MBR data partition(s)` as the UEFI-mode serial marker.
4. `test-uefi.ps1` boots `uefi.img` headless (OVMF pflash pair, `-snapshot`),
   greps serial markers, and grabs a QEMU-monitor **screendump of the GOP
   framebuffer** (decoded to ASCII) — the kernel's header bar is visible, and
   the full M8 flow (disk-loaded shell, autoexec, fstest `PASSED`) runs
   identically under UEFI.

**Build-system notes (portable-toolchain specifics):**
- LLVM converts `CStr16` u16 NUL-scan loops into `wcslen` calls; the
  `x86_64-unknown-uefi` target has no C runtime, so the UEFI sub-build fails
  to link. Fixed by adding a `#[unsafe(no_mangle)] extern "C" fn wcslen` shim
  to the vendored `bootloader-x86_64-uefi-0.11.17/src/main.rs` in the local
  cargo registry (`.toolchain\cargo\registry\src\...`). If that crate is ever
  re-extracted (registry prune / version bump), re-apply the shim.
- QEMU inputs must live in space-free paths: `Start-Process -ArgumentList`
  splits arguments on spaces, which silently tore the `PROJECT OS` path apart.

**Why it matters for gaming:** modern hardware boots UEFI (Secure Boot is
common on gaming rigs). Having a working UEFI path means today's QEMU-verified
kernel has a real shot at a future USB/hardware boot image — even though we
never test on this machine. The GOP framebuffer also feeds the M10 GPU track,
the M9.5 desktop base and the M13 GUI.

---

## M9.6 — Core hardening + missing subsystems (current)

**Why it exists (replaces the deferred M9.5 for now):** before new features, the
kernel gets a hardening + foundation pass aimed squarely at the gaming/latency
vision. Three phases, each step landing with its own headless regression test
on both BIOS and UEFI.

### Phase A — upgrade existing systems
- **A1. High-resolution timekeeping** ✅ done — `kernel/src/time.rs`: hand-rolled
  CPUID (invariant-TSC detect), TSC calibrated against 10+ PIT ticks
  (`time: TSC calibrated: ... Hz`), monotonic `now_ns()` (u128-safe math), RTC
  anchored `realtime_ns()`. New syscall `SYS_GETTIME(0=mono, 1=wall)`. Ring-3
  smoke test in fstest (`test-time.ps1`).
- **A2. APIC interrupt architecture** ✅ done — `kernel/src/apic.rs`: LAPIC
  enabled via IA32_APIC_BASE (device MMIO accessed THROUGH the bootloader's
  physical-memory window — see the note below), spurious vector 0xFF,
  LINT0/1 + error LVTs masked; IOAPIC routes GSI0+2 (PIT — the 8254 output
  is GSI 2 per the ACPI IRQ0 override) -> vector 0x20, PS/2 keyboard GSI1 ->
  0x21, mouse GSI12 -> 0x2C; legacy PIC fully masked (EOI path gated on
  `apic::active()`); **LAPIC timer @ 1000 Hz** (TSC-calibrated, periodic,
  vector 0x30) is now the preemption source — 10x finer slices than the PIT,
  single-MMIO-write EOI, no mutex in the IRQ hot path; the PIT keeps counting
  100 Hz uptime ticks for rtc.rs/the ticker. Bring-up runs with IF=0 (a PIT
  interrupt landing inside `pic::mask_all()`'s PICS-lock window self-deadlocked
  — found and fixed via the A2 test). Test: `test-apic.ps1` incl. a **real
  keyboard end-to-end** (QEMU monitor `sendkey` types `echo hi`; the shell
  printing `hi` proves i8042 -> IOAPIC -> LAPIC -> ring).

  **Debugging notes (three real bugs the tests caught):** wrong LAPIC
  EOI/SIVR offsets (0x0C0/0x100 vs the correct 0x0B0/0x0F0) froze the kernel
  after the first timer tick; the PIT is delivered on GSI 2, not GSI 0 (ACPI
  IRQ0 override); and the ORIGINAL MMIO scheme (fresh page-table entries under
  the bootloader's dynamic higher-half mapping) **silently aliased kernel
  accesses onto device registers** — corrupted-control-flow #GP + per-boot
  random hangs. Root-cause fix: device MMIO is accessed through the
  bootloader's existing physical-memory window (`phys_offset + phys`), i.e.
  `memory::phys_offset()` — zero new page-table entries, zero aliasing risk.
- **A3. Scheduler v2** ✅ done — priority classes (RT 0 > Normal 1 > Idle 2),
  task states (Running/Ready/Sleeping/Blocked/Dead), explicit time slices
  (RT 1 tick / Normal 8 ticks), a sleep queue woken on 1 ms LAPIC deadlines,
  waking blocked-on-input tasks via `keyboard::line_pending()`, and blocking
  `SYS_READ(0)` (the shell no longer busy-polls stdin). New syscalls
  `SYS_GETPID`/`SYS_SLEEP`/`SYS_YIELD`; kernel-side `sleep_kernel`/
  `spawn_prio`/`set_priority`. Enabler: the syscall frame is now
  self-contained — the exit stub restores the user RSP from the in-frame slot
  instead of the shared `SCRATCH`, so a task can yield mid-syscall safely.
  RT tasks hold the CPU until they block/yield (test-proven: no Normal line
  interleaves an RT burst); idle only runs when nothing else is ready.
  Test: `test-sched.ps1` (RT isolation, 500 ms sleep cadence, interactive
  blocking input via QEMU `sendkey`).
- **A4. FPU/SIMD save-restore** — extended beyond the base fix below if needed
  (lazy XSAVE opt-in later).
- **A5. Block layer performance** ✅ done — `kernel/src/blkcache.rs`: 1 MiB
  sector cache (2048 × 512 B, second-chance clock LRU) under
  `AtaDrive::read_sectors`/`write_sectors`, so FAT32 + ext2 are accelerated
  with zero driver changes. Write-through (disk first, then cache — never
  dirty), coalesced multi-sector raw reads for miss runs, 4-sector sequential
  read-ahead, hit/miss counters. Boot-time `blk_test` proves: **x1098 hot-read
  speedup** on /SHELL.ELF, byte-equality across the cache, read-ahead warming
  during cold reads, write-through coherence across a flush, and a
  deterministic read-ahead probe. Debugging note: the original `insert`
  didn't dedupe — a write of a previously-read sector left a stale duplicate
  that first-match `find` kept returning (broke every read-after-write, incl.
  fstest subdirectory creates); `insert` now updates in place. Test:
  `test-block.ps1`.
- **A6. Memory manager** — **done** — frame allocator v2: the M2 bump allocator (O(frames-so-far) per alloc, no free path at all) replaced by a two-tier O(1) design — intrusive free list (next-pointers stored in the free frames themselves through the phys window, no heap needed) served first, region-bump cursor for fresh frames, `deallocate_frame` inherent method (the x86_64 trait has no free), alloc/free/OOM counters surfaced in the `perf` snapshot (`[perf] frames:` line), and `with_global_frames` now enforces IF=0 itself. Kernel heap grown 2 MiB → 16 MiB. Regression: `test-memory.ps1`.

### Phase B — missing subsystems
- **B1. PCI enumeration** ✅ done — `kernel/src/pci.rs`: legacy CF8/CFC config
  scan over bus 0–255, type-0 header decode, multi-function handling, BAR
  base-address reads (discovery-only; the destructive size probe is deferred
  to M10), vendor/device name table + class classification. Device registry
  feeds the shell `lspci` (SYS_LSPCI = 12) and the M10 GPU track (the VGA
  framebuffer BAR base comes out as `bar0=mem: fd000000`). Test: `test-pci.ps1`.
  **Two real bugs the tests caught on the way:** (1) `serial_writeln` held a
  spinlock with interrupts enabled — the 1 ms timer could preempt a task
  mid-write and the next task's print deadlocked forever (exposed by the PCI
  scan's long lines; the kernel-wide serial path is now IF=0-atomic); (2) the
  PCI config port is a two-step select/read handshake — a preemption between
  the two wedges QEMU's PIIX config port permanently (the scan hung at a
  different device/bar each run). Fix: the scan runs with interrupts disabled
  (before the scheduler starts), making every config transaction atomic by
  construction; runtime `lspci` only re-renders the cached registry.
- **B2. ACPI core** ✅ done — `kernel/src/acpi.rs`: RSDP from `BootInfo` (validated
  signature; legacy-EBDA fallback), RSDT/XSDT walk, checksum-validated tables,
  MADT decode (LAPIC + IOAPIC + IRQ source overrides), HPET + FADT presence.
  All reads are unaligned little-endian through the physical-memory window (no
  new page tables; ACPI tables live at odd addresses — a naive aligned read
  panics). The boot-time MADT output cross-checks the A2 wiring exactly
  (`IOAPIC addr=0xfec00000`, `IRQ 0 -> GSI 2`, `LAPIC 0xfee00000`); the
  cached IOAPIC/GSI info feeds M9.8 SMP. Test: `test-acpi.ps1`. Debug notes:
  ACPI revision-0 RSDP + QEMU's 56-byte HPET (base field absent) are handled;
  `memory::init` moves `boot_info`, so the RSDP pointer is captured before it.
- **B3. Process lifecycle** ✅ done — `SYS_WAITPID` (0 = wait any child; blocks
  mid-syscall on the self-contained frame), `SYS_KILL` (self-kill reuses the
  exit path; foreign kill = zombie with code 137), per-task exit-status records,
  zombie reaping (parent `waitpid` + auto-reap of kernel/orphaned children in
  the scheduler's wake pass), per-task fd tables dropped on exit/kill. The
  shell's `run` now blocks in `SYS_WAITPID` and reports the child's exit code.
  Also landed: `b3_test` kernel regression (blocked-waitpid reaps 42, kill →
  137, orphan auto-reap) and the scheduler's `wake_state_locked` orphan pass.
  Debug notes: `SYS_WAITPID` had to loop over `try_reap`/`block_on_child` (a
  wire-race where the child dies between checks); the shell's `sync` command's
  SYS_FLUSH id was corrected to 13 (was 12 = LSPCI). Test: `test-proc.ps1`.
- **B4. Raw input event ring** ✅ done — `kernel/src/input.rs`: one unified raw
  event ring fed by BOTH IRQ handlers. Every PHYSICAL key event (pc_keyboard
  KeyCode + Down/Up — releases included, which the decoder suppresses for
  non-toggle keys) and every completed mouse packet (relative screen-space
  dx/dy + BTN_* bitmask) is pushed as a fixed **24-byte little-endian record**
  (kind/flags/code/x/y/ts_ns) stamped with the A1 monotonic ns clock. Ring-3
  consumes via **`SYS_INPUT_READ` (16)**: copies full records into a validated
  user buffer; `blocking != 0` parks the task on a new scheduler state
  (`blocked_on_raw`) resumed by the 1 ms wake pass when `input::pending()`.
  The shell's line discipline is untouched (parallel side channel). Kernel
  regression `input_test` (runs every boot): hand-decoded wire-layout check
  (mouse -123/45 + key-up A, decoded with plain integer ops), drop-on-full
  (256 kept / 44 dropped), and block-on-raw wake (child pushes after 150 ms;
  parent wakes through the real scheduler path). Ring-3 E2E `EVTEST.ELF`
  (linked at 0x1000000): prints `[ev] K down/up code=.. ts=..` /
  `[ev] M x=.. y=.. b=.. ts=..`, exits 0 after seeing both kinds; the shell's
  B3 `run` reports `run: exit code 0`. **Two bugs the tests caught:** (1) the
  ring's `next == TAIL` guard dropped one event early when exactly one slot
  was free (full ring has HEAD == TAIL — indistinguishable from empty without
  COUNT; observed 255/45 instead of 256/44) — full-check now uses COUNT alone;
  (2) evtest initially linked at the default base and collided with hello's
  region ("region overlaps") — user programs each need an explicit
  `--image-base` build.rs (now 0x400000/0x800000/0xC00000/0x1000000). Also:
  QEMU `sendkey` needs symbolic names ('slash'/'dot') — literal '/'/'.'
  silently drop the keystroke. Tests: `test-raw.ps1` (kernel + E2E) +
  `test-input.ps1` (cursor/line-discipline observability, unchanged). In
  `bios-gui`, QEMU grabs the mouse only after a click into its window
  (release: Ctrl+Alt+G).
- **B5. Latency instrumentation + `perf`** — **✅ done**: boot snapshot + shell re-render landed; the rare snapshot freeze (~1-in-6 boots) was root-caused to `console_bytes` holding the CONSOLE/FB spinlocks with IF=1 while IF=0 syscall paths (ring-3 `SYS_WRITE` echoes, `take_line`) re-acquire them — fixed with the codebase's never-hold-a-lock-across-a-preemption discipline (`docs/B5-DEBUG-STATE.md`). It measures rdtsc IRQ-latency histograms,
  switch/syscall counters, cache hit rate — the "ultrafast" feedback loop.

### Phase C — ABI & file-API foundation (shared by M9.7 Linux-ABI and M12/M14 PE)
- **C1. argv/envp/auxv** ✅ **done** — the kernel builds the full System V
  AMD64 process-start stack at spawn: `[rsp] = argc`, argv[] (NULL-term),
  envp[] (NULL-term, empty until C3), auxv pairs (AT_PHDR/PHENT/PHNUM/
  PAGESZ/ENTRY/RANDOM/EXECFN, AT_NULL last; AT_RANDOM = 16 TSC-mixed bytes)
  with the strings above, rsp 16-aligned pointing at argc. `SYS_SPAWN` now
  tokenizes the spawner's command line: argv[0] = ELF path, remaining
  whitespace-separated tokens = argv[1..] (backward compatible: a bare path
  spawns with `argv = [path]`; the shell needs no changes). Ring-3
  `ARGTEST.ELF` (naked `_start` → `main(argc, argv, envp)`, the glibc
  idiom) reads everything back — regression: `test-args.ps1`.
- **C2. Syscall hardening** ✅ **done** — user-pointer validation (C2-pre: all
  user pointers checked with `user_buf_ok` + retried without copy-on-write)
  + errno normalization. New `kernel/src/errno.rs` with the Linux errno
  table (EPERM/ENOENT/ESRCH/EIO/E2BIG/ENOEXEC/EBADF/ECHILD/EAGAIN/ENOMEM/
  EFAULT/EEXIST/EINVAL/EMFILE/ENOSPC/ENAMETOOLONG/ENOSYS); failing syscalls
  now return `-errno` in rax (`0xFFFF...` = -1 = -EPERM, so the old M4
  `u64::MAX` sentinel still decodes as *a* generic error; the only pre-C2
  callers that broke compared against the exact sentinel and were migrated
  to `is_err`). Spawn errors map to ENOENT/E2BIG/ENOEXEC/EEXIST/ENOMEM;
  VFS errors via `fs_err` (NotFound→ENOENT, Exists→EEXIST, NoSpace→ENOSPC,
  Io/BadFs→EIO). Also fixed: `SYS_READ` now returns EBADF for an invalid
  fd even when the supplied buffer is itself unwritable (fd validity wins
  over buffer validity, Linux order); fstest asserts every failure class
  (`test-args.ps1` "errno OK").
- **C3. File API completion** ✅ **done** — `SYS_STAT`(18)/`SYS_SEEK`(19)/
  `SYS_UNLINK`(20)/`SYS_RENAME`(21) plus a real mount table replacing the
  hardcoded FS/FS2 pair. `kernel/src/vfs.rs` now holds `MOUNTS: Mutex<Vec<Mount>>`
  (FAT32 + ext2 both at "/", longest-prefix match with FIFO tie-break; the
  re-entrant-lock bug on pick_mount fixed by passing the locked slice). The
  `FileSystem` trait gained `stat`/`unlink`/`rename`; `FileStat { size, is_dir }`
  is returned through a caller buffer. `fat.rs` implements all three (rename is
  same-directory only — rewrites the 8.3 name in place; unlink marks the slot
  deleted + frees the chain) and `ext2.rs` adds stat, same-dir rename, and
  unlink with block/inode-bitmap + free-count reclamation. `sys_seek`
  implements SET/CUR/END (END stats the file) with EBADF/EINVAL; stat/unlink/
  rename map errors via `fs_err` (including the new `EISDIR`). Ring-3: fstest's
  C3 block asserts stat size + is_file/is_dir and each errno class
  (ENOENT/EFAULT/EINVAL/EBADF/EEXIST/EISDIR) and gates them into PASSED
  (`test-fs.ps1`); the shell gained `stat`/`rm`/`ren` (SYS_RENAME's 4th arg
  rides in r8, which `syscall` doesn't clobber).

### Latent-bug fixes landed in this milestone (found by the pre-upgrade audit)
1. **FPU/SSE state corruption** — `context_switch` saved only GP registers;
   every task (kernel + ring 3) is compiled with baseline SSE, so live XMM
   values were silently corrupted across preemptions. Fixed with eager
   `fxsave`/`fxrstor` inside the naked switch (`kernel/src/fpu.rs`, 16-aligned
   per-task areas, template-seeded so a fresh `fxrstor` can't unmask FP
   exceptions). Regression: two tasks verify exact XMM checkpoints
   (`fpu-test: task A/B PASSED`, `test-fpu.ps1`); `#NM` handler is now a
   "must never happen" diagnostic.
2. **Unvalidated user pointers** — `SYS_READ`/`SYS_WRITE`/path syscalls
   dereferenced user pointers unchecked (kernel memory access / ring-0 page
   fault = panic). Fixed with `user_buf_ok` (range + page-table walk via
   `memory::page_flags`, USER_ACCESSIBLE/WRITABLE enforcement). Regression:
   fstest badptr checks from ring 3 (`fstest: badptr rejected OK`).
   Note: user *stacks* live at 0x4000_0000 + slot×0x1000_0000, so the range
   bound is 1 TiB — the page walk is the real security gate.

Known design notes (documented, deliberately unfixed until A3): syscalls run
with IF=0 for their whole duration (spinlock discipline + the single
SCRATCH-slot syscall ABI make this required today) — long syscalls like
SYS_SPAWN add IRQ latency; A3's syscall framing work addresses it.

---

## M9.7 — Linux ABI compat: run static Linux ELFs

**Status:** ✅ **done** — the syscall entry routes Linux-ABI tasks through a
number-compatible shim (`linux_dispatch`: write/read/open/close/fstat/lseek,
mmap/munmap/brk, getpid/getppid, getrandom, clock_gettime, exit(_group),
wait4, arch_prctl ARCH_SET_FS/GET_FS — arg 4 in r10, `-errno` returns). The
loader detects Linux-ABI images (`ONYXLNX\0` marker or ET_DYN + dynamic
section) and serves them the full C1 process-start stack; per-task brk/mmap
state lives in the existing stack-slot registry and dies with the task.
ET_DYN (static-PIE) images load at a fixed PIE region (`0x2000000` +
preferred vaddrs) with R_X86_64_RELATIVE — and any GLOB_DAT/JUMP_SLOT —
relocations applied before entry; targets are validated against the loaded
segments. `AT_BASE` reports the load base for static-PIE TLS setups.

**Verified:** `LNXTEST.ELF` (in `user/linuxtest`, built by the root `build.rs`
with `rustc --target x86_64-unknown-linux-gnu` + `rust-lld -shared -static`)
is a genuine Linux-ABI static-PIE binary; booted from the shell AUTOEXEC it
exercises brk, anonymous mmap (write-back through the mapping), FS-base TLS,
kernel-applied PIE relocations (read through a relocated `.rodata` pointer),
getpid, getrandom and clock_gettime from ring 3 and prints
`[lnxtest] PASSED`. Regression script: `test-lnx.ps1`. The M9.6 suite
(`test-args.ps1`) still passes unchanged.

**Bug found & fixed during M9.7:** adding the `AT_BASE` auxv pair without
growing the stack-block budget (`8 * 16` still assumed) made the trailing
`AT_NULL` pair overflow the block by 16 bytes — landing exactly on the head of
the `argv[0]` string and zeroing it (`argv[0]=` / `at_execfn=` printed empty in
`test-args.ps1`). The budget now counts 9 pairs; both suites verify the full
strings.

**Why:** signature goal #1 ("split personality binaries") is half-built: our
syscall layer already mirrors Linux x86-64 (rax=nr, rdi/rsi/rdx) and the ELF64
loader exists. This milestone runs *unmodified statically-linked Linux
binaries* (musl-static C/Rust) from the FAT32 image. Scope: Linux syscall
number shim over M9.6-C2, argv/envp/auxv (C1), `arch_prctl(ARCH_SET_FS)` TLS,
per-task brk/mmap in the existing 256 MiB stack slots, PIE/ET_DYN loading with
R_X86_64_RELATIVE relocations (shared with M12's PE relocations).
**Explicitly out of scope:** dynamic linking (ld.so/glibc), threads (clone),
signal delivery, sockets.

**Success test:** a static musl hello-world + a static Rust binary dropped into
the image run from the shell via SYS_SPAWN.

---

## M9.8 — SMP: multi-core (its own stage, by decision)

**Status:** ✅ **done** — the kernel runs on every CPU the MADT advertises.
Regression script: `test-smp.ps1` (boots the same image at `-smp 4`, `-smp 2`
and `-smp 1`); the whole M9.6/M9.7 suite still passes unchanged.

### What was built

1. **Per-CPU state (GS base).** `kernel/src/smp.rs` owns a `PerCpu` block per
   CPU, published in `IA32_GS_BASE`; `gs:[0]` is the CPU index. The two slots
   the syscall stub used to share globally now live there too: `gs:[32]` =
   kernel-stack top, `gs:[40]` = parked user RSP. That closes a real SMP hole —
   the old single scratch slot could be clobbered by a second core between the
   entry store and the load, which no `IF=0` protects against across cores.
   Per CPU: current task index, idle RSP + idle FXSAVE area, idle kernel stack,
   pending-handoff slot, LAPIC id/online flag, timer-tick and worker counters.
2. **AP bring-up (INIT-SIPI-SIPI).** A frame below 1 MiB is reserved before the
   heap claims frames (`memory::reserve_low_frame`), then
   `smp::prepare` copies + identity-maps a one-page trampoline into it. The
   trampoline is hand-assembled bytes (16-bit real mode → PAE/CR3/EFER →
   64-bit) with its listing in the source and a dev helper to disassemble it
   (`tools/extract-trampoline.ps1`). `smp::start_aps` re-patches the page per
   AP (plain memory writes — the mapping happens once) and drives
   INIT → deassert → SIPI → SIPI, waiting for each AP's `online` flag.
3. **AP side (`ap_entry`).** Identifies itself by LAPIC id, installs GS base,
   loads its own GDT + TSS (`gdt::init_ap`, so TR/IST/RSP0 are per-CPU), loads
   the shared IDT, mirrors the BSP's CR0/CR4 and EFER **writable** bits, sets
   up FPU/SSE, programs its SYSCALL MSRs (`syscall::init_ap`) and arms its own
   LAPIC timer at the BSP's calibrated interval, then idles in the scheduler.
4. **Scheduler (SMP-safe).** One global `SCHED_LOCK` (always taken with IF=0)
   guards the task table. The context switch deliberately runs *outside* it —
   so the outgoing task keeps `Running` and its owner CPU until the CPU
   schedules again (`release_pending_prev`), which is what stops another core
   from stealing a task whose RSP has not been saved yet. "Current" is per-CPU
   and each task's RSP lives in a stable heap slot, so nothing points into the
   `TASKS` vector while the switch runs.
5. **IPIs.** Vector 0x31 (reschedule) is sent whenever a task becomes Ready
   (`smp::kick_others`), so an idle core never sleeps through new work; vector
   0x32 (TLB shootdown) exists for the first remap/unmap path. Device IRQs keep
   their IOAPIC destination = the BSP, so drivers stay single-CPU by design.
6. **Work for the APs.** Each AP gets its own worker task (`ap_worker_task`): a
   floating-point + integer accumulator that verifies its XMM state every 100k
   iterations and prints a heartbeat, plus per-CPU LAPIC tick counters in the
   boot summary — i.e. the log proves real parallel execution, not just
   "online".

### Deliberate scope (documented limitations)

* **CPU affinity, not migration.** A task only ever runs on the CPU that
  spawned it (the filter in `plan_switch`). The file-system, page-table and
  device layers are still single-CPU structures; keeping each task on its core
  keeps all of them serialized by construction. Real migration needs those
  layers locked (the TLB IPI is already in place for it).
* **Single shared run queue.** Per-CPU queues are the natural follow-up once
  migration exists; today the run queue is global with per-CPU "current".
* **Boot context.** `kernel_main` stops being scheduled once preemption starts
  (nothing is Ready while the peripheral tasks are), which is why the AP
  bring-up runs as a **task** (`smp::bringup_task`) rather than inline. The
  same pre-existing effect is why `apic::init`'s closed-loop correction and
  `input::init` never complete on this build; M9.8 works around it by
  publishing the first-guess timer interval immediately and by having an AP
  leave its timer masked if no interval was published (a near-zero count would
  storm that core). Fixing the boot-context starvation itself is its own item.

### M9.8 completion pass — bugs found in my own logic and how each was solved

Logged verbatim as requested; every item below was found by a failing test or a
serial trace, never assumed.

1. **Boot context starvation (fixed).** The pre-existing effect above meant the
   LAPIC-timer closed-loop calibration and the `input` ring self-check never
   ran. Fix: moved the post-`apic::init` boot steps into a kernel task
   (`main.rs`), so the heartbeat `[diag]` line and the interval correction are
   back; the two M9.8 workarounds (first-guess interval publish, AP-timer
   mask fallback) are retained only as safety nets.
2. **AP trace noise.** ~10 serial lines per AP at 115200 baud slowed every
   boot. Fix: `ap_trace!` gated behind a `const AP_TRACE: bool` switch.
3. **Keyboard ring was a same-CPU-only data structure.** `RING`/`HEAD`/`TAIL`
   were plain `static mut` with the comment "never touched concurrently" — true
   under CPU affinity, false the moment tasks migrate. Fix: lock-free SPSC
   ring on `AtomicU64`/`AtomicUsize` with Release/Acquire ordering (the IRQ
   producer must never spin, so no mutex).
4. **Line discipline was serialized by IF=0 only.** `take_line`'s `LINE` buffer
   is a *shared terminal*; IF=0 excludes only same-CPU interrupts, not another
   CPU's task calling `SYS_READ(0)`. Fix: dedicated `LINE_LOCK` mutex (chosen
   over the KSL because the echo path takes the framebuffer lock and a mouse
   IRQ needs the KSL — holding the KSL across an echo would make that IRQ spin
   for milliseconds). `line_pending()` became lock-free (reads the SPSC ring
   only), because the scheduler's wake pass calls it under `SCHED_LOCK`.
5. **Mouse state had a genuinely broken critical section.** `read_packet` used
   `disable(); ...; enable()` — (a) it re-enabled interrupts *unconditionally*,
   destroying an outer IF=0 section such as a syscall, and (b) IF=0 excludes
   only this CPU's IRQs while the mouse IRQ can be delivered on another core.
   Fix: both `handle_irq` and `read_packet` take the KSL; the IRQ handler
   releases it *before* the input-ring push and cursor move (both take locks
   of their own — a nested KSL acquisition would reentrancy-panic).
6. **CPU affinity removed → work stealing.** The M9.8 pick loop filtered by
   `owner_cpu`; that filter was the *serialization mechanism* for all unlocked
   single-CPU structures. Fix: removed the filter, added steal-with-local-
   preference (highest priority wins; ties prefer the owner CPU), made safe by
   the ksl/locks from items 3–5 plus `SCHED_LOCK` claiming.
7. **Stall diagnostic false positive under stealing.** "Nothing Ready for 8
   ticks" is a *normal steady state* on 4 cores (other tasks Running elsewhere
   or blocked). Fix: the trigger now requires *provable* starvation — a sleeper
   past its deadline, or an input-blocked task with events actually pending.
8. **MAIN's saved state was per-CPU while MAIN is global.** With migration the
   one boot task could resume on another CPU with a different per-CPU image.
   Fix (audit result): `fpu_ptr(MAIN)`/idle sp/kstack stay per-CPU *idle
   contexts* by design — the boot task never migrates because it is the only
   thing each CPU's idle path picks; audited, no change needed, documented.
9. **THE BIG ONE — `xsave64`/`xrstor64` ran with a garbage EDX:EAX mask.** The
   naked `context_switch_xsave` never loaded the state-component mask operand,
   so every save/restore covered a *random subset* of the state. Symptom
   chain: `-smp 1` passed by luck (benign leftover register contents), `-smp 4`
   failed only when the AVX task migrated — images with `XSTATE_BV=0x1`
   (x87-only save of a task with live YMM), exactly the "upper YMM lanes zero,
   lower lanes intact" signature. Isolated by: per-CPU `live_xcr0` probe
   (all 0x7 → not an XCR0 problem), per-switch `[avxsw]` trace with image
   headers (saves never produced bit 1/2), image YMM-region dump (region zeros
   with live legacy data = fxsave-shaped write), a live-vs-image ymm0 probe
   after the switch (matched, both zero → save-side). Fix: the switch takes the
   programmed `XCR0` as a 5th argument and sets `mov eax, r8d; xor edx, edx`
   before *each* of `xsave64`/`xrstor64` (rdx is the pointer operand, so it
   first moves to r9); `fpu::save_into` (template capture) had the same bug and
   got the same fix. Result: `test-avx.ps1` 74+74+53 consecutive rounds, zero
   failures, on `-smp 4 -cpu max` *and* `-smp 1`.
10. **Build-image staleness (my process error).** After editing the kernel I
    once ran only `cargo build` and tested against the *old* `bios.img`
    (recognized because the failing log printed the pre-instrumentation
    message format). Rule recorded: always rebuild the image with
    `build.ps1` before any QEMU test.
11. **Test-script flake under host load.** `test-sched.ps1`'s "sleeper woke"
    counter and one batch run of `test-pci`/`test-perf` failed once each and
    passed on immediate retry with no kernel change — host-load timing flakes,
    not kernel regressions. The final full sweep: all 25 suites exit 0.
12. **Parallelism benchmark findings (measurement, not a bug).** The first
    benchmark runs exposed two things worth recording: (a) software TCG
    time-shares ONE emulation thread across the vCPUs, so at `-smp 4` every
    core runs at ~1/4 speed — SMP can only break even there, never speed up;
    (b) switching the harness to WHPX (Windows Hypervisor Platform) gives real
    hardware vCPUs: 4 workers go from 782 ms serial to 156-266 ms at `-smp 4`
    (**x2.9-5.1**). Also found while tracing: the per-AP demo workers busy-spin
    between 100 ms sleeps, and the claim-then-migrate pattern means all Ready
    workers may *start* on one CPU yet still finish in parallel — the
    benchmark therefore records completion CPUs, not start CPUs. The switch
    tracer itself (`bench::TRACE`) stays off by default: an early version
    collided its `usize::MAX` "no worker yet" sentinel with a `usize::MAX`
    current-task sentinel and wedged the first boot switch — kept for future
    debugging only, with the sentinel fixed by never matching the idle index.

### Success criteria

`test-smp.ps1`: at `-smp 4` all three APs reach 64-bit mode, come online with
their own GDT/TSS/IDT/CR0/CR4/EFER/SYSCALL MSRs and timers, run their worker
tasks (non-zero, growing iteration counts, FPU checks passing), advance their
own per-CPU tick counters, and the system keeps passing the earlier milestones'
markers (fstest/fpu-test/argv) with no PANIC, no unexpected exception and no
stall diagnostic. `-smp 2` starts exactly one AP; `-smp 1` starts none and
leaves the single-CPU path untouched.

---

## M10a — GPU scan + kernel-controlled modesetting (done)

**What was built:**

1. **PCI layer extension** (`pci.rs`): display-class devices (class 0x03,
   VGA-compat subclass 0x00 preferred) are identified out of the existing
   registry; their MEM BARs are *sized* (write-probe under IF=0 only, display
   class only — the general PIIX IDE IO-BAR probe stays forbidden). New API:
   `find_display()`, `framebuffer_bar()` (first sized MEM BAR — the LFB is not
   always BAR0), `read_bar_base()` (post-modeset re-read; a mode switch may
   move/resize the VGA window), `size_str()`.
2. **Modesetting driver** (`gpu.rs`, new): talks to QEMU's bochs VBE/dispi
   interface (ports 0x1CE index / 0x1CF data). Boot-path sequence, every step
   with an honest check: save the firmware's "mode as found" → dispi ID probe
   → program the kernel mode → **read the registers back** (QEMU silently
   rejects unbootable geometry by leaving registers unchanged, so read-back is
   the only truth) → re-read the LFB BAR → map it → canary → console hand-over
   → record `GpuMode`.
3. **Mapping**: 8100 KiB (1920x1080x32) at `0x400_0000_0000` (PML4 slot 8 —
   kernel slot 0, phys window slot 5, heap slot 136; `translate_addr`-guarded
   so a live mapping is never clobbered), 4 KiB pages through
   `with_global_frames` (KSL discipline).
4. **Canary**: 192 tagged pixels at top-left / bottom-right / middle-right
   (position+stride folded into each value), written and read back; the 32-bit
   wrapped sums must match — proves the mapping reaches the *device's* memory
   and the row stride is right.
5. **Console hand-over** (`framebuffer::adopt`): re-points the writer,
   scrolling console and mouse cursor at the dispi surface; the bootloader
   framebuffer stays mapped (adopt-able again later).
6. **Graceful fallback**: every failure path logs `[gpu] ... fallback`, counts,
   **restores the firmware mode** and keeps the bootloader framebuffer live.
   Plus a per-boot **fallback rehearsal**: a deliberately bogus mode write
   whose read-back must be caught by the same decision function the live path
   uses, then restored — the graceful-fallback path is exercised and verified
   in every single boot.
7. **Task-context re-verification**: a spawned scheduler task re-reads the
   dispi registers and probes the mapped LFB (proves the modeset survives the
   boot→scheduler hand-over; follows the M9.8 "boot work goes in a task" rule).

**What was measured (std-VGA 1234:1111 qemu-std-vga, class 0300):**

- BARs sized: BAR0 (LFB) 16 MiB default / **128 MiB** with
  `-global VGA.vgamem_mb=128`; BAR2 4 KiB. Dispi id `0xb0c5`, vram 16384 /
  131072 KiB, mode as found `1920x1080x24 enable=1` (SeaBIOS's VBE mode).
- Modeset `1920x1080x32` programmed, read back `enable=1` in both configs;
  canary sum `0xc6593ea0` (128 MiB) / `0xc4847ea0` (16 MiB, pre-resize run at
  1024x768) verified exactly.
- Fallback rehearsal: bogus 16bpp write caught by the read-back check and
  restored + re-verified, every boot (`fallbacks=0 rehearsals=1`).
- External confirmation: QEMU `screendump` geometry follows the *kernel's*
  mode, not the firmware's (1920x1080 after the resize-capable runs; 1024x768
  in the earlier 3 MiB run) — read straight off the monitor socket.
- Scheduler-task probe: the dispi registers still read `1920x1080x32 enable=1`
  from a real scheduled task (on which CPU does not matter), and the LFB byte
  probe translates — the modeset survives the boot→scheduler hand-over.
- Suites: `test-gpu.ps1` ×2 (QEMU default 16 MiB VRAM + `vgamem_mb=128`) green;
  `test-smp`, `test-avx`, `test-fs`, `test-shell`, `test-pci` all green after
  the kernel changes.

### M10a — bugs found in my own logic and how each was solved

1. **BAR0 is not always the framebuffer.** My first mapping code read BAR index
   0 blindly. On QEMU's vmware-svga the *IO ports* live in BAR0 and the LFB in
   BAR1 — reading BAR0 there returned an IO base and the canary would have
   written megabytes into IO/config space. Fix: `framebuffer_bar()` picks the
   first *sized MEM* BAR, the chosen index is recorded (`GpuMode::bar_idx`) and
   used for the post-modeset re-read.
2. **A failed modeset left a half-programmed mode nobody owned.** After a
   rejected mode the dispi registers held the kernel's geometry while the
   console kept drawing into the bootloader framebuffer — the device scanned
   out something nothing owned. Fix: the "mode as found" is saved before the
   first write; *every* failure after the modeset attempt restores it and
   re-reads it (`restore_and_log` / `fallback_restore`).
3. **The decision function's log named the wrong mode.** `unusable_mode()`
   printed `want` (the good mode) in its "was not accepted" wording while the
   rehearsal deliberately asks for a bogus mode — the log said
   "1920x1080x32 was not accepted" right after a 16bpp write the device *had*
   accepted. Fix: `want` (what the kernel needs) is separate from `attempted`
   (what was written); the rehearsal wording states the read-back factually.
4. **`-vga none` is unbootable on this QEMU/SeaBIOS** (no VGA BIOS → the boot
   never reaches the bootloader; serial stays empty), and `-device
   bochs-display` alone does not replace the std-VGA (QEMU keeps both; the
   class-0300 device wins the scan — bochs-display, whose dispi is MMIO-only,
   would be the ideal real fallback device but cannot be selected alone).
   Fix: the fallback is asserted via the in-boot bogus-mode rehearsal, the
   alternative the M10a spec sanctions for exactly this case.
5. **Device survey became the M10b input (measurement, not a bug):**
   virtio-vga boots end-to-end (1af4:1050, BAR0 8 MiB, dispi id 0xb0c5 — the
   modern device with a legacy-compatible face); qxl boots; vmware-svga boots
   and exposes a working dispi (and caused bug 1). Combined with the
   architecture goal, this fixed the M10b direction: **virtio-gpu becomes the
   primary backend** (capability discovery, DMA resources, command virtqueues,
   host-GPU 3D via virgl), with the M10a dispi driver kept verbatim as the
   legacy fallback behind a `DisplayBackend` trait.

### M10a success criteria

`test-gpu.ps1`: two boots (std-VGA with 128 MiB VRAM, std-VGA with QEMU's
default 16 MiB). Each must find the display device in the PCI scan
(1234:1111, class 0300, a sized MMIO framebuffer BAR), detect the bochs VBE
interface (`id=0xb0c*`), program and read back 1920x1080x32, verify the
framebuffer canary, switch the console to the dispi framebuffer, re-verify the
mode from a scheduler task, run the fallback rehearsal (bogus mode caught,
good mode restored), complete the full boot flow (fstest PASSED, interactive
shell) with no actual fallback and no unexpected exception.

## M10b stage 1 — virtio-gpu transport probe (done)

**Scope:** the first increment of the M10b redefinition (M9.8 bug 5): decode
the virtio-gpu PCI device *before* touching its queues, so every later
increment (virtqueues, GEM-lite resources, damage-rect present, virgl 3D) lands
on a verified transport. Kernel code:

- `pci.rs`: multi-function display scan (function 0 *and* 1 — virtio-vga and
  virtio-gpu-pci present as 00:02.0 + 00:02.1), `find_virtio()` by vendor 1af4
  device 1050/1052, `virtio_caps()` walking the PCI capability list through the
  *config-port* path (IF=0-safe, same discipline as the BAR sizing probe).
- `virtio.rs` (new): modern-transport probe — status machine
  (ACK|DRIVER → VERSION_1 → FEATURES_OK), the four capability regions (common /
  notify with its multiplier / ISR / device config) located and MMIO-mapped,
  the virtio-gpu control virtqueue size/alignment recorded (queue not yet
  started — stage 2), device feature bits decoded with named GPU bits
  (virgl / edid / blob / context_init).
- `gpu.rs`: the scheduler task re-reports the probe result (`[vgpu] task:`
  line) — transport state survives the boot→scheduler hand-over.

**Measured** (`test-gpu.ps1`, four boots, run twice — exit 0 both times):

| boot | device | result |
|------|--------|--------|
| 1 | std-VGA, `-global VGA.vgamem_mb=128` | M10a assertions green: 1920x1080x32 dispi modeset, canary, task re-verify |
| 2 | std-VGA, default 16 MiB | identical — the mode does not depend on extra VRAM |
| 3 | `-vga virtio` (1af4:1050) | all four capability regions decoded (`common bar2+0x1000/2048 → notify bar2+0x3000/4096 (mul 4) → isr bar2+0x1800/2048 → device bar2+0x2000/4096`), VERSION_1 negotiated, 2 queues / 1 scanout, feature bits decoded — and the full M10a set green on the modern device too |
| 4 | `-vga none -device virtio-vga-gl` + `egl-headless,gl=on` | same probe **and `virgl=1 ctx=1`** — the host-GPU 3D path exists on the emulated device (M10c/M10d foundation) |

Suites after the kernel changes: `test-gpu.ps1` ×2, `test-smp.ps1`,
`test-avx.ps1`, `test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1`
clean (image rebuilt before every run).

### M10b stage 1 — bugs found and how each was solved

1. **`-like` treats `[gpu]` as a character class.** Every marker assertion of
   the form `'*[gpu] scan: ...*'` matched nothing: in a PowerShell wildcard
   `[gpu]` means "one character from {g,p,u}", so a pattern containing its own
   brackets can never match the literal marker. Fix: assertions now use
   substring `.Contains()` on plain ASCII fragments — which also immunizes them
   against the second hazard: the kernel's UTF-8 log separators (`→`) decode as
   replacement glyphs when PowerShell reads the serial log in ANSI mode.
2. **`$fail +=` inside a function silently drops the failures.** PowerShell
   scoping: the `+=` reads the parent's array but assigns to a *local*
   variable, so the top level never sees what `Check-Boot` found — the suite
   would have "passed" with every boot broken. Fix: `$script:fail` for every
   append made from inside a function.
3. **`test-gpu.ps1` got structurally scrambled during multi-region edits.**
   Patching three regions in one pass left `Has-Line`'s closing brace missing
   and spliced the four boots *inside* `Check-Boot`'s body, nesting
   `Check-Boot` inside `Has-Line` — it parsed only by luck and ran the boots in
   the wrong scope with failures going nowhere. Fix: deleted the file, rewrote
   it whole, and required `[Parser]::ParseFile` to report zero errors before
   any run. Rule: structural edits to test scripts get an AST parse check
   before execution.
4. **Running a suite in the live shell kills the session / loses output.**
   The scripts' top-level `Exit` terminates the *interactive* shell, and
   `Write-Host` bypasses stdout redirection so `*>` captures nothing. Fix:
   suites run as a child `powershell -NoProfile -ExecutionPolicy Bypass
   -File ... *> file`, and the rewritten script emits `Write-Output` only.
5. **Boot 4 needed `-vga none` to actually expose the *gl* device.** With
   QEMU's default VGA also instantiated, the legacy/virtio instance is primary
   and the virgl-capable configuration is never the one asserted — yet bare
   `-vga none` is unbootable (M10a bug 4). `-vga none -device virtio-vga-gl`
   *is* bootable because virtio-vga provides its own scanout + BIOS path.
   Fix: `Invoke-Boot` takes the `-vga` value and the extra `-device` as
   independent parameters.

### M10b stage 1 success criteria

Four boots in `test-gpu.ps1` (the two M10a boots plus virtio-vga and
virtio-vga-gl): every M10a assertion green on all four; the four virtio
capability regions decoded on the virtio boots; VERSION_1 negotiated with
2 queues + 1 scanout reported; `virgl=1 ctx=1` on the gl boot; the transport
probe re-reported from the scheduler task; full boot flow (fstest PASSED,
interactive shell) complete with no unexpected exception — suite exits 0
twice in a row.

## M10b stage 2 — control virtqueue + GEM-lite resource + present path (done)

**Scope:** the second increment. Stage 1 decoded the transport and left the
device at `FEATURES_OK` with no queues; stage 2 gives it ONE queue (the
controlq) and uses it to draw a scanout out of ordinary guest RAM, take the
display over from dispi, and adopt the console onto it. Kernel code:

- `pci.rs`: `config_or_u16(f, offset, mask)` — a 16-bit read-modify-write of a
  config register through the aligned u32 path. Used once, for the PCI
  *command* register: the driver sets IO|MEM|**bus master** itself, because
  without bit 2 the device may not DMA the rings and every doorbell is
  silently ignored.
- `memory.rs`: `allocate_contiguous(count)` (bump-cursor only — the free list
  is single-frame and may be fragmented) + the `alloc_contiguous` wrapper.
  The scanout backing must be one unbroken physical span; scatter-gather
  multi-entry backing is stage 3.
- `virtio.rs`: the controlq engine. One 4 KiB frame holds the whole split ring
  (desc @0x000, avail @0x400, used @0x800, size ≤ 64); a second frame holds
  the command request/response pair. Every ring byte is reached through the
  bootloader's physical-memory window, so the device DMAs exactly the
  addresses programmed into the common config — no page-table edits on the
  device-DMA path. `queue_init` programs the queue, computes the doorbell
  (`notify_base + notify_off_multiplier × queue_notify_off`) and latches
  `DRIVER_OK` only after the read-backs verify. `submit` publishes two chained
  descriptors, rings the doorbell (value = queue index, since
  NOTIFICATION_DATA was not negotiated) and polls the used ring.
- `virtio.rs`: the 2D command set — `RESOURCE_CREATE_2D` →
  `RESOURCE_ATTACH_BACKING` → `SET_SCANOUT` → `TRANSFER_TO_HOST_2D` →
  `RESOURCE_FLUSH`, each a 24-byte control header plus its verified payload
  (exact lengths, padding fields included). `present_bringup` runs the whole
  sequence with a canary on the backing *before* the scanout moves, and adopts
  the console onto the virtio surface **last**. `flush_loop` re-pushes the full
  rect every 100 ms and is the GPU task's permanent job.
- `gpu.rs`: the task hook — `present_bringup(m.width, m.height)`, and on
  success the task becomes the flusher instead of exiting.

**Measured** (`test-gpu.ps1`, four boots, run twice — exit 0 both times):

| boot | device | result |
|------|--------|--------|
| 1 | std-VGA, `-global VGA.vgamem_mb=128` | stage 2 declines gracefully: `[vgpu] present: no virtio-gpu transport`, dispi console stays live (all M10a assertions green) |
| 2 | std-VGA, default 16 MiB | same graceful decline — no transport, no risk |
| 3 | `-vga virtio` (1af4:1050) | **full present path**: controlq `size=64`, `DRIVER_OK (status=0x0f)`; `resource: created 1920x1080 B8G8R8X8 id=1, backing 8100 KiB`; backing canary round-trips (`0xc6593ea0`); `scanout: set_scanout ok, transfer+flush ok`; `console adopted the virtio surface`; 100 ms flusher scheduled |
| 4 | `-vga none -device virtio-vga-gl` + `egl-headless,gl=on` | transport + controlq live (`virgl=1 ctx=1` from stage 1), and the documented virgl skip: the 2D scanout belongs to the 3D path (stage 3), so the dispi console is kept on purpose |

Suites after the kernel changes: `test-gpu.ps1` ×2, `test-smp.ps1`,
`test-avx.ps1`, `test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1`
clean.

### M10b stage 2 — bugs found and how each was solved

1. **The 2D command codes in the task brief were wrong (and the failure hid
   behind a plausible error).** The brief listed
   `TRANSFER_TO_HOST_2D=0x0102 … ATTACH_BACKING=0x0105 … FLUSH=0x0106`, but the
   2D block is a *contiguous enum* starting at `0x0100`
   (`GET_DISPLAY_INFO=0x0100`), so the real values are `CREATE_2D=0x0101,
   UNREF=0x0102, SET_SCANOUT=0x0103, FLUSH=0x0104, TRANSFER_TO_HOST_2D=0x0105,
   ATTACH_BACKING=0x0106`. The tell: with the brief's codes `ATTACH_BACKING`
   was answered `0x1205` (`ERR_INVALID_PARAMETER`) *and* QEMU's own
   guest-error log said `virtio_gpu_transfer_to_host_2d: command size incorrect
   48 vs 56` — the device had dispatched the "attach" to the *transfer*
   handler. Fix: every code is transcribed from
   `include/uapi/linux/virtio_gpu.h`, and the enum is quoted next to the
   constants so the next reader cannot re-derive it from memory.
2. **`0x1205` is `ERR_INVALID_PARAMETER`, not "bad format".** The first guess
   was that the pixel format was wrong, which sent the debugging into the
   backing size and the format enum. The authoritative enum is `0x1200 UNSPEC,
   0x1201 OUT_OF_MEMORY, 0x1202 INVALID_SCANOUT_ID, 0x1203
   INVALID_RESOURCE_ID, 0x1204 INVALID_CONTEXT_ID, 0x1205 INVALID_PARAMETER`.
   Fix: the refusal line now prints the real code, and reading
   `INVALID_PARAMETER` as "some *parameter* of this command" is what pointed
   at the dispatch (bug 1).
3. **The format enum was also off by one.** The brief said `B8G8R8X8 = 1`; the
   spec's "simple formats" block is `B8G8R8A8=1, B8G8R8X8=2, A8R8G8B8=3,
   X8R8G8B8=4`. Sending 1 asks the device for BGRA-*with alpha* — a different
   layout from the console's Bgr 32bpp surface. Fix: `GPU_FMT_B8G8R8X8 = 2`,
   with the enum spelled out in the source.
4. **Request lengths must include the trailing `padding` fields.** The device
   checks the descriptor length against the full C struct
   (`VIRTIO_GPU_FILL_CMD` → `iov_to_buf(...) != sizeof(struct)`), so a request
   that stops at the last real field is a short read and returns
   `INVALID_PARAMETER`. `RESOURCE_FLUSH` is 48 bytes (not 44) and
   `TRANSFER_TO_HOST_2D` is 56 (not 52); each length is now derived from the
   UAPI struct field by field and written as a comment beside it.
5. **A virgl-capable device routes `SET_SCANOUT` to the 3D path.** On
   `virtio-vga-gl` the virgl renderer owns the scanout and resolves resources
   in the 3D namespace; a plain 2D resource (`RESOURCE_CREATE_2D`) is
   invisible there and the device answers `virgl_cmd_set_scanout: illegal
   resource specified 1` — the display silently stays on dispi. Since the 2D
   present path is stage 2 and virgl 3D is stage 3, the driver now *detects*
   the VIRGL feature and takes the graceful path on purpose, logging why.
   `test-gpu.ps1` asserts that documented skip on boot 4 rather than the 2D
   present markers.
6. **`core::hint::pause()` is not in this `core`.** The used-ring poll used
   `core::hint::pause()`, which the pinned toolchain does not provide, and the
   build failed. Fix: `core::hint::spin_loop()`.
7. **The physical-memory window made the DMA path free of page-table work.**
   Worth recording as the *positive* measurement of this increment: the rings,
   the command buffers and the scanout backing are all reached through the
   bootloader's 1:1 RAM window, so the guest never edits a page table on the
   device-DMA path and the addresses handed to the common config are exactly
   the bytes the CPU draws into. The one asymmetry — the console is a
   `&'static mut [u8]` over that same window, so `adopt` is the hand-over, and
   it runs with interrupts disabled.

### M10b stage 2 success criteria

`test-gpu.ps1` four boots, twice, exit 0: boots 1/2 report the graceful
no-transport fallback with every M10a assertion still green; boot 3 brings the
controlq up (`DRIVER_OK`), creates and backs a 1920x1080 B8G8R8X8 resource,
canary-verifies the backing, sets the scanout with a transfer+flush, adopts
the console onto the virtio surface and schedules the 100 ms flusher; boot 4
brings the queue up and takes the documented virgl 2D-skip. QEMU's
`guest_errors` log is empty on the virtio boot (every command the device
rejected would be named there). The four regression suites stay at exit 0.

## M10b stage 3a — damage-rect present (done)

**Scope:** the increment that turns the present path from *correct* into
*cheap*. Stage 2 flushed the **whole** 8294400-byte surface every 100 ms —
a full `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` pair, ~160 MiB/s of DMA, for
a text console that changes a few hundred bytes per keystroke. Stage 3a
tracks what the console actually drew and pushes only that.

**Design.** `framebuffer.rs` grows a **dirty-rectangle accumulator**: an
inclusive bounding box widened by every pixel write. It is marked at the
single chokepoint every console draw flows through — `TextConsole::set_pixel`
— plus the two paths that write bytes directly (`clear_row`, `scroll_up`).
It lives behind one `spin::Mutex`, and `take_dirty_rect()` takes-and-clears in
a single acquisition (see bug 1 below for why that specific discipline is
required). The present side calls `take_dirty_rect()` each 100 ms tick:
`None` means nothing was drawn and **both** device commands are skipped;
`Some((x0,y0,x1,y1))` becomes the device rect (clamped to the resource so a
stale box can never address outside the backing). A rect whose flush *fails*
is re-armed (`mark_dirty_public`) so its pixels are retried instead of being
lost. The first present after `adopt` marks the whole surface dirty
explicitly — the new buffer is entirely undisplayed regardless of what the
console happened to draw.

**Why a bounding box, not a rect list:** the device takes one rect per flush,
so a single box is exactly the API's shape. A keystroke's glyph (8×8, or
16 px at scale 2) is tiny, and a burst of text between ticks collapses into
one box — the win is the box being small, not the draw count.

**Measured** (`-vga virtio`, ~40 s boot, live serial, counter built in). The
first window is boot-time console output; later windows are the settled
console:

```
[vgpu] flush: 50 ticks, 41 idle-skips, 40297 KiB pushed
             (full-rect would be 405000 KiB, saved 364702 KiB)
[vgpu] flush: 50 ticks, 49 idle-skips, 7680 KiB pushed
             (full-rect would be 405000 KiB, saved 397320 KiB)
```

**The 49/50 idle-skips are the headline:** once the console is quiet, 49 of
every 50 ticks push *nothing at all* (zero DMA), where the old full-rect
flusher pushed 8.1 MiB on every one of them. When work does happen it is
~7680 KiB per 50-tick window vs 405000 KiB full-rect — a **~98 % reduction**
in active windows. The counters are printed so the saving is auditable rather
than estimated (the earlier "~160 MiB/s" was a back-of-envelope figure).
These are the numbers *after* the race fix (bug 1); the pre-fix CAS version
measured worse (41/50 skips) precisely because it was losing rects.

Suites: `test-gpu.ps1` ×2, `test-smp.ps1`, `test-avx.ps1`, `test-fs.ps1`,
`test-shell.ps1` — all exit 0. `build.ps1` clean. QEMU `guest_errors` empty on
the virtio boot.

## M10b stage 3b — input→present latency probe (done)

**Scope:** the increment that makes latency a *measured* property. The vision
is a low-latency, performance-driven OS; you cannot optimize what you do not
measure, and before this the project had zero numbers for the one metric a game
feels. This adds the probe itself (a software-cursor cursor move, timed from
the input IRQ to the device acknowledging the present). It is deliberately
*measurement before optimization*: the number it reports (~64 ms average) is
the current software-cursor baseline that every later optimization — hardware
cursor, frame pacing, tearing — will be judged against.

**How it works.** `framebuffer::move_cursor` (called from the mouse IRQ, IF=0)
stamps `time::now_ns()` into a pending slot before any early return, so the
stamp reflects when the IRQ fired, not whether the move changed the sprite. The
damage-rect flusher, on a successful transfer+flush, takes that pending stamp,
reads the clock again *after* the device has acknowledged both commands, and
records the delta into a small power-of-two histogram. A report line prints
`n / avg / min / max` in microseconds once per 5 s window, beside the existing
DMA-saving line so bandwidth and latency are read together.

**Why it is a histogram, not an average:** an average hides the tail that makes
a game feel unresponsive; a min/max pair without a distribution can hide a
regression in the bulk. The buckets are powers of two in microseconds.

**Tested as an artifact** (`test-latency.ps1`, new): boots `-vga virtio` (the
only backend that runs the flusher), waits for the present path, injects real
PS/2 mouse packets over the QMP monitor in an alternating pattern (so the
cursor sweeps rather than pinning at a screen edge), and asserts that
(i) the virtio present path is live, (ii) the cursor actually moved
(`[mouse] pos`), and (iii) a latency window closed **with samples** (`n>0`,
not the idle line). A latency metric that never fires is worse than none, so
the probe is under test, not just printed.

**Measured** (`test-latency.ps1` run, live serial):

```
[vgpu] flush: 50 ticks, 42 idle-skips, 40363 KiB pushed
             (full-rect would be 405000 KiB, saved 364636 KiB)
[vgpu] latency: input->present n=2 avg=64442 us min=51328 us max=77557 us
```

**~64 ms average input→present on the software-cursor path.** That is a
terrible number for a game and it is exactly the point: it is the honest
current baseline, and it says the dominant cost is the 100 ms flush cadence
plus the software cursor redrawing two full cursor rows per move. A hardware
cursor (queue 1) would collapse most of it, because a mouse move becomes one
32-byte command with no framebuffer damage at all.

Suites: `test-gpu.ps1` ×2, `test-latency.ps1`, `test-smp.ps1`, `test-avx.ps1`,
`test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 3c — hardware cursor (queue 1): the input-latency optimization (done)

**Scope:** the optimization the 3b probe was built to justify. 3b measured
~64 ms input→present for the *software* cursor; this makes the pointer a
device-side object so the pointer stops waiting on the 100 ms present cadence.

**Why this was the right next step (and not scatter-gather or a surface API).**
The software cursor repaints two full 16x24 (32x48 scaled) rectangles on every
mouse packet. That dirties the framebuffer, so the damage-rect flusher has to
transfer those rectangles and the pointer pixel only lands on the next 100 ms
tick — the cursor carries the *same* latency as any other drawing, which is
wrong, because the cursor is what the player is looking at while they aim. A
hardware cursor removes the pointer from the present path entirely.

**What it does.**
- Brings up **queue 1** (the cursor virtqueue) in its own pair of frames, so a
  cursor move can never contend with a control-queue command in flight.
- Uploads the arrow once as a 2D resource (`B8G8R8A8_UNORM` — the cursor needs
  *alpha* for its transparent pixels, unlike the scanout's `B8G8R8X8`), then
  sends one `UPDATE_CURSOR`.
- Every subsequent move is a single 56-byte `MOVE_CURSOR` (`virtio_gpu_update_cursor`
  struct: hdr + pos + resource_id + hot_x + hot_y + padding) — **zero framebuffer
  traffic**, so the flusher has nothing to push for a mouse move.
- `framebuffer::move_cursor` forwards to the device when the hardware cursor is
  live and skips the sprite repaint; a device with no cursor queue (or any
  refused step) keeps the software sprite.

**The measurement, and a probe that had to change.** With the hardware cursor
live, a mouse move produces *no framebuffer damage*, so the 3b probe (which
times input→framebuffer-flush) no longer sees cursor moves at all — the
pointer was being measured by the wrong instrument. The probe now also times
**input→MOVE_CURSOR-acknowledged**, which is the number the player actually
feels. This was not a cosmetic split: the two numbers are ~440× apart.

**Measured** (`test-latency.ps1`, `-vga virtio`, hardware cursor live):

```
[vgpu] cursor: hardware cursor live on queue 1 (16x24 B8G8R8A8, moved via MOVE_CURSOR - zero framebuffer damage)
[vgpu] latency: input->present     n=2  avg=57647 us min=1295 us max=114000 us
[vgpu] latency: input->MOVE_CURSOR n=30 avg=130 us  min=72 us  max=960 us
```

**Pointer latency: 130 µs (hardware cursor) vs ~57 ms (software cursor) — a
~440× improvement.** This is the headline number for a latency-driven OS: the
pointer is now updated in well under a millisecond instead of being gated by
the 100 ms present loop. (The `input->present` figure remains the honest
latency for *framebuffer* drawing, which is still present-cadence-bound — that
is what frame pacing will attack next.)

**Design note — the flusher is scheduled before the cursor is enabled.** The
hardware cursor is a best-effort extra: if its bring-up fails or stalls, the
console keeps its already-live flusher and the software cursor, so an optional
latency feature can never block the present path. (An early version called
`hardware_cursor_enable()` *before* the flusher-started log and a bring-up
stall took the whole console down with it — see the bug log.)

Suites: `test-latency.ps1` (now also asserts the hardware cursor is live or the
fallback is documented, and that the MOVE_CURSOR probe closed), `test-gpu.ps1`
×2, `test-smp.ps1`, `test-avx.ps1`, `test-fs.ps1`, `test-shell.ps1` — all exit
0. `build.ps1` clean. QEMU `guest_errors` empty on the virtio boot.

### M10b stage 3c — bug found and solved

- **An optional feature must never be able to block the feature it depends on.**
  The first version called `hardware_cursor_enable()` *before* the
  `flusher scheduled` log; a stall in the cursor bring-up then meant the console
  never got its flusher (the whole present path was gated behind a hardware
  cursor that, by design, is allowed to fail). Fix: schedule the flusher and
  set `PRESENT` first, then attempt the cursor as a best-effort extra whose
  failure keeps the software sprite. The invariant is now "the present path is
  live before anything optional runs on top of it."

## M10b stage 4 — adaptive frame pacing (done, with an honest caveat)

**Scope:** stop treating the present period as a fixed 100 ms. A fixed period
is a *latency* decision disguised as a DMA decision — a change drawn just after
a tick waits a whole period to appear. The flusher now runs an **adaptive
cadence**: a 1 ms *latency quantum* while the console is changing, backing off
to a 16 ms idle sleep after enough quiet quanta, so response latency and
CPU/DMA cost are decoupled instead of traded. A damage *generation* counter
(`framebuffer::damage_generation()`, bumped on every draw, read without
consuming the box) is the change signal, so an idle quantum does a single
relaxed load and no work.

**New instrumentation.** The flusher now reports two more metrics per window:
- `response: damage->present` — how long a drawn change waited to be pushed
  (the direct, input-independent number the cadence targets);
- `pacing: … interval` — the cadence between consecutive damage-driven
  presents (min/avg/max), so jitter is visible.

**Honest result / caveat.** The adaptive cadence and its instrumentation are in
and correct, but the boot-time `test-latency` workload is a *console burst*
(autoexec runs commands back to back), so every damage→present sample is drawn
during continuous output — the mark is stamped early and the flusher presents an
accumulated box later. The measured `response min ≈ 43 ms` and
`pacing min ≈ 93 ms` therefore reflect how long the *console took to draw a
burst*, not the flusher's own wait, and this harness cannot cleanly A/B the
cadence against the old fixed-100 ms baseline. I am **not** claiming a measured
latency win from the pacing change itself; what is proven is that the flusher no
longer *structurally* injects up to 100 ms of wait (it now waits ≤ quantum while
active / ≤ idle-sleep when quiet), and that the response and interval are now
*measured* going forward. A clean A/B needs a workload that draws on demand (one
glyph, wait, one glyph) rather than a burst — that is the natural companion to
this increment and the honest next validation.

The 1 ms quantum is the LAPIC tick (`sleep_kernel` rounds to it), so while
active the flusher wakes every tick; the 16 ms idle sleep bounds the
worst-case wait for a change that arrives during back-off. The earlier 50 ms
idle sleep was measured costing ~46 ms min response on a late first keystroke
and was reduced for exactly that reason.

Suites: `test-latency.ps1` (now asserts the adaptive cadence + response/pacing
lines), `test-gpu.ps1` ×2, `test-smp.ps1`, `test-avx.ps1`, `test-fs.ps1`,
`test-shell.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 5 — where the latency actually is (done: a finding, not a win)

**Scope:** stage 4 shipped the adaptive cadence but could not prove a win,
because the boot-time workload is a console *burst*. Stage 5 adds the
workload that was missing (on-demand, isolated single-glyph draws), and then
splits the response measurement so the number is **attributable**.

**The workload.** `test-latency.ps1` gained a phase 2: quiesce the console for
6 s (so the flusher's idle backoff engages), then type five characters, well
spaced (700 ms apart). Each keystroke is echoed by the ring-3 shell, producing
*isolated* framebuffer damage — one glyph, one present — instead of a boot
burst. This is the A/B workload stage 4 lacked.

**The measurement that mattered: attribution.** A total `damage->present`
number is ambiguous — a big value could be the pacing quantum (fixable by
sleeping less) or the scheduler (not fixable that way). Stage 5 stamps the
moment the flusher returns from `sleep` and reports
`response: wake/schedule avg/max` *separately* from the total. The two together
say exactly where the time goes.

**Measured** (isolated single-glyph presents, `test-latency.ps1`):

```
[vgpu] response: damage->present n=1 avg=239479 us ...
[vgpu] response: wake/schedule  n=1 avg=170333 us ...
```

**The finding: ~70 % of framebuffer-present latency is SCHEDULER wake latency
(≈170 ms of ≈239 ms), not the pacing quantum.** The 1 ms quantum and 16 ms
idle backoff are working as designed — the flusher is not sleeping too long.
It simply is not being *run* promptly once its sleep ends, under this
TCG/scheduler load. This is a genuinely useful negative result: it redirects
the next optimization from "frame pacing" (stage 4's guess) to **task
wake-up latency** — e.g. an event-driven present (wake the flusher directly on
damage instead of relying on a timer + preemption), or scheduler
priority/slice changes so the present path is not queued behind CPU-bound
tasks. Pacing is the *right* long-term direction, but the quantum is not the
lever, and the 239 ms is not a pacing bug.

I am still **not** claiming a measured latency win from stage 4's cadence: the
isolated-workload measurement shows the cadence is not the dominant term, and
the honest statement is "the cadence removes the structural 100 ms wait, but
scheduler wake latency now dominates." That is what the next increment should
attack.

Suites: `test-latency.ps1` (now runs the on-demand phase + asserts the
response/pacing/wake lines), `test-gpu.ps1` ×2, `test-smp.ps1`,
`test-avx.ps1`, `test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 6 — event-driven present (done: mechanism works, partial win)

**Scope:** stage 5 showed the flusher's response was dominated by *scheduling*
wait (~170 ms of ~239 ms), not the pacing quantum. Stage 6 makes the present
path event-driven: the damage path wakes the flusher directly on a drawn
change, instead of relying on the flusher's poll timer to notice.

**What it does.**
- `scheduler::wake_task_now(id)` — promote a `Sleeping` task to `Ready` now
  (idempotent, safe from IRQ or task context), and send the reschedule IPI
  (`smp::kick_others()`) so the newly-Ready task is seen by the other CPUs.
  This mirrors what `spawn`/`exit`/`kill`/`set_priority` already do.
- `framebuffer::mark_dirty_rect` calls `virtio::wake_present_on_damage()` on the
  **empty → non-empty** damage transition only (per-pixel it would take the
  scheduler lock per pixel; a burst keeps the box non-empty and wakes once).
- The flusher publishes its task id and promotes itself to **RT** priority, so
  a damage wake preempts CPU-bound Normal work.

**Measured** (isolated single-glyph presents, `test-latency.ps1`):
- Before (stage 5): `damage->present ≈ 239 ms`, `wake/schedule ≈ 170 ms`.
- After (event-driven + RT): `damage->present ≈ 134 ms`, `wake/schedule ≈ 105 ms`,
  and `event-driven wakeups=6` (damage woke the flusher directly, matching the
  5 typed glyphs — the mechanism is demonstrably firing).

**So: a real ~44 % improvement in single-glyph present latency (239 → 134 ms),
attributable to event-driven wake + RT priority.** That is a measured win.

**Honest caveat / where the rest of the latency is.** The residual ~105 ms
`wake/schedule` did NOT drop to ~0 even with the IPI kick and RT priority, so
the remaining wait is not the present path's timer. The `sendkey` workload
drives the echo through the **ring-3 shell**, whose own task scheduling sits
*upstream* of the damage mark; a large part of what this harness measures is
how long the ring-3 shell takes to echo, not how long the flusher takes to
present. Separating those needs a kernel-local draw that bypasses the shell
(single glyph drawn from the flusher's own task), which this harness does not
yet have. The next clean step is a **kernel-side single-glyph present test** to
attribute present latency without the shell in the loop.

Suites: `test-latency.ps1`, `test-gpu.ps1` ×2, `test-smp.ps1`, `test-avx.ps1`,
`test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 7 — kernel-local present probe: the pure number (done)

**Scope:** finish the attribution stage 6 started. The `sendkey` workload
measures a draw→present round trip that goes *through the ring-3 shell*, whose
scheduling sits upstream of the damage mark and inflates the number (~130 ms).
The present path itself was never measured in isolation. Stage 7 adds a
**kernel-local probe**: a task that stamps an instant, writes one 4×4 marker
rect straight into the framebuffer surface (bypassing the ring-3 console text
path), and lets the flusher present it — so the *only* thing between the write
and the device is the present path.

**Design points.**
- `framebuffer::draw_probe_marker()` writes the marker at the bottom-left
  (background slate — a visual no-op) and marks exactly that rect dirty, so it
  never disturbs the visible screen or the boot-flow assertions.
- The probe waits ~8 s after present before its first draw: probing during the
  boot/autoexec burst competes with the boot flow and measurably lengthens it
  (caught and fixed during this increment — the first version probed at 2 s and
  the boot-window assertions started failing).
- `draw_probe_marker` takes the `FB` spin lock with **interrupts disabled**,
  the same discipline every other console draw uses: the mouse IRQ handler also
  takes `FB`, so a lock taken with interrupts ON could be preempted and spin
  forever.

**Measured** (`test-latency.ps1`, kernel-local, no shell/input in the loop),
across many runs: **216–486 µs, typically ~340–430 µs.**

```
[vgpu] probe: kernel-local draw->present n=1 avg=399 us min=399 us max=399 us
[vgpu] probe: kernel-local draw->present n=1 avg=433 us min=433 us max=433 us
[vgpu] probe: kernel-local draw->present n=1 avg=339 us min=339 us max=339 us
```

**Conclusion (the whole point of stages 4–7).** The virtio-gpu present path —
damage rect → event-driven wake → RT-priority transfer + flush → device
acknowledged — is **sub-millisecond (~0.4 ms)** end to end. The ~130 ms that the
`sendkey` workload reports is **not** the display driver: it is ring-3 shell
task scheduling, upstream of the draw. So the GPU/present stack is not the
latency bottleneck for input-driven UI; a future input/game path that draws
from a kernel task (or an RT-priority process) will see the sub-ms number.

Suites: `test-latency.ps1` (now asserts the kernel-local probe), `test-gpu.ps1`
×2, `test-smp.ps1`, `test-avx.ps1`, `test-fs.ps1`, `test-shell.ps1` — all exit
0. `build.ps1` clean.

## M10b stage 8 — kernel frame clock: sustained frame pacing (done: a finding)

**Scope:** stage 7 proved a *single* present is sub-millisecond. The next
gaming question is different and harder: can the present path **sustain** a
frame rate, and with what jitter? Stage 8 adds a kernel **frame clock**: a task
that redraws a small animated strip (`framebuffer::draw_anim_frame` — a box
travelling along a bottom-screen track) once per tick, kernel-locally (no
ring-3 shell), and reports the achieved cadence. The flusher's existing
`pacing:` counters now also print achieved **fps** (from the mean frame
interval) alongside the interval min/avg/max jitter.

**What it measures and found.** The kernel-local present probe (stage 7) stays
**sub-millisecond (~217–324 µs) even under the frame-clock load** — the
transfer+flush path is never the bottleneck. But the *sustained* frame rate the
flusher delivers is only **~5–7 fps** with wide inter-frame intervals
(150–950 ms), even when the frame clock produces a frame every 50 ms (20 fps).
Each individual present is fast; they just do not happen every frame, because
the RT flusher is not *scheduled* on every frame under this 2-cpu TCG VM — the
damage coalesces in the accumulator between presents.

**The honest conclusion.** Frame *pacing* (how often presents land) is limited
here by scheduler/CPU availability, not by the virtio present path (which is
sub-ms per frame). On more CPU cores (or with the flusher pinned/dedicated) the
cadence should track the producer; on this configuration it does not. So the
60/120+ fps target for a real game is gated by giving the present path a
dedicated CPU / better scheduling, not by the driver. This is exactly the kind
of finding the frame clock was built to surface, and it redirects the next step
toward **scheduling / CPU-affinity for the present task**, not more driver work.

(The frame clock targets 20 fps, not 60, on purpose: a 16 ms producer on this
VM overloads the scheduler so badly the "cadence" becomes a collapse rather
than a measurement. 20 fps is sustained end-to-end, so the report reflects real
pacing + jitter.)

Suites: `test-latency.ps1` (now asserts the fps/jitter pacing line and the
kernel-local probe), `test-gpu.ps1` ×2, `test-smp.ps1`, `test-avx.ps1`,
`test-fs.ps1`, `test-shell.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 9 — scheduler/SMP investigation (done: two unsafe ideas rejected, honest finding)

**Scope:** stage 8's frame clock showed sustained pacing capped at ~5–7 fps
with 150–950 ms inter-present gaps while the *present path itself* is sub-ms.
The hypothesis was CPU contention on the flusher's CPU. This increment
investigated the scheduler/SMP side and the goal of a 1000+ FPS game OS.

**Two candidate fixes were built, measured, and then REJECTED as unsafe** (both
are recorded here because "we tried it and it broke" is the useful result):

1. **Self-reschedule IPI on a same-CPU wake.** `wake_task_now` marks the flusher
   Ready, but `smp::kick_others` deliberately skips the calling CPU, so the
   flusher's own CPU only re-schedules on its next 1 ms LAPIC tick. Fixing that
   by sending THIS cpu its own reschedule IPI crashed the kernel with a
   `#GP`/null-context jump: the IPI lands in arbitrary interrupt state and can
   preempt a task already inside the scheduler lock or mid-context switch. A
   present-latency micro-optimization is not worth an unstable kernel. **Backed
   out.** (A *safe* same-CPU wake — e.g. a resched flag the timer/IPI path
   honours at a safe point — is the right shape, but is a scheduler change, not
   a present change.)
2. **Pinning the frame clock to CPU 1** (`spawn_on_cpu(frame_clock, .., 1)`).
   This *did* lift measured pacing ~5 → ~10 fps, but crashed the non-virgl virtio
   boot with a null-context jump: `spawn_on_cpu` only sets `owner_cpu` and
   assumes the target AP is fully online with its per-CPU context set up (not
   true at present-bringup time), and `plan_switch` is free to steal an
   `owner_cpu=1` task onto CPU-0 (the M9.8-(d)/(e) affinity filter was removed),
   so the task's context can run on the wrong core. **Backed out.**

**The honest finding.** The single-CPU case (flusher + frame clock + shell +
ticker all on the BSP) is the pacing bottleneck, and the *correct* fix is real
cross-core CPU affinity: a task must actually stay on its owner CPU, and the
present flusher should own a core. The current scheduler explicitly removed
the affinity filter (M9.8-d/e) and is therefore free to move any task to any
CPU — which is correct for general throughput but wrong for a latency-critical
present task. **Making affinity opt-in (a per-task "pin me" bit) is the real
stage-10 fix**, and it belongs to the scheduler track, not the GPU one.

**Reality check on 1000+ FPS, honestly.** The per-present cost is ~217–324 µs
and the cheapest device round-trip ~97 µs, so a 1 ms frame budget *fits* the
present path with room to spare — the ceiling is not the driver. But this test
environment is QEMU **TCG** (software CPU emulation, no KVM) on a 2-vCPU guest;
the 1 ms LAPIC tick and task-switch cost are themselves emulated, and a literal
1000 FPS sustained *here* is unlikely even with perfect affinity. The right
target is: present path proven sub-ms (done, stage 7), scheduling fixed so
pacing tracks the producer on a real CPU budget, then validated on KVM/hardware
where the numbers mean something. I will not claim a 1000 FPS number I cannot
reproduce.

**Net code change from this increment:** none to the scheduler/SMP behaviour —
both experiments are reverted. The only lasting edits are the stage-9 comments
documenting why the two unsafe fixes were rejected, so the next person does not
retry them blind. The frame clock (stage 8) remains the measurement tool that
exposed the problem.

Suites: `test-gpu.ps1` ×2, `test-smp.ps1`, `test-avx.ps1`, `test-fs.ps1`,
`test-shell.ps1`, `test-latency.ps1` — all exit 0 (verified in the reverted
state). `build.ps1` clean.

## M10b P1 — task-context memory safety (step B: CPU affinity restored) (done)

**Scope.** The P1 finding in `docs/ARCHITECTURE-ANALYSIS.md`: cross-CPU task
execution was not memory-safe. A task's context is *split* across a heap
`sp_slot`, its kernel stack (re-installed into the running CPU's `TSS.RSP0`),
and its FPU save area — and `plan_switch` could migrate any `Ready` task to
any CPU (`owner_cpu` was a *preference*, rewritten to `me` on every switch).
Nothing owned that context, so two CPUs could touch the same bytes. Proof it
is a real memory-corruption class: stage 10 added one **inert** `u8` field to
`Task` (unused at runtime) and flipped a passing boot into a jump-to-heap crash
(`RIP` in `0x4444_4444_xxxx`). An inert field cannot break logic; only a
shifted layout/timing can expose an existing race.

**What changed** (`scheduler.rs`, two spots):
1. `plan_switch` Ready-scan: a task is only considered by the CPU that owns it
   (`if owner_cpu != me { continue }`) — restores the pre-M9.8-(d)/(e)
   affinity filter.
2. Switch-into: **stop rewriting `owner_cpu = me`**, so it stays the task's
   authoritative CPU for its lifetime.

With this, a task's entire context is touched by exactly one CPU, ever.

**Consequence, measured.** Migration is gone, so anything that relied on
work-stealing had to become explicit or it silently serialized:
- `bench.rs` workers are now spawned one-per-CPU (worker `i` → CPU `i`). It
  previously depended on the claim-then-migrate pattern. Result:
  `cpus=[3,1,2,0] distinct=4, speedup x4.6–x4.8` (**up from x2.65** under
  migration — the affinity removes migration overhead, so this is a perf win,
  not just a safety one).
- `main.rs`: `fpu-test B` is spawned on CPU 1 (A stays on the BSP). The
  two-task SSE/FXSAVE regression is *designed* to run concurrently on separate
  cores; serialized, it blew `test-avx`'s 40 s window (verified: both tasks
  pass with a 75 s window once the load is distributed).

**Rejected approach (and why it is not shipped).** Holding `SCHED_LOCK` across
`context_switch` to close the plan/switch TOCTOU. It is unsound: the switch
transfers control while the lock guard is still live on the outgoing frame, so
the incoming task spins forever on `SCHED_LOCK` with interrupts disabled. Caught
during implementation and backed out rather than committed.

**Verified** (each run exit 0, and the scheduler-sensitive ones re-run per the
test-hygiene rules): `test-gpu.ps1` ×3, `test-smp.ps1` ×2, `test-avx.ps1` ×2
(FXSAVE + XSAVE/AVX 200+ rounds), `test-para.ps1` ×2, `test-fs.ps1`,
`test-shell.ps1`, `test-latency.ps1`. `build.ps1` clean.

**Honest caveat — this is a safety-first tradeoff, not the final design.** A task
is now pinned to its birth CPU: placement must be explicit, and dynamic
work-distribution (stealing) is gone. That is the correct trade *now* (a memory
corruption is not), but the destination is the per-CPU owned-context rewrite
(step A), where a task's context becomes a single owned allocation and safe
migration/work-stealing is possible again. Step B is the safe floor that makes
that rewrite possible without racing.

## M10b repo hygiene — untrack build artifacts (done)

~7 MB of regenerated build/debug output was committed (three 2.4 MB raw LLVM
IR `kernel*.ll` files, 14 build/error logs, a stray `build.pid`). `.gitignore`
now covers `*.ll`, `*.err`, `*.out`, `*.pid`, `*.txt`; all 17 were
`git rm --cached` (kept on disk). The `*.txt` glob is safe here: every tracked
`.txt` was a build log (verified) and all real documentation is `docs/*.md`
(verified unaffected). Standalone commit `ba70b42`.

## M10b P3 — disk-input-driven panic hardening (done)

**Scope.** `panic = abort` is set in both Cargo profiles, so any panic on a
path driven by disk bytes takes the whole kernel down — a corrupt boot sector
had the same blast radius as a scheduler bug. Audited the 48 `.unwrap()` /
`expect` / `panic!` sites in `ext2.rs` / `block.rs` / `fat.rs` / `virtio.rs` and
hardened the ones bad media can actually reach.

**The real find: `block.rs` `scan_gpt`.** It read `entry_size` and
`entry_count` straight off the GPT header and used them to index the partition
-entry array, unvalidated:

- `entry_size == 0` → `table[0..0]`, and the loop then indexed past it;
- `entry_size < 128` → the UTF-16 name read at `e[56..128]` sliced out of range;
- `sectors as u8` → a >255-sector entry array was **silently truncated** by the
  `u8` cast, under-read, then indexed past the bytes it actually had;
- `entry_count * entry_size` could overflow the allocation;
- `last - first + 1` underflowed on a corrupt entry (`first > last`).

All now go through `validate_gpt_geometry()` (bounds) plus a saturating,
**chunked** read (a GPT array larger than one 255-sector transfer is now read
in pieces rather than truncated), and a rejected header logs and degrades to
"no GPT partitions" instead of aborting.

**Audited and deliberately left as-is** (genuine invariants, not disk-driven):
- the `ext2` block-iterator slices (`t[k*4..k*4+4]` …) are bounded by the
  compile-time `PTRS`/`BLOCK` constants into fixed `[u8; BLOCK]` arrays, so no
  disk value can take them out of range;
- `fat.rs`'s `spc` divide is already validated (`spc == 0` → `BadFs`) before the
  divide, and the `si * 32` slices are bounded by a modulo of the same buffer's
  size.

**Verification** — `block::gpt_selftest()` drives the *real* validation with
hostile geometries on every boot: `(0,128)`, `(32,128)`, `(MAX,128)`,
`(128,MAX)` must be rejected and `(128,0)` accepted; a regression aborts the
kernel and is caught immediately. Asserted by the new `test-fs-corrupt.ps1`.

**Why a self-test rather than a crafted corrupt image** (the obvious approach,
which I tried first and abandoned): the real trigger for `scan_gpt` is MBR
partition 0's type byte being `0xEE`, but on this image that *same* byte is the
bootloader's own boot partition (type `0x20`, LBA 1). Setting it to `0xEE` to
reach the GPT path stops the kernel from booting at all — verified: zero
serial output. Exercising the validation directly is deterministic and needs
no crafted media.

**Gotcha worth remembering** (hit and fixed): the self-test's PASS line must not
contain the word "panic" — several suites assert on `PANIC|EXCEPTION` in the
serial log, so naming it there made `test-fs` report a *phantom* kernel panic.

Suites: `test-fs-corrupt.ps1` (new), `test-fs.ps1`, `test-gpu.ps1` ×2,
`test-smp.ps1`, `test-avx.ps1`, `test-shell.ps1`, `test-latency.ps1`,
`test-para.ps1` — all exit 0. `build.ps1` clean.

## M10b stage 3b — decisions and deferrals

- **Deferred scatter-gather backing.** 8.1 MiB contiguous always fits the
  512 MiB bump, and scatter-gather buys no latency — it is a *scaling* concern
  for higher resolutions, not a gaming-latency one. It drops below the hardware
  cursor in priority under the latency-first vision.
- **The surface/blit API is the right long-term direction, but not yet.** For a
  low-latency OS the present path (frame pacing, tearing) matters more than a
  general drawing API, and the latency probe now exists to measure it. The next
  build step after this is the **hardware cursor** (queue 1), not a generic
  surface API.

### M10b stage 3a — bugs found and solved (post-3b fix, kept here for the record)

1. **The dirty-rect accumulator could LOSE a rect (the one failure a present
   path must not have).** The first version made the four box edges
   lock-free, with a `compare_exchange` on `DIRTY_X0` as a claimed ownership
   token and three plain stores for the other edges. That is wrong in two
   distinct ways, both found by re-reading the code rather than by a failing
   test:
   - A marker that widened *only* the other three edges (i.e. its `x0` was
     already the box minimum) would CAS `DIRTY_X0` to **the same value**, so
     the CAS did not actually exclude a concurrent taker — the "ownership"
     was a fiction and a rect could still be lost.
   - A taker that read the box with four independent operations could observe
     a half-updated box, and a marker landing between its read and its clear
     had its damage discarded.

   Lost damage means pixels that are never presented and never redrawn — stale
   garbage on screen, the exact failure the present path exists to prevent.
   Fix: a single `spin::Mutex<Option<(x0,y0,x1,y1)>>`; the mark widens under
   the lock and `take_dirty_rect()` takes-and-clears in ONE acquisition, so a
   concurrent marker either merges into the rect we are returning (and is
   included in this flush) or lands after the clear (and waits for the next
   tick). No window exists. The lock is uncontended and never blocks inside
   (pure arithmetic, microseconds), and every console write already runs under
   the `CONSOLE` lock, so the added cost is a few instructions.
2. **Concurrent suites kill each other's QEMU and read as kernel failures.**
   `test-*.ps1` all call `Get-Process qemu-system-x86_64 | Stop-Process -Force`
   in `Invoke-Boot`, so two suites run even slightly overlapped produce
   `QEMU self-exited` on boots that were merely cut short (a boot truncated
   mid-way misses later markers like `fpu-test: task A PASSED`, which then
   reports as an SMP regression even though the SMP path is fine — the
   affected boot in one run reached `bring-up complete` and produced a normal
   432-line log, just not the tail). Fix: run the suites strictly one at a
   time, each as its own child `powershell`, and treat a `QEMU self-exited` or
   a missing tail-marker with a re-run before believing it.
3. **A busy host makes a 30 s boot window flaky.** `Invoke-Boot` samples the
   serial log after a fixed `Start-Sleep` and then asserts `fstest: PASSED` +
   `autoexec done`; on a loaded machine the boot can miss the window and the
   suite reports a boot-flow failure that has nothing to do with the change
   under test. Observed once on boot 1 (std-VGA, no virtio involved); the
   immediate re-run was clean. Fix: same discipline as (2) — re-run before
   believing a boot-flow-only failure, and never run two boots concurrently.

## M9.5 — Graphical desktop userspace (DEFERRED)

**Why it exists (inserted between M9 and M10):** M13's full GUI was too big a
leap — a desktop needs kernel primitives that don't exist yet. M9.5 delivers
exactly those, so M13 becomes (mostly) userspace work on a stable base.

Scope:

1. **Per-process address spaces.** Every ring-3 process gets its own page
   tables and the scheduler switches CR3 with the task. Kernel mappings stay
   shared (higher half), user mappings isolated. Until then all ring-3 code
   shares the kernel's single address space (the M4 state).
2. **Window server + graphics syscalls.** A ring-3 *window server* owns the
   screen: the kernel maps the framebuffer into it, apps draw into off-screen
   surfaces (`surface_create` / `blit` / `present` syscalls) and the server
   composites wallpaper → windows → cursor with double buffering. This also
   kills the M3 cursor ghost-trail issue for good. (The wallpaper file loads
   via VFS from M6 onward; before that a baked-in image is the fallback.)
3. **IPC + input delivery.** Keyboard/mouse events collected by kernel IRQ
   handlers become readable in ring 3 (event queues behind syscalls), and
   processes can message each other. `spawn` / `waitpid` / `kill` groundwork
   lands here too.

**Success test:** a ring-3 window server draws a wallpaper, a draggable
window and a smooth cursor — no graphics code in the kernel beyond the
syscall API. Verified visually via `cargo run -- bios-gui` (headless serial
can't see pixels), plus serial markers for the server's lifecycle.

---

## Gaming / performance / latency track

- **M3** — scheduler designed with low latency in mind: per-task priorities,
  interruptible syscalls, minimal lock contention in the hot path.
- **M9.8** — SMP: multi-core scheduling with task migration + work stealing,
  verified by a parallelism benchmark (`test-para.ps1`: 4 CPU-bound workers,
  x2.9-5.1 wall-clock speedup at `-smp 4` on WHPX; on TCG the vCPUs time-share
  one emulation thread so only near-neutral overhead is assertable).
- **M10** — GPU driver system, built in stages ("basic first, then scale up to
  decent" — NOT a placeholder anymore):
  - **M10a (basic): ✅ done.** PCI GPU scan (display class, sized MEM BARs,
    display-function registry lookup) + kernel-controlled modesetting through
    QEMU's bochs VBE/dispi interface (`kernel/src/gpu.rs`): kernel-chosen mode
    **1920x1080x32** programmed with hardware read-back verification, LFB BAR
    mapped at a fixed kernel address (`0x400_0000_0000`, translate-guarded),
    canary-verified mapping, console handed over (`framebuffer::adopt`),
    save/restore of the firmware mode on every failure path, an in-boot
    fallback rehearsal (deliberately bogus mode write → detected → restored),
    and a scheduler-task re-verification of the installed mode. Full details,
    measurements and the bug log in the M10a section below. `test-gpu.ps1`.
  - **M10b (usable):** render-surface API (`surface_create` / `blit` /
    `present` syscalls), compositor stub, 2D blits/fills, dirty-rect present —
    the API shape the whole GUI track programs against. **Direction decision
    (post-M10a):** the *primary* display backend becomes **virtio-gpu** — QEMU's
    modern paravirtual GPU (capability discovery, DMA buffer resources, command
    virtqueues, per-scanout planes, host-GPU 3D via virgl contexts) — the only
    device in this VM with a non-legacy hardware model. The M10a dispi driver
    is kept verbatim as the legacy fallback backend behind a `DisplayBackend`
    trait; bootloader framebuffer stays the last resort. This is what makes the
    "modern driver architecture" goal (buffer objects, rings, fences, flips)
    reachable at all.
  - **M10c (decent):** the acceleration path — hardware-accelerated fill/blit
    where QEMU's devices expose it (virtio-gpu / bochs), presentation pacing,
    multi-surface composition — scaling the basic driver into a decent one.
  - M15 remains the endgame (full accelerated graphics API), but M10's target
    is a *working* driver system, not stubs.
- **M15** — *full* GPU driver with an accelerated, low-overhead graphics API
  (QEMU's VM GPU; Vulkan-lite surface) — the last milestone on purpose: it
  needs the entire kernel (scheduler, memory, GUI, syscalls) to be useful.

Latency posture throughout: direct IRQ handlers → serial/eoi as fast as
possible, kernel code kept allocation-light in hot paths, and the scheduler
defaulting to responsiveness over throughput.

---

## Windows `.exe` / `.dll` roadmap (foundation → run)

1. **M12 — PE foundation.** Parse PE/COFF headers, sections, and relocations;
   build a DLL import-resolution table. *Goal: demonstrate parsing/linking of
   real `.exe`/`.dll` files copied into the FAT32 image — no execution yet.*
2. **M14 — run them.** Implement the Windows syscall surface (`_open/_read/…`,
   Win32 API stubs) over our syscall layer + VFS, a Windows-compatible loader
   mapping PE images into ring 3, and a compat layer for DLLs. Target: a simple
   console `.exe` (e.g. hand-built hello-world PE) runs unmodified inside
   OnyxOS. Full WINE-style binary compat is *explicitly out of scope*.

---

## Testing strategy (unchanged, QEMU-only)

- `cargo build` → `bios.img` (and `uefi.img` from M9) via `build.rs` +
  `bootloader`.
- `test.ps1` boots headless QEMU, greps serial markers, auto-passes/fails.
- `test-para.ps1` measures SMP wall-clock speedup; it prefers **WHPX**
  (hardware vCPUs — a real parallelism measurement) and falls back to
  `tcg,thread=multi` with relaxed assertions, because software TCG time-shares
  one emulation thread across the vCPUs.
- FS drivers (M6/M7) tested from **ring 3** through the shell — the payoff of
  the M4-first ordering.
- Manually: `cargo run -- bios-gui` for the graphical window.
---

## P1A status (dated 2026-09-26): owned context - built, then REVERTED

**Verdict: not landed. The tree is back at the P1 safety floor and is green.**

The `TaskCtx` restructure (one owned allocation holding saved SP + kernel stack
+ FPU image, tagged with the CPU it runs on) compiled cleanly but produced a
**double fault (#DF) on the very first context switch**, on every CPU count
including `-smp 1`. It was reverted with `git checkout -- kernel/src/scheduler.rs`
rather than left half-working. Details and the two encoding/ownership traps are
in KNOWLEDGE.md item 16.

**What is verified green at this commit (full suite, sequential):**

| Test | Result |
|---|---|
| `build.ps1` | 0 (no new warnings) |
| `test-smp.ps1` | 0 |
| `test-avx.ps1` | 0 |
| `test-para.ps1` | 0 - `cpus=[1,2,3,0] distinct=4, speedup x4.3` |
| `test-fs-corrupt.ps1` | 0 - 4 corrupt geometries rejected |
| `test-fs.ps1` | 0 |
| `test-gpu.ps1` x2 | 0 / 0 (no flake) |
| `test-shell.ps1` | 0 |
| `test-latency.ps1` | 0 |

**Latency note (feeds P2).** Present latency is bimodal, not uniformly ~300 us:
`damage->present` measured `min=283 us` but `max=111944 us`, and one run showed
`wake/schedule avg=110310 us` with pacing at only `7.89 fps`
(`interval avg=126651 us`). The fast path is as good as measured before, but the
tail is dominated by **scheduler wake-up / cadence**, not by the GPU driver. This
is direct evidence for the P2 deadline timer: the 1 ms LAPIC quantum cannot
service a 1000 FPS loop, and a periodic task is being made to wait a whole
quantum before it is run.

**Next (unchanged order):** redo P1A one small step at a time - build + `test-smp`
after each edit - until one owned context is genuinely in place; then P2
deadline timer; then P6a surface/blit + scalable Unicode font, compositor and
input dispatch, and finally the first desktop (taskbar, clock, one window).

---

## P1A progress (updated 2026-09-27): validator landed, FPU now owned

Two of the three planned increments are in. Migration is still disabled.

**Step 1 - context canary + pre-switch validation (LANDED, `c8f3336`).**
Every task kernel stack is stamped with `STACK_CANARY` at its LOW end (where a
downward overflow hits first), and `validate_ctx()` runs on both contexts right
before `context_switch`, checking: `sp` non-zero and 8-byte aligned, `sp` INSIDE
that task's own stack, the FXSAVE image 64-byte aligned, and the canary intact.
It reports once per task id, then stays quiet. Diagnostic only - the switch still
proceeds, because refusing to switch could livelock the very CPU that would
report it.

This is the tool that makes the rest of P1A safe to attempt: step A's original
failure was an unattributable `#DF`, and this names the offending context.

**Step 2a - the FPU image is OWNED by the task (LANDED, `3ac6a25`).**
`Task.fpu_area` is now `Box<FpuArea>` rather than a `*mut FpuArea` into a
`Box::leak`. `fpu::new_area()` is replaced by `new_owned_area()` and the leaking
constructor is deleted, not left unused. Alignment is free: `FpuArea` is
`#[repr(C, align(64))]` and `Box` preserves alignment. That removes one of the
three separate per-task allocations.

**The #DF root cause is now known** (see KNOWLEDGE.md): it was not a pointer
bug. `Box::new(TaskCtx { stack: [0u8; 65536], .. })` builds the whole ~66 KB
struct as a temporary ON THE STACK before copying it to the heap, which cannot
fit in a 64 KiB kernel stack. The spawn smashed the stack and the damage only
surfaced at the next switch.

**Step 2b - combine sp + fpu + stack into ONE allocation (NOT DONE).** The fix
is to allocate the box first and initialise it in place
(`alloc_zeroed(Layout::new::<TaskCtx>())` + raw-pointer field writes), never a
struct literal containing the big array. Until that lands, a task context is
still 2 allocations (FPU box + leaked stack) plus the leaked 8-byte SP slot.

**Suite green at `3ac6a25`** (strictly sequential): build/smp/avx/para(x4.9, 4
distinct CPUs)/fs-corrupt/fs/gpu x2/shell/latency all 0, zero `ctxcheck` reports.
AVX: 58 rounds, 16 YMM lanes intact across 21 switches, XCR0=0x7.

## P1A LANDED: one owned context (2026-09-27, `6ca1ba9`)

**The goal of P1A is achieved: a task context is ONE owned allocation.**

`TaskCtx { stack, sp, fpu }` is a single `#[repr(C, align(64))]` value held as
`Box<TaskCtx>`. All three former per-task allocations are gone - the leaked
saved-SP slot, the leaked kernel stack, and the `Box<FpuArea>`. A context now
has a single address, a single owner and a single lifetime.

**How the old #DF was avoided.** `new_ctx()` calls
`alloc_zeroed(Layout::new::<TaskCtx>())` and writes each field through the raw
pointer, so the ~66 KB struct is NEVER materialised on the stack. That temporary
was the entire cause of the original crash. `fpu::seed_in_place()` uses
`copy_from_slice` (a memcpy) for the same reason.

**Layout.** The stack is the FIRST field, so its base is the allocation base and
is 64-byte aligned; the inline `fpu` image is 64-byte aligned for `fxsave`
without any manual alignment code.

**The CPU tag stays in `Task::owner_cpu` and is deliberately NOT duplicated**
into the context. Two fields that mean the same thing can disagree, which is
exactly the split-brain hazard P1 exists to eliminate. When migration is enabled,
the tag MOVES into the context and `owner_cpu` is deleted in the same change.

**Suite green at `6ca1ba9`** (strictly sequential): build/smp/avx/para(x4.26, 4
distinct CPUs)/fs-corrupt/fs/gpu x2/shell/latency all 0, zero `ctxcheck` reports,
zero `#DF`.

**What P1A deliberately did NOT do:** migration is still disabled and the
affinity filter in `plan_switch` still holds. The structural prerequisite is now
in place, but re-enabling migration is a separate, riskier change - it must move
the CPU tag into the context and delete `owner_cpu` atomically, and it should be
gated behind the validator, not done blind.

**Recommended next:** P2 deadline timer. The owned context is a safety
prerequisite, not a speed fix - latency is still bimodal (`min=336 us` but
`max=157706 us`, pacing 7.67 fps, `wake/schedule avg=133850 us`). Cadence, not
the GPU or the context layout, is what caps the frame rate.
