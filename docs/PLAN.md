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
| **M10** | **GPU drivers — placeholder**: PCI GPU scan + modesetting + framebuffer-accelerated stubs (no full accel yet) | gaming track start |
| M11 | NTFS read-only + multi-drive mounting | extra Windows compat |
| **M12** | **PE foundation**: parse `.exe` / `.dll` (PE/COFF), relocations, DLL imports groundwork | solid foundation only |
| M13 | GUI: window manager + compositor + built-in apps (terminal, file manager) | apps on the M9.5 base |
| **M14** | **VISION: run Windows `.exe` / `.dll` binaries** (via M12 + syscall emulation layer) | vision capstone #1 |
| **M15** | **Full GPU driver** (accel via VM-based GPU, Vulkan-lite API) | vision capstone #2 — the last |

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

### Success criteria

`test-smp.ps1`: at `-smp 4` all three APs reach 64-bit mode, come online with
their own GDT/TSS/IDT/CR0/CR4/EFER/SYSCALL MSRs and timers, run their worker
tasks (non-zero, growing iteration counts, FPU checks passing), advance their
own per-CPU tick counters, and the system keeps passing the earlier milestones'
markers (fstest/fpu-test/argv) with no PANIC, no unexpected exception and no
stall diagnostic. `-smp 2` starts exactly one AP; `-smp 1` starts none and
leaves the single-CPU path untouched.

---

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
- **M10** — GPU *placeholder*: detect the GPU (PCI), set framebuffer
  resolution/modes, expose a small "render surface" API, and stub the
  acceleration interfaces (so the API shape is set before the full driver).
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
- FS drivers (M6/M7) tested from **ring 3** through the shell — the payoff of
  the M4-first ordering.
- Manually: `cargo run -- bios-gui` for the graphical window.