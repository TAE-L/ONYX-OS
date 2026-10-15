# OnyxOS

A performance-first custom operating system written in Rust for `x86_64` —
built for **gaming and low latency**, with a vision of running both **Linux
files (ext2)** and **Windows files (FAT32/NTFS)**, and eventually **Windows
`.exe`/`.dll` binaries**.

Everything — toolchain, emulator, build artifacts and tests — lives inside
this folder, and the OS is only ever executed inside **QEMU** on this machine
(no hardware boots).

> Full vision, milestone rationale, per-phase results and the GPU roadmaps live
> in **[`docs/PLAN.md`](docs/PLAN.md)**. Hard-won gotchas live in
> **[`docs/KNOWLEDGE.md`](docs/KNOWLEDGE.md)**.

**This is where the project stops.** Phases 0 and 1 are complete; Phases 2–4
are not started. This README is also the retrospective — what the project was
meant to be, what it got right, what it got wrong, and what should have been
done sooner. Read [`docs/OPEN_ISSUES.md`](docs/OPEN_ISSUES.md) before trusting
anything marked "done": several items are *done* in the sense that the code
exists, not that the behaviour is finished.

---

## What it was meant to be

Two signature goals beyond "a working hobby OS":

1. **Split personality filesystem & binaries** — read/write both Linux files
   (ext2) and Windows files (FAT32, then read-only NTFS), and *ultimately run
   Windows `.exe`/`.dll` binaries*.
2. **Max performance, minimal latency** — low-latency interrupt handling,
   preemptive multitasking, and a graphics/GPU path from day one, with the full
   GPU driver as the closing milestone.

One principle shaped the order of everything: **syscalls and ring-3 came
before the filesystem drivers**, so FAT32/ext2 were written against a settled
kernel/app boundary and tested from ring 3 rather than being re-plumbed later.

## What it actually became

| | |
|---|---|
| Kernel | **~24,000 lines** of `no_std` Rust (`kernel/src/`) |
| Syscalls | **24** |
| Test suites | **35** PowerShell suites in `tests/`, plus an independent FAT32 verifier |
| Milestones | **M0 → M10 complete**; M9.5 deferred; M11–M15 not started |
| Audit phases | **Phase 0 and Phase 1 complete**; Phases 2–4 not started |

Bootable on BIOS and UEFI, preemptive multitasking, ring-3 userspace with an
ELF loader and shell, FAT32 and ext2 read/write through a path-aware VFS, SMP,
UEFI framebuffer console, and a working virtio-gpu present path with a
hardware cursor.

---

## Layout

| Path | Purpose |
|------|---------|
| `kernel/` | The `no_std` kernel (VGA, serial, VFS, FAT32, ext2, GPU…) |
| `src/main.rs` | Host-side launcher that boots the images in QEMU |
| `build.rs` | Combines kernel + `bootloader` into bootable images |
| `user/` | Ring-3 programs (`shell`, `fstest`, `evtest`, `hello`, `argtest`) |
| `tools/` | Host-side utilities (`mkfat`, `mkext2`, `fatfs-verify`) |
| `tests/` | **All 35 test suites + `run-all.ps1` + `fat32-verify.ps1`** |
| `logs/<suite>/` | Per-suite serial logs (git-ignored) |
| `docs/` | `PLAN.md`, `KNOWLEDGE.md`, `OPEN_ISSUES.md`, `ARCHITECTURE-ANALYSIS.md` |
| `.toolchain/` | Self-contained Rust, QEMU and GCC toolchains |
| `target/debug/images/` | Generated `bios.img` / `uefi.img` |

Test scripts used to sit loose in the repo root alongside their logs; they were
migrated to `tests/` and `logs/` with history preserved via `git mv`. Every
suite derives `$root` by double-parenting its own path, which is why they still
find `target\debug\images` after the move.

## How to build & run (Windows PowerShell)

```powershell
# build from a fresh shell (sets up the portable toolchain itself)
powershell -ExecutionPolicy Bypass -File build.ps1

# boot it
cargo run -- bios          # headless, serial to console, no window
cargo run -- bios-gui      # graphical QEMU window

# run the tests
powershell -ExecutionPolicy Bypass -File tests\run-all.ps1          # everything
powershell -ExecutionPolicy Bypass -File tests\run-all.ps1 -Only fs,block   # subset
powershell -ExecutionPolicy Bypass -File tests\test-fsync.ps1       # one suite
```

