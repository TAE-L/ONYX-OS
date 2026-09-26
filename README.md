# OnyxOS

A performance-first custom operating system written in Rust for `x86_64` â€”
built for **gaming and low latency**, with a vision of running both **Linux
files (ext2)** and **Windows files (FAT32/NTFS)**, and eventually **Windows
`.exe`/`.dll` binaries**.

Everything â€” toolchain, emulator, build artifacts and tests â€” lives inside
this folder, and the OS is only ever executed inside **QEMU** on this machine
(no hardware boots).

> Full vision, milestone rationale and the UEFI/GPU/PE roadmaps live in
> **[`docs/PLAN.md`](docs/PLAN.md)**.

---

## Layout

| Path                 | Purpose                                              |
|----------------------|------------------------------------------------------|
| `kernel/`            | The `no_std` kernel (VGA, serial, later VFS etc.)    |
| `src/main.rs`        | Host-side launcher that boots the images in QEMU     |
| `build.rs`           | Combines kernel + `bootloader` into bootable images  |
| `.toolchain/`        | Self-contained Rust, QEMU and GCC toolchains         |
| `target/images/`     | Generated `bios.img` / `uefi.img`                    |
| `test.ps1`           | Headless automated smoke test                        |

## How to build & run (Windows PowerShell)

```powershell
# one-time environment for this shell
$env:RUSTUP_HOME = "$PWD\.toolchain\rustup"
$env:CARGO_HOME  = "$PWD\.toolchain\cargo"
$env:Path = "$PWD\.toolchain\cargo\bin;$PWD\.toolchain\w64devkit\bin;" + $env:Path

cargo build                                   # build kernel + boot images
cargo run -- bios                             # boot in QEMU (serial to console, no window)
cargo run -- bios-gui                         # boot in a QEMU graphical window
powershell -ExecutionPolicy Bypass -File test.ps1   # headless automated test
```

> Toolchain notes (all self-contained, non-admin, inside `.toolchain/`):
> - rustup + **nightly GNU** toolchain with `x86_64-unknown-none` / `x86_64-unknown-uefi`
>   targets, `llvm-tools` and `rust-src`.
> - **mingw-w64 (winlibs)** full standalone build provides `gcc`, `dlltool`, `ld`,
>   `ar`, `windres`, `make`, `gdb`, etc. on PATH.
> - **QEMU 11.1.0** portable install.
> - No Visual Studio / MSVC needed.

## Status

- [x] M0  bootable kernel (BIOS) â€” prints "Hello, OnyxOS!" to VGA + serial âœ…
- [x] M1  interrupts (GDT/IDT), exceptions handled with RIP + halt, double-fault stack âœ…
- [x] M2  memory: frame allocator from memory map, page-table mapping, kernel heap (Vec/Box) âœ…
- [x] M3  preemptive multitasking: PIT @100 Hz, round-robin scheduler (naked-asm context switch), FPU/SSE state now saved per task âœ…
- [x] M4  syscalls + ring-3 userspace + ELF loader âœ… (syscall/sysret fast path; user-pointer validation landed in M9.6)
- [x] M5  block layer: ATA PIO, MBR/GPT âœ…
- [x] M6  VFS + FAT32 read/write (Windows files), tested from ring 3 âœ…
- [x] M7  ext2 read/write (Linux files), tested from ring 3 âœ…
- [x] M7.5 path-aware VFS (subdirectories) âœ…
- [x] M8  ELF loader from disk + ring-3 shell âœ…
- [x] M9  UEFI boot (GPT + OVMF) + graphical framebuffer console âœ…
- [ ] M9.5 graphical desktop userspace â€” **deferred**
- [~] M9.6 core hardening + missing subsystems â€” **in progress**: latent-bug fixes (FPU/SSE save-restore, user-pointer validation, serial-print deadlock) + A1 TSC ns timekeeping (`test-fpu.ps1`, `test-time.ps1`) + A2 APIC/IOAPIC + 1000 Hz LAPIC-timer preemption (`test-apic.ps1`) + A3 scheduler v2 (`test-sched.ps1`) + A5 block cache (`test-block.ps1`) + B1 PCI enumeration + `lspci` (`test-pci.ps1`) + B2 ACPI core / MADT (`test-acpi.ps1`) + B3 process lifecycle â€” waitpid/kill/zombie reaping (`test-proc.ps1`) + B4 raw input event ring â€” `SYS_INPUT_READ`, 24-byte ns-stamped key/mouse records, blocking wake, ring-3 `EVTEST.ELF` E2E (`test-raw.ps1`) + framebuffer live text console + input/clock serial observability (`test-input.ps1`); B5 latency instrumentation â€” boot/shell `perf` snapshots + console-lock deadlock fix that froze the ~3 s snapshot ~1-in-6 boots (`test-perf.ps1`, 22/22 clean, `docs/B5-DEBUG-STATE.md`) + A6 frame allocator v2 â€” O(1) alloc/free with an intrusive free list replacing the never-free bump allocator, alloc counters in the `perf` snapshot, heap grown to 16 MiB (`test-memory.ps1`); C1 argv/envp/auxv process-start stack â€” SYS_SPAWN tokenizes the command line (argv[0]=path, rest=argv[1..]), SysV initial stack + auxv built by the kernel, ring-3 ARGTEST.ELF E2E (`test-args.ps1`); C2 errno â€” new `kernel/src/errno.rs`, Linux `-errno` returns for failing syscalls (vfs + spawn errors mapped), SYS_READ fd-vs-buffer precedence, all 8 regression suites green; next: C3 file API
- [x] M9.7 Linux ABI compat: run static Linux ELFs [done]
- [x] M9.8 SMP - multi-CPU bring-up, AP workers, scheduler + FPU/SSE/AVX save-restore [done] (test-smp.ps1, test-avx.ps1, test-para.ps1; x4.6-4.8 parallel speedup)
- [x] M10 GPU drivers - the gaming track (detail below)
- [ ] M11 NTFS read-only + multi-drive mounting
- [ ] M12 PE foundation: parse .exe/.dll, relocations, import table â€” solid groundwork
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