> Toolchain notes (all self-contained, non-admin, inside `.toolchain/`):
> - rustup + **nightly GNU** toolchain with `x86_64-unknown-none` /
>   `x86_64-unknown-uefi` targets, `llvm-tools` and `rust-src`.
> - **mingw-w64 (winlibs)** provides `gcc`, `dlltool`, `ld`, `ar`, `windres`,
>   `make`, `gdb` on PATH.
> - **QEMU 11.1.0** portable install.
> - No Visual Studio / MSVC needed.

> **Build the whole workspace, not a single crate.** `cargo build` at the root
> builds the kernel as an artifact dependency for `x86_64-unknown-none` and
> seeds the FAT image via `build.rs`. Passing `--manifest-path kernel\Cargo.toml`
> links for the host target and fails with a confusing linker error.
> Editing kernel source alone will **not** refresh `bios.img`'s seeds — touch
> `build.rs` to force it.

## Wins

Things that are genuinely done and hold up under scrutiny:

- **M0 → M10.** Boot, interrupts, memory, preemptive multitasking, syscalls +
  ring-3, block layer, FAT32, ext2, subdirectories, ELF-from-disk shell, UEFI,
  Linux-ABI static ELF execution, SMP, and GPU drivers (dispi modesetting +
  virtio-gpu transport/control/present/hardware-cursor).
- **SMEP + SMAP are hardware-enforced** (Phase 0.1). Enabling SMAP immediately
  faulted three unprotected user-page writes — the protection found real bugs
  on day one rather than being a checkbox.
- **The filesystem actually round-trips.** FAT32 writes long filenames,
  `rename` preserves them, and both filesystems stamp real timestamps.
- **Durability is verifiable from outside the guest.** `fsync` is proven by
  booting without `-snapshot`, killing QEMU, and reading the raw image back
  with a reader that never ran inside the kernel it checks.
- **Independent verification exists at all** — `tests/fat32-verify.ps1` is a
  second implementation of the on-disk format written from the spec, and
  `tools/fatfs-verify` uses the third-party `fatfs` crate as a third opinion.
- **It knew when to revert.** The `blit` fast path (0.4) failed 31-of-32 pixels
  against a self-test that read real values back, and was reverted with the
  reasoning written down. The mouse IIR damper got the same treatment.
- **Fully self-contained and non-admin.** Rust, QEMU and GCC live in
  `.toolchain/`. No Visual Studio, no MSVC, no elevation needed to build.
- **Honest documentation.** A false claim I made about the FAT checksum was
  retracted *in place* in `PLAN.md` rather than quietly deleted.

---

## Mistakes — what could have been better from the beginning

### 1. Round-trip testing was the default, and it hid four shipped bugs

Every serious filesystem bug in this project shared one shape: **our writer
and our reader agreed with each other while both disagreed with the format.**

- wrong LFN character offsets — invisible, we read our own bytes back
- `LAST_LONG` in the type byte — invisible, our reader ignored that bit
- a directory-slot write that erased the *next* entry — invisible, and it was
  **destroying files**
- LFN chunks stored in reverse — invisible, our reader sorted to match

**None of these could ever be found by a round-trip test.** The independent
verifier should have been built when FAT32 write landed (M6), not three phases
later in 1.5. A second implementation written from the specification is not a
luxury — it is the only thing that can see this class of bug.

### 2. The independent verifier itself was wrong

Its first checksum implementation used a plain sum where the format uses a
rotate — so it "found" a bug in the kernel that did not exist, and the "fix"
would have broken **every long filename on real hardware** while every test
stayed green. It was caught only because a third implementation disagreed with
*both*.

> **"My new test fails" is not evidence that the code is wrong.** It can just
> as easily be evidence that the checker is. An independent checker is only as
> good as its own correctness.

### 3. Built bottom-up — right for learning, wrong for a product

The architecture audit put it plainly: *no craftsmanship problems, several
architectural ones*. Comments explain *why*, failures are documented rather
than hidden, self-tests read real pixels. But the layering means **~4,700
lines of graphics work sits on top of a layer that does not exist**: surface
damage and present damage are still separate accumulators (finding J), so a
composited frame never reaches scanout. The architecture should have been
designed before the graphics, not discovered after them.

### 4. No per-process address spaces from the start (finding A)

All ring-3 programs share one address space — `userspace.rs` says so in a
comment that has been true for the whole project. This is the single biggest
piece of debt: it is *why* SMEP/SMAP were needed, it blocks KPTI from meaning
anything, and it makes Phase 4 without Phase 3 the worst available ordering
mistake. Every milestone made it more expensive to change.

### 5. The audit came too late

The architecture audit ran against a **16,110-line kernel** — after
filesystems, graphics, SMP and GPU work were all built. Findings A–M would
have been far cheaper at 4,000 lines. Reviews like that should be periodic,
not something you run when the project feels finished.

### 6. Verification discipline was enforced by prose, not tooling

"Run the test suites sequentially" was written down and kept being violated —
because a rule that lives in prose has no enforcement. `tests/run-all.ps1`
exists now (it kills QEMU between suites), but it should have arrived with the
second suite, not the thirty-fifth.

### 7. Numbers were repeated instead of derived

The `fsync` durability test hardcoded `size=40` against a 42-byte payload, so
it **failed against a kernel that had written the correct size**. Worse, the
guest test called `sayln()`, which that binary never defined — the tree did
not build. A number duplicated in two places is a bug waiting to happen.

### 8. Claims were made that were never measured

The Phase 0.3 `fill_row` speedup is *reasoned*, not benchmarked. It is marked
that way, and the rule in `PLAN.md` is that no number is a measurement unless
it says "measured" — but it should never have been a reasoned claim in a
project this test-heavy.

---

## What should have been done earlier

| Should have come | Instead arrived | Why it matters |
|---|---|---|
| Independent FAT verifier — with M6 | Phase 1.5 | Caught 2 shipped-breaking bugs; the first four were invisible to every other test |
| SMEP/SMAP — first hardening pass | Phase 0.1 | Found 3 real unprotected writes on day one; cheap late, cheapest early |
| Per-process CR3 (3.1) | never | Blocks KPTI, `mmap`, real isolation, and safe Phase 4 |
| Architecture audit | at 16k lines | Would have caught the graphics-without-a-layer problem much sooner |
| `fsync` / timestamps | with the write path / `stat` | Durability and metadata are properties of a write, not an afterthought |
| `run-all.ps1` | suite #35 | Prose rules do not get followed; harnesses do |

---

## What was not done

**Deferred, blocked, or left open** — all recorded in
[`docs/OPEN_ISSUES.md`](docs/OPEN_ISSUES.md):

- **A5 — multi-slot long names are not proven readable.** The `fatfs` oracle
  reads our single-slot long names but returns only the 8.3 alias for
  multi-slot ones, and nothing explains why. **Filenames longer than 13
  characters are unverified outside our own reader.** This is the one real
  open mystery.
- **A3 — Windows will not mount the images.** Needs elevation this host does
  not have, so "Windows-compatible" is still an inference, not an observation.
- **A1** duplicate directory entry under a colliding alias · **A2** `mkfat`
  writes non-conformant LFN entries (upstream) · **A4** ext2 has no
  independent verifier at all · **A6** `fsync` is durable, not crash-consistent.
- **0.4 blit fast path** reverted (31/32 pixels wrong) · **0.6** blocked on the
  toolchain · **0.3** never benchmarked.
- `fsync`'s "crash" is a QEMU process kill, which says nothing about a real
  power cut, and it does not replace a journal.

**Not started at all:**

- **Phases 2–4:** huge pages, the damage→scanout bridge, virtio-gpu
  completion, **ext4**, per-process CR3, `mmap`, shared memory, scheduler
  migration, virtio-blk, **the entire network stack** (ethernet → ARP → IP →
  UDP → TCP → sockets), and the **PE loader**.
- **Milestones:** M9.5 desktop, M11 NTFS, M12 PE foundation, M13 GUI,
  M14 Windows binaries, M15 full GPU driver.
- **Architectural gaps:** no `mmap` family, no networking whatsoever, no KPTI,
  no huge pages, scheduler migration disabled, ATA still PIO.

> **The uncomfortable summary**, taken from the plan: the phases were ordered
> fastest-to-slowest, so the **biggest** limitations land last. After Phase 2
> the OS looks most complete and is least safe. Phases 3 and 4 are what turn
> "a demo that boots" into "something you could daily-drive".

## Status

- [x] M0  bootable kernel (BIOS) — prints "Hello, OnyxOS!" to VGA + serial ✅
- [x] M1  interrupts (GDT/IDT), exceptions handled with RIP + halt, double-fault stack ✅
- [x] M2  memory: frame allocator from memory map, page-table mapping, kernel heap (Vec/Box) ✅
- [x] M3  preemptive multitasking: PIT @100 Hz, round-robin scheduler (naked-asm context switch), FPU/SSE state now saved per task ✅
- [x] M4  syscalls + ring-3 userspace + ELF loader ✅ (syscall/sysret fast path; user-pointer validation landed in M9.6)
- [x] M5  block layer: ATA PIO, MBR/GPT ✅
- [x] M6  VFS + FAT32 read/write (Windows files), tested from ring 3 ✅
- [x] M7  ext2 read/write (Linux files), tested from ring 3 ✅
- [x] M7.5 path-aware VFS (subdirectories) ✅
- [x] M8  ELF loader from disk + ring-3 shell ✅
- [x] M9  UEFI boot (GPT + OVMF) + graphical framebuffer console ✅
- [ ] M9.5 graphical desktop userspace — **deferred**
- [x] M9.6 core hardening + missing subsystems ✅ — **complete**: A1 TSC ns timekeeping, A2 APIC/IOAPIC + LAPIC preemption, A3 scheduler v2, A4 FPU/SSE save-restore, A5 block cache, A6 frame allocator v2, B1 PCI/`lspci`, B2 ACPI/MADT, B3 process lifecycle (waitpid/kill/zombies), B4 raw input ring, B5 latency instrumentation, C1 argv/envp/auxv, C2 errno, C3 file API. Regressions pass on BIOS + UEFI.
- [x] M9.7 Linux ABI compat: run static Linux ELFs [done]
- [x] M9.8 SMP - multi-CPU bring-up, AP workers, scheduler + FPU/SSE/AVX save-restore [done] (test-smp.ps1, test-avx.ps1, test-para.ps1; x4.6-4.8 parallel speedup)
- [x] M10 GPU drivers - the gaming track (detail below)
- [ ] M11 NTFS read-only + multi-drive mounting
- [ ] M12 PE foundation: parse .exe/.dll, relocations, import table — solid groundwork
- [ ] M13 GUI: window manager + compositor + built-in apps
- [ ] M14 VISION: run Windows .exe/.dll binaries
- [ ] M15 VISION: full GPU driver (the last milestone)

### M10 GPU track (the gaming path) - where it actually is

- [x] M10a   PCI display scan + kernel modesetting (dispi), 1920x1080x32, canary-verified
- [x] M10b1  virtio-gpu transport probe (capability list, VERSION_1, queues, features)
- [x] M10b2  control virtqueue + GEM-lite 2D resource + damage-rect present path
           (create -> attach -> canary -> set_scanout -> transfer -> flush -> console adopt)
- [x] M10b3  latency instrumentation + **hardware cursor** on queue 1 (MOVE_CURSOR);
           input->cursor ~130 us, a ~440x win over the software sprite
- [x] M10 P1  task-context memory safety (CPU affinity: one task context is touched
           by ONE cpu) + disk-input-driven panic hardening (GPT validation + self-test)
- [ ] NEXT: per-CPU owned contexts (P1 step A) -> deadline timer (P2, real frame
           pacing) -> drawing substrate + scalable font -> compositor -> first desktop

> The present path is ALREADY sub-millisecond (~220-320 us per present). The
> remaining frame-rate ceiling is CPU scheduling and the 1 ms tick, NOT the GPU
> driver — so the next work is the scheduler and a drawing substrate, not more
> driver code. Full status and the hard-won gotchas:
> **[`docs/KNOWLEDGE.md`](docs/KNOWLEDGE.md)** (bugs, wrong assumptions, roadmap).

### Audit phases (P16, 2026-10-01)

| Phase | Contents | Status |
|---|---|---|
| **0** — free wins | SMEP/SMAP · page-table aliasing · bulk row writes · blit fast path · doc dedup · `static mut` cleanup | ✅ done, except 0.4 **reverted** and 0.6 **blocked** |
| **1** — cheap correctness | LBA48 · FAT32 LFN write · `rename` LFN · timestamps · independent interop verification · `fsync` | ✅ **all done** |
| **2** — big but self-contained | huge pages · damage→scanout bridge · virtio-gpu · **ext4** | not started |
| **3** — the hard rework | **per-process CR3** · `mmap` · shared memory · scheduler migration · virtio-blk | not started |
| **4** — the monsters | ethernet → ARP → IP → UDP → **TCP** → sockets · **PE loader** | not started |

---

## Read next

| File | What's in it |
|---|---|
| [`docs/OPEN_ISSUES.md`](docs/OPEN_ISSUES.md) | **Start here.** Bugs found and deliberately left unfixed, plus design gaps and the handoff list |
| [`docs/PLAN.md`](docs/PLAN.md) | Vision, milestones, the full P16 audit, and a per-phase result section for every completed item |
| [`docs/KNOWLEDGE.md`](docs/KNOWLEDGE.md) | Hard-won gotchas, wrong assumptions, GPU roadmaps |
| [`docs/ARCHITECTURE-ANALYSIS.md`](docs/ARCHITECTURE-ANALYSIS.md) | The architecture audit that produced findings A–M |
| [`docs/B5-DEBUG-STATE.md`](docs/B5-DEBUG-STATE.md) | The console-lock deadlock investigation |
