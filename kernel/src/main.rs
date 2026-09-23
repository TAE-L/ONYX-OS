#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

#[macro_use]
extern crate alloc;

mod allocator;
mod apic;
mod acpi;
mod ata;
mod bench;
mod blkcache;
mod block;
mod errno;
mod ext2;
mod fat;
mod fpu;
mod framebuffer;
mod gdt;
mod gpu;
mod input;
mod interrupts;
mod keyboard;
mod ksl;
mod memory;
mod mouse;
mod pci;
mod perf;
mod pic;
mod pit;
mod rtc;
mod scheduler;
mod serial;
mod smp;
mod syscall;
mod time;
mod userspace;
mod vfs;
mod virtio;

use alloc::boxed::Box;
use alloc::vec::Vec;
use bootloader_api::config::Mapping;
use bootloader_api::info::Optional;
use bootloader_api::{BootInfo, entry_point};
use core::fmt::Write;
use x86_64::structures::paging::mapper::Translate;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

/// Bootloader configuration: we ask the bootloader to map the complete
/// physical memory into the virtual address space (dynamically placed) so the
/// kernel can manage page tables. See `BootInfo::physical_memory_offset`.
const CONFIG: bootloader_api::BootloaderConfig = {
    let mut config = bootloader_api::BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &CONFIG);

/// Text drawn to the framebuffer (visible in the QEMU window). The text
/// cursor starts just below the header bar drawn by `draw_header`.
const SCREEN_TEXT: &str = "    Hello from OnyxOS 0.1!\n    M7: ext2 + FAT32 live - RTC clock in the header - move the mouse!";

/// Kernel entry point. `boot_info` contains the memory map, framebuffer
/// and other information gathered by the bootloader.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    let mut port = serial::init_port();
    let _ = writeln!(port, "Hello, OnyxOS!");
    let _ = writeln!(
        port,
        "kernel running at -> memory regions: {}",
        boot_info.memory_regions.len()
    );

    // --- FPU/SSE state management: must run before any task is spawned and
    // before the first context switch, so every fxsave/fxrstor works and
    // fresh task areas hold a valid (exceptions-masked) image.
    fpu::init();
    serial_writeln!("fpu: x87/SSE state management ready (eager fxsave/fxrstor on switch)");

    // Draw directly onto the framebuffer so the OS is visible on screen.
    // The writer is kept alive globally: the mouse IRQ uses it for the cursor.
    let mut writer: Option<framebuffer::FrameBufferWriter<'static>> = None;
    match &mut boot_info.framebuffer {
        Optional::Some(fb) => {
            let info = fb.info();
            let buffer: &'static mut [u8] =
                unsafe { core::mem::transmute(fb.buffer_mut()) };
            let mut w = framebuffer::FrameBufferWriter::new(buffer, info);
            w.draw_header("OnyxOS 0.1");
            w.write_str(SCREEN_TEXT);
            writer = Some(w);
            let _ = writeln!(
                port,
                "drew text to framebuffer ({}x{}, {}bpp)",
                info.width, info.height, info.bytes_per_pixel
            );
        }
        Optional::None => {
            let _ = writeln!(port, "no framebuffer available");
        }
    }
    if let Some(w) = writer {
        framebuffer::init_global(w);
        let (cx, cy) = framebuffer::cursor_pos();
        let _ = writeln!(port, "mouse cursor drawn at ({}, {})", cx, cy);
    }

    // --- RTC quick-win: read the CMOS wall clock once at boot ---
    rtc::init();

    // --- M1: interrupts, exceptions, GDT/TSS ---
    // M9.8: the BSP's per-CPU state (GS base + idle stack) comes first — the
    // per-CPU TSS inside `gdt::init` takes its idle RSP0 from there.
    smp::init_bsp();
    gdt::init();
    interrupts::init();
    pic::init();
    serial_writeln!("interrupts + GDT/TSS initialized");

    // M1 demo: recoverable breakpoint exception.
    serial_writeln!("triggering breakpoint (int3)...");
    x86_64::instructions::interrupts::int3();
    serial_writeln!("breakpoint handled, continuing...");

    // --- M2: memory management ---
    // Count usable regions before `boot_info` is moved into the memory module.
    let usable_regions = boot_info
        .memory_regions
        .iter()
        .filter(|r| r.kind == bootloader_api::info::MemoryRegionKind::Usable)
        .count();
    // Build a frame allocator from the bootloader's memory map.
    // The memory-map data lives as long as the (static) boot info, so we curl
    // the reference into a 'static slice without keeping `boot_info` borrowed.
    let mem_regions: &'static [bootloader_api::info::MemoryRegion] = unsafe {
        let regions: &[bootloader_api::info::MemoryRegion] = &boot_info.memory_regions;
        core::mem::transmute(regions)
    };
    let mut frame_allocator = unsafe { memory::BootInfoFrameAllocator::init(mem_regions) };
    // M9.8: reserve ONE frame below 1 MiB for the AP trampoline (SIPI can only
    // start a CPU at a real-mode page). Done here, before the heap claims any
    // frame, so the page is guaranteed untouched and never handed out again.
    match unsafe { frame_allocator.reserve_low_frame() } {
        Some(addr) => smp::set_low_page(addr),
        None => serial_writeln!("[smp] no usable frame below 1 MiB - SMP will stay off"),
    }
    // Set up paging: mapper over the active level-4 page table.
    // Capture the RSDP pointer NOW: `memory::init(boot_info)` below MOVES the
    // boot_info reference (it cannot be re-read afterwards). Read-only ACPI
    // parsing happens later, but the address is already in hand.
    let rsdp = match boot_info.rsdp_addr {
        Optional::Some(addr) => Some(addr),
        Optional::None => None,
    };

    let mut mapper = memory::init(boot_info);
    serial_writeln!(
        "memory init: phys_offset={:#x}, usable_regions={}",
        mapper.phys_offset().as_u64(),
        usable_regions
    );

    // Initialize the kernel heap (maps 16 MiB + sets up global allocator).
    allocator::init_heap(&mut mapper, &mut frame_allocator);
    serial_writeln!(
        "heap initialized at {:#x}..{:#x}",
        allocator::HEAP_START,
        allocator::HEAP_START + allocator::HEAP_SIZE
    );

    // --- M2 tests ---
    heap_allocation_test();
    paging_test(&mut mapper, &mut frame_allocator);

    // --- M3: preemptive multitasking + keyboard/mouse ---
    serial_writeln!("M3: pit init...");
    pit::init(); // PIT channel 0 at 100 Hz
    serial_writeln!("M3: mouse init...");
    if let Err(e) = mouse::init() {
        serial_writeln!("mouse init failed: {e}");
    }
    serial_writeln!("M3: enabling IRQs...");
    // Unmask the IRQs we use (they are all masked after `pic::init()`).
    pic::enable_irq(0); // PIT timer
    pic::enable_irq(1); // PS/2 keyboard
    pic::enable_irq(12); // PS/2 mouse
    serial_writeln!("M3: spawning tasks...");
    // M9.8: the SMP bring-up task rides along with the M3 spawns so it is
    // already scheduled when the APIC stack goes live (see `smp::bringup_task`).
    smp::spawn_bringup_task();
    // M9.8: the interrupt-liveness heartbeat is a task too — the boot context
    // stops being scheduled once the timer drives preemption, so anything that
    // must keep reporting has to be a task.
    scheduler::spawn(diag_task);

    // Spawn kernel threads. Interrupts are still disabled here, which is the
    // only reason it is safe to build the task list.
    scheduler::spawn(ticker_task);
    serial_writeln!("M3: spawned ticker");
    // M8 note: the keyboard reader task is gone — the ring-3 shell consumes
    // key events via SYS_READ(0) (keyboard::take_line line discipline).
    scheduler::spawn(mouse_task);
    serial_writeln!("M3: spawned mouse");
    scheduler::spawn(clock_task);
    serial_writeln!("spawned header clock");
    // FPU/SSE corruption regression: two tasks keep live XMM accumulators
    // across preemptions and verify exact checkpoints (see below).
    scheduler::spawn(fpu_test_a);
    serial_writeln!("spawned fpu-test A");
    scheduler::spawn(fpu_test_b);
    serial_writeln!("spawned fpu-test B");
    // M9.8-f: AVX/YMM regression — only when boot enabled XCR0.YMM, so the
    // task's VEX-encoded code can never execute on a CPU without AVX.
    if fpu::avx_enabled() {
        scheduler::spawn(fpu::avx_test_task);
        serial_writeln!("spawned fpu-avx test (XCR0={:#x})", fpu::xcr0());
    } else {
        serial_writeln!("fpu-avx test not spawned (AVX unavailable)");
    }
    // A3 scheduler tests: a Normal-priority sleeper (proves sleep+wake on a
    // 500 ms cadence) and an RT burst task (proves RT holds the CPU against
    // Normal tasks for its burst — no normal-task lines may interleave).
    scheduler::spawn_prio(sched_sleeper, scheduler::PRIO_NORMAL);
    serial_writeln!("spawned sched-sleeper (normal)");
    scheduler::spawn_prio(sched_rt_burst, scheduler::PRIO_RT);
    serial_writeln!("spawned sched-rt-burst (rt)");
    // A5: block-cache benchmark — cold vs hot read, data equality,
    // write-through coherence, deterministic read-ahead probe.
    scheduler::spawn(blk_test);
    // M9.6-B3: process lifecycle regression (waitpid / kill / zombie reap).
    scheduler::spawn(b3_test);
    // M9.6-B4: raw input ring regression (wire layout, drop-on-full,
    // scheduler block-on-raw wake).
    scheduler::spawn(input_test);
    // M9.6-A6: frame-allocator v2 regression (free-list LIFO + bump path).
    scheduler::spawn(mem_test);
    serial_writeln!("spawned mem-test");
    // M9.6-B5: print one full performance snapshot ~3 s into the boot (the
    // shell's `perf` command re-renders it live at any time).
    scheduler::spawn(perf_task);
    serial_writeln!("spawned blk-test");
    // M9.8: parallelism benchmark — 4 CPU-bound workers, wall-clock measured;
    // test-para.ps1 compares -smp 1 vs -smp 4 for real speedup.
    scheduler::spawn(crate::bench::task);
    serial_writeln!("spawned para bench");
    // M10a: re-verify the installed GPU mode from a real task (the boot path
    // did the modeset; this proves the dispi registers and the mapped LFB
    // survive the hand-over to the scheduler). No-op without a dispi mode.
    crate::gpu::spawn_task();
    serial_writeln!("spawned gpu-verify");
    serial_writeln!("multitasking initialized: 5 tasks spawned");

    // --- M4: syscalls + ring-3 userspace ---
    serial_writeln!("M4: enabling syscall entry (EFER.SCE, STAR/LSTAR/SFMASK)...");
    syscall::init();
    serial_writeln!("M4: loading user programs and mapping ring-3 regions...");
    userspace::init(&mut mapper, &mut frame_allocator);
    userspace::spawn_embedded(
        userspace::HELLO_ELF,
        &mut mapper,
        &mut frame_allocator,
        &["hello"],
    );
    // fstest is normally launched by the shell's AUTOEXEC via SYS_SPAWN
    // (proving the disk-ELF path end to end); it is only embedded here if the
    // shell could not be loaded from disk below (regression fallback).
    let mut shell_ok = false;
    serial_writeln!("M4 ready: user tasks in the round robin (drop to ring 3)");

    // --- M5: block layer ---
    serial_writeln!("M5: probing ATA primary master...");
    block::init();

    // --- M6: VFS + FAT32 ---
    serial_writeln!("M6: mounting filesystem...");
    vfs::init();

    // --- M7: ext2 (VFS secondary mount) ---
    serial_writeln!("M7: mounting ext2 partition...");
    ext2::init();

    // --- M8: ELF loader from disk + ring-3 shell ---
    // Hand the frame allocator over to the runtime snapshot (last boot-path
    // use was the embedded spawns above), then load the shell from the FAT32
    // partition. Its link base (0xC00000) is region-checked against the
    // embedded programs (hello @ 0x400000, fstest @ 0x800000).
    memory::init_global_frames(frame_allocator);
    serial_writeln!("M8: loading /SHELL.ELF from disk (ring 3)...");
    match userspace::spawn_from_vfs("/SHELL.ELF") {
        Ok(_shell_pid) => {
            shell_ok = true;
            serial_writeln!("M8 ready: shell live in the round robin (loaded from disk)")
        }
        Err(e) => serial_writeln!("M8: shell load failed: {e}"),
    }
    if !shell_ok {
        // Fallback: keep the M6/M7/M8 regression markers alive without a
        // working disk-ELF loader by running the embedded fstest directly.
        // (The boot allocator lives in the runtime snapshot now, so this goes
        // through the runtime spawn path.)
        if let Err(e) =
            userspace::spawn_bytes_runtime_argv(userspace::FSTEST_ELF, &["/FSTEST.ELF"])
        {
            serial_writeln!("M8: embedded fstest fallback failed: {e}");
        }
    }

    // --- M9.6-B1: PCI enumeration (config-space scan + BASE-address reads).
    // Runs BEFORE interrupts are enabled: the PIIX3 config port is a
    // two-step (select/read) handshake, and a preemption between the two
    // wedges QEMU's config port forever (observed as a scan that hangs at a
    // different device each run). With IF=0 the whole scan is atomic by
    // construction; the runtime `lspci` re-uses this cached registry, so no
    // config I/O ever happens with interrupts on. The registry feeds the
    // shell's `lspci` and the M10 GPU track (VGA BAR = framebuffer base).
    pci::init();

    // --- M10a: GPU scan + kernel-controlled modesetting --------------------
    // Must run AFTER `pci::init` (it reads the display function out of that
    // registry) and AFTER `memory::init_global_frames` above (mapping the
    // framebuffer BAR needs the runtime frame allocator), and it must run with
    // interrupts disabled: the dispi index/data ports (0x1CE/0x1CF) are a
    // two-step access like the PCI config port, and the page-table edits it
    // makes must not interleave with anything else's `map_to`.
    //
    // Either outcome is fine: `gpu::init` only ever logs `[gpu]` markers and
    // falls back to the bootloader framebuffer (which stays mapped and live),
    // so a machine without a display device still boots normally.
    gpu::init();

    // --- M9.6-B2: ACPI core (RSDP/RSDT/XSDT/MADT/HPET). Read-only decode
    // through the physical-memory window; also runs before interrupts for
    // the same two-step-access reason. Cross-checked against A2's APIC wire.
    acpi::init(rsdp);

    // Turn on interrupts: from here the PIT preempts us every 10 ms.
    serial_writeln!("M3: enabling interrupts...");
    x86_64::instructions::interrupts::enable();
    serial_writeln!("interrupts enabled — scheduler active");

    // --- M9.6-A1: high-resolution timekeeping. Needs the PIT running, so it
    // calibrates only now (counts 10 PIT ticks against the TSC).
    time::init();

    // --- M9.6-B4: raw input ring init (size self-check — the encode/decode
    // and the ring-3 E2E test both assume the 24-byte wire layout).
    // M9.8: this runs BEFORE `apic::init`, because the moment the periodic
    // LAPIC timer is armed the boot context stops being scheduled while any
    // task is Ready (pre-existing behavior) — boot-time work after that point
    // silently never ran.
    input::init();

    // --- M9.6-A2: APIC interrupt architecture. Moves IRQ delivery from the
    // legacy 8259 PIC to LAPIC+IOAPIC (EOI = one MMIO write, no mutex) and
    // preemption to a 1000 Hz LAPIC timer. The PIT keeps counting uptime
    // ticks for rtc.rs/the ticker; the PIC ends up fully masked. The
    // closed-loop interval correction is handed to a kernel task by `init`
    // (it needs to keep running after the boot context loses the CPU).
    apic::init();

    // Boot work is done: park the boot context as a scheduler idle context.
    // It is picked whenever no task is Ready, and the diagnostic heartbeat
    // that proves interrupt liveness lives in `diag_task` (a real task) for
    // exactly the reason above.
    scheduler::idle_forever()
}

/// Diagnostic heartbeat (M9.8: kept as a task so it is always scheduled).
/// Proves interrupt delivery liveness: `ms` = LAPIC timer ticks (preemption
/// alive), `pit` = PIT ticks through the IOAPIC (uptime clock alive), `esr` =
/// LAPIC error status.
fn diag_task() {
    loop {
        serial_writeln!(
            "[diag] ms={} pit={} task={} cpu={} esr={:#x}",
            apic::ms_since_boot(),
            pit::ticks(),
            scheduler::current_task_id(),
            crate::smp::cpu_index(),
            apic::error_status()
        );
        scheduler::sleep_kernel(2000);
    }
}

/// Ticker task: reports the PIT tick count every 0.5 s. Demonstrates that the
/// timer is running and that this task is being preempted and resumed.
fn ticker_task() {
    // Print each tick-multiple only once (the busy loop would otherwise
    // reprint the same value hundreds of times per 10 ms window).
    let mut last_printed = u64::MAX;
    loop {
        let t = pit::ticks();
        if t % 25 == 0 && t != last_printed {
            last_printed = t;
            serial_writeln!("[ticker] t={}", t);
        }
    }
}

/// Keyboard reader task: M8 removed — the shell is the keyboard consumer now
/// (SYS_READ(0) -> keyboard::take_line). `keyboard::run_reader` is kept as a
/// debugging aid.
#[allow(dead_code)]
fn keyboard_task() {
    keyboard::run_reader();
}

/// Mouse reader task: prints movement/button events the IRQ handler queues.
fn mouse_task() {
    mouse::run_reader();
}

/// Header-bar clock task: redraws the wall clock once per second (RTC boot
/// time advanced by PIT uptime). The draw itself runs with IF=0 inside
/// `framebuffer::draw_clock`, so it cannot race the mouse IRQ.
fn clock_task() {
    let mut last = u32::MAX;
    let mut last_dec = u32::MAX;
    let mut logged = false;
    loop {
        let sod = rtc::now_secs_of_day();
        if sod != last {
            last = sod;
            framebuffer::draw_clock(&rtc::format_hms(sod));
            if !logged {
                logged = true;
                serial_writeln!("clock: {} — updating every second", rtc::format_hms(sod));
            }
            // Serial heartbeat: proves the IST clock advances on the log
            // (one line per 10 s — verifiable from the test scripts).
            if sod / 10 != last_dec {
                last_dec = sod / 10;
                serial_writeln!(
                    "[clock] IST {} console_bytes={}",
                    rtc::format_hms(sod),
                    framebuffer::console_bytes_total()
                );
            }
        }
        x86_64::instructions::hlt();
    }
}

/// FPU/SSE corruption regression test: two tasks keep live XMM accumulators
/// across preemptions and verify exact checkpoints. All values stay below
/// 2^24 (f32's exact-integer range), so every comparison is exact — a single
/// lost/corrupted XMM register shows up as a mismatch, not as noise. The
/// integer `iters` counter cross-checks that the loop really completed.
fn fpu_test_task(tag: char, seed: f32) {
    // 10 × 400k × 4 adds ≈ 16M SSE ops: under QEMU TCG this is a few seconds
    // per task while every 400k-iteration checkpoint spans many 10 ms
    // preemptions. Peak value ≈ 4e6 < 2^24 → every f32 add is exact.
    const CHECKPOINTS: u32 = 10;
    const PER_CKPT: u32 = 400_000;
    let mut acc = seed;
    let mut iters: u64 = 0;
    for ck in 0..CHECKPOINTS {
        // Four independent accumulators: LLVM keeps them in XMM registers
        // (scalar or vectorized), live across the whole checkpoint loop.
        let (mut a0, mut a1, mut a2, mut a3) = (acc, acc + 1.0, acc + 2.0, acc + 3.0);
        for _ in 0..PER_CKPT {
            a0 += 1.0;
            a1 += 1.0;
            a2 += 1.0;
            a3 += 1.0;
            iters += 1;
        }
        let expect = acc + PER_CKPT as f32;
        if a0 != expect
            || a1 != expect + 1.0
            || a2 != expect + 2.0
            || a3 != expect + 3.0
            || iters != (ck as u64 + 1) * PER_CKPT as u64
        {
            serial_writeln!(
                "fpu-test: task {} FAILED at checkpoint {} (FPU/SSE state corrupted)",
                tag,
                ck
            );
            scheduler::exit_current();
        }
        acc = a0;
    }
    serial_writeln!(
        "fpu-test: task {} PASSED ({} checkpoints, FPU/SSE state intact)",
        tag,
        CHECKPOINTS
    );
    scheduler::exit_current();
}

fn fpu_test_a() {
    fpu_test_task('A', 1000.0);
}

fn fpu_test_b() {
    fpu_test_task('B', 2000.0);
}

/// A3 regression: an Idle-priority task that sleeps 500 ms at a time. The
/// `[sched] sleeper woke ms=N` markers must land on a ~500 ms cadence (proves
/// the sleep queue + LAPIC-timer wake path).
fn sched_sleeper() {
    let mut n = 0;
    loop {
        scheduler::sleep_kernel(500);
        n += 1;
        serial_writeln!(
            "[sched] sleeper woke n={} ms={}",
            n,
            crate::apic::ms_since_boot()
        );
    }
}

/// Busy-wait for `ms` milliseconds using the TSC ns clock.
fn spin_ms(ms: u64) {
    let start = crate::time::now_ns();
    while crate::time::now_ns() - start < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

/// A3 regression: an RT-priority task that does two CPU bursts with `spin_ms`
/// gaps. No Normal-priority task may interleave between `rt start` and
/// `rt end` (the test greps for that). The RT task then exits.
fn sched_rt_burst() {
    // Burst 1: 300 ms of CPU hogging; normal tasks (ticker, clock, ...) must
    // NOT run in [rt start, rt end].
    serial_writeln!("[sched] rt start");
    spin_ms(300);
    serial_writeln!("[sched] rt end (no normal task should have run between)");
    // Pause a bit so the sleeper can tick, then a second short burst.
    scheduler::sleep_kernel(200);
    serial_writeln!("[sched] rt start2");
    spin_ms(100);
    serial_writeln!("[sched] rt end2");
    serial_writeln!("[sched] rt burst PASSED");
    scheduler::exit_current();
}

/// A5 regression: block-cache correctness + measurable speedup. Runs with
/// interrupts DISABLED for its whole body — the FS/DISK/cache spin-lock
/// discipline requires that no one holds these locks across a preemption.
// --- M9.6-B3: process lifecycle regression ---------------------------------
// Children of b3_test exercise the full lifecycle: exit-status recording,
// blocked-waitpid wake-up, kill-to-zombie (137), and orphan auto-reaping.
fn b3_child_exit42() {
    serial_writeln!("[b3] childA alive");
    scheduler::sleep_kernel(150);
    scheduler::exit_current_code(42);
}

fn b3_child_exit7() {
    scheduler::exit_current_code(7);
}

fn b3_child_loop() {
    loop {
        x86_64::instructions::hlt();
    }
}

fn b3_test() {
    serial_writeln!("[b3] test task alive");
    let me = scheduler::current_task_id();

    // 1. waitpid: block on a child that exits with code 42; the scheduler's
    //    wake pass must move this (Blocked) task back to Ready when it dies.
    let ca = scheduler::spawn_with_parent(b3_child_exit42, scheduler::PRIO_NORMAL, me);
    scheduler::block_on_child();
    scheduler::preempt();
    let mut got42 = false;
    if let Some((cpid, code)) = scheduler::try_reap(ca) {
        got42 = cpid == ca && code == 42;
    } else {
        // Bounded fallback poll — must never be needed if the wake pass works.
        for _ in 0..100 {
            scheduler::sleep_kernel(10);
            if let Some((cpid, code)) = scheduler::try_reap(ca) {
                got42 = cpid == ca && code == 42;
                break;
            }
        }
    }
    serial_writeln!("[b3] waitpid code=42 {}", if got42 { "OK" } else { "BAD" });

    // 2. kill: a looping child becomes a zombie with exit code 137.
    let cb = scheduler::spawn_with_parent(b3_child_loop, scheduler::PRIO_NORMAL, me);
    let killed = scheduler::kill(cb);
    let k137 = scheduler::try_reap(cb)
        .map(|(cpid, code)| cpid == cb && code == 137)
        .unwrap_or(false);
    serial_writeln!(
        "[b3] kill code=137 {}",
        if killed && k137 { "OK" } else { "BAD" }
    );

    // 3. orphan auto-reap: a child whose parent id does not exist must be
    //    reaped by the wake pass without anyone ever waiting on it.
    //    Normal priority: an IDLE-class child would be starved indefinitely
    //    by the Normal-priority ticker (by design), so the zombie would
    //    never even come to exist for the wake pass to reap.
    let cc = scheduler::spawn_with_parent(b3_child_exit7, scheduler::PRIO_NORMAL, 0xFFFF_FFFF);
    // 300 ms: an IDLE-priority task legitimately runs only when no Normal
    // task is Ready, so give it ample time to run, exit, and be reaped.
    scheduler::sleep_kernel(300);
    serial_writeln!(
        "[b3] orphan auto-reap {}",
        if scheduler::is_reaped(cc) { "OK" } else { "BAD" }
    );

    scheduler::exit_current_code(0);
}

/// M9.6-B5: one full performance snapshot ~3 s into the boot, then exit.
/// Gives every test run a stable latency/throughput baseline on serial; the
/// shell's `perf` command (SYS_PERF) renders the same snapshot live.
fn perf_task() {
    scheduler::sleep_kernel(3000);
    serial_writeln!("[perf] --- boot snapshot @ ~3 s ---");
    crate::perf::print_snapshot();
    scheduler::exit_current_code(0);
}

/// B4 test child: sleeps then pushes a synthetic raw key event so the blocked
/// `input_test` parent woke through the real scheduler wake path (not a poll).
fn input_child_pusher() {
    scheduler::sleep_kernel(150);
    crate::input::push_key(0x1E /* KeyCode::A */, true);
    scheduler::exit_current_code(0);
}

/// M9.6-B4 regression: raw input ring wire layout, drop-on-full behavior, and
/// the scheduler's block-on-raw wake path. Each check prints OK/BAD so the
/// test script can assert every stage; `input_test` runs as an ordinary kernel
/// task so all icalls go through the same code paths the compositor will use..
fn input_test() {
    // --- 1. Direct wire-layout check -------------------------------------
    // Clear any boot input, then push a known mouse + key event and drain them
    // back; verify every field against hand-decoded little-endian bytes (using
    // plain integer ops, NOT the module's own constants, so a wrong EV_SIZE or
    // encoding can't pass by symmetry).
    crate::input::clear();
    crate::input::push_mouse(-123, 45, crate::input::BTN_LEFT | crate::input::BTN_RIGHT);
    crate::input::push_key(0x1E /* A */, false);
    let mut buf = [0u8; 48];
    let n = crate::input::drain(&mut buf);
    // n must be 48 (two full 24-byte events).
    let size_ok = n == crate::input::EV_SIZE * 2;
    let kind0 = buf[0];
    let flags0 = buf[1];
    let code0 = (buf[2] as u16) | ((buf[3] as u16) << 8);
    let x0 = (buf[4] as i32) | ((buf[5] as i32) << 8) | ((buf[6] as i32) << 16) | ((buf[7] as i32) << 24);
    let y0 = (buf[8] as i32) | ((buf[9] as i32) << 8) | ((buf[10] as i32) << 16) | ((buf[11] as i32) << 24);
    let ts0 = (buf[16] as u64
        | ((buf[17] as u64) << 8)
        | ((buf[18] as u64) << 16)
        | ((buf[19] as u64) << 24)
        | ((buf[20] as u64) << 32)
        | ((buf[21] as u64) << 40)
        | ((buf[22] as u64) << 48)
        | ((buf[23] as u64) << 56));
    let m_ok = size_ok
        && kind0 == crate::input::EV_KIND_MOUSE
        && flags0 == (crate::input::BTN_LEFT | crate::input::BTN_RIGHT)
        && code0 == 0
        && x0 == -123
        && y0 == 45
        && ts0 != 0;
     // Second record: key-up A.
    let k1 = crate::input::EV_SIZE;
    let kind1 = buf[k1];
    let flags1 = buf[k1 + 1];
    let code1 = (buf[k1 + 2] as u16) | ((buf[k1 + 3] as u16) << 8);
    let k_ok = kind1 == crate::input::EV_KIND_KEY
        && flags1 == crate::input::KEY_UP
        && code1 == 0x1E
        && crate::input::count() == 0;
    serial_writeln!(
        "[b4] wire layout: size={} mouse x/y=({},{}) code={} up_code={} ts0={}",
        if size_ok { "24B ok" } else { "BAD" },
        x0,
        y0,
        code0,
        code1,
        ts0
    );
    serial_writeln!(
        "[b4] layout checks: mouse={} key={}",
        if m_ok && size_ok { "OK" } else { "BAD" },
        if k_ok { "OK" } else { "BAD" }
    );
 
    // --- 2. Drop-on-full: push 300 events into a 256-slot ring -----------
    // New events must drop, the oldest 256 stay, non-zero dropped counter.

    let before_dropped = crate::input::dropped();
    for i in 0..300 {
        crate::input::push_key((i % 256) as u16, true);
    }
    let kept = crate::input::count();
    let dropped_delta = crate::input::dropped() - before_dropped;
    let drop_ok = kept == 256 && dropped_delta == 44 && crate::input::total() > 0;
    serial_writeln!(
        "[b4] drop-on-full: kept={} dropped_delta={} {}",
        kept,
        dropped_delta,
        if drop_ok { "OK" } else { "BAD" }
    );
    crate::input::clear();
 
    // --- 3. Block-on-raw wake: a child pushes an event after 150 ms ---
    // the parent blocks (Blocked, blocked_on_raw=true); the scheduler's wake
    // pass must move it back to Ready when `input::pending()` turns true.\
    let me = scheduler::current_task_id();
    let child = scheduler::spawn_with_parent(input_child_pusher, scheduler::PRIO_NORMAL, me);
    scheduler::block_current_on_raw_input();
    scheduler::preempt();
    // Woken: drain and verify one K-down A event arrived (and its child id is
    // reap-able with the 150 ms sleep proving the wake was event-driven, not
    // a fixed timer coincidence).
    let mut wbuf = [0u8; 24];
    let wn = crate::input::drain(&mut wbuf);
    let wake_ok = wn == crate::input::EV_SIZE
        && wbuf[0] == crate::input::EV_KIND_KEY
        && (wbuf[2] as u16 | (wbuf[3] as u16) << 8) == 0x1E;
    serial_writeln!(
        "[b4] block-on-raw wake: n={} {}",
        wn,
        if wake_ok { "OK" } else { "BAD" }
    );
    let _ = scheduler::try_reap(child);
 
    scheduler::exit_current_code(0);
}
fn blk_test() {
    x86_64::instructions::interrupts::disable();
    let mut buf = [0u8; 16384];
    let mut buf2 = [0u8; 16384];

    // --- Cold vs hot read of /SHELL.ELF (FAT32, ~19 KB) ---
    crate::blkcache::flush();
    crate::blkcache::reset_stats();
    let t0 = crate::time::now_ns();
    let cold = vfs::read_at("/SHELL.ELF", 0, &mut buf);
    let t1 = crate::time::now_ns();
    let cold_ns = t1.saturating_sub(t0);
    let cold_hits_before = crate::blkcache::stats().0;

    let t2 = crate::time::now_ns();
    let hot = vfs::read_at("/SHELL.ELF", 0, &mut buf2);
    let t3 = crate::time::now_ns();
    let hot_ns = t3.saturating_sub(t2);
    let (hits_after, misses_after) = crate::blkcache::stats();

    let data_ok = match (&cold, &hot) {
        (Ok(a), Ok(b)) => a == b && *a > 0 && buf[..*a] == buf2[..*b],
        _ => false,
    };
    let speedup = if hot_ns > 0 && cold_ns > hot_ns {
        (cold_ns * 100) / hot_ns
    } else {
        0
    };
    serial_writeln!(
        "[blk] cold read /SHELL.ELF: {cold_ns} ns ({} bytes) | hot: {hot_ns} ns | speedup x{speedup}",
        cold.as_ref().copied().unwrap_or(0)
    );
    serial_writeln!(
        "[blk] data equality: {}",
        if data_ok { "PASSED" } else { "FAILED" }
    );
    serial_writeln!(
        "[blk] read-ahead during cold: {} (hits before any re-read = {cold_hits_before})",
        if cold_hits_before > 0 { "YES" } else { "NO" }
    );
    serial_writeln!(
        "[blk] cache stats: hits={hits_after} misses={misses_after} (hot hit ratio {})",
        if misses_after > 0 {
            hits_after.saturating_sub(cold_hits_before) * 100
                / (hits_after.saturating_sub(cold_hits_before) + misses_after).max(1)
        } else {
            100
        }
    );

    // --- Write-through coherence: write /BLCK.TXT, read via cache, flush,
    // read again cold — both must match the original bytes. ---
    const MSG: &[u8] = b"block-cache write-through\n";
    let wok = vfs::write_at("/BLCK.TXT", 0, MSG).is_ok();
    let mut rbuf = [0u8; 64];
    let rok = match vfs::read_at("/BLCK.TXT", 0, &mut rbuf) {
        Ok(n) => n == MSG.len() && rbuf[..n] == MSG[..],
        Err(_) => false,
    };
    crate::blkcache::flush(); // drop the cache; re-read must land on disk bytes
    let mut rbuf2 = [0u8; 64];
    let rok2 = match vfs::read_at("/BLCK.TXT", 0, &mut rbuf2) {
        Ok(n) => n == MSG.len() && rbuf2[..n] == MSG[..],
        Err(_) => false,
    };
    serial_writeln!(
        "[blk] write-through: {}",
        if wok && rok && rok2 { "PASSED" } else { "FAILED" }
    );

    // --- Deterministic read-ahead probe: two contiguous cached reads must
    // leave READ_AHEAD sectors past the end cached. ---
    let ra_ok = crate::block::with_disk(|disk| {
        crate::blkcache::flush();
        crate::blkcache::reset_stats();
        let base = 8192u32;
        let mut b1 = [0u8; 2048];
        let mut b2 = [0u8; 2048];
        let r1 = disk.drive.read_sectors(base, 4, &mut b1);
        let r2 = disk.drive.read_sectors(base + 4, 4, &mut b2);
        r1.is_ok()
            && r2.is_ok()
            && (0..crate::blkcache::READ_AHEAD as u32)
                .all(|k| crate::blkcache::is_cached(base + 8 + k))
    })
    .unwrap_or(false);
    serial_writeln!(
        "[blk] read-ahead probe: {}",
        if ra_ok { "PASSED" } else { "FAILED" }
    );

    x86_64::instructions::interrupts::enable();
    scheduler::exit_current();
}

/// M2 test 1: allocate on the kernel heap using `Box` and `Vec`.
fn heap_allocation_test() {
    let boxed = Box::new(41u64);
    let mut vec = Vec::new();
    for i in 0..1000u64 {
        vec.push(i);
    }
    serial_writeln!(
        "heap test: box={}, vec.len()={}, vec[999]={}",
        *boxed,
        vec.len(),
        vec[999]
    );
    assert_eq!(*boxed, 41);
    assert_eq!(vec.len(), 1000);
    serial_writeln!("heap allocation test PASSED");
}

/// M2 test 2: map a fresh page with the mapper and prove we can write to it.
fn paging_test(
    mapper: &mut (impl Mapper<Size4KiB> + Translate),
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) {
    // Map the page immediately after the kernel heap.
    let page_addr = VirtAddr::new((allocator::HEAP_START + allocator::HEAP_SIZE) as u64);
    let page = Page::containing_address(page_addr);
    let frame = frame_allocator.allocate_frame().expect("no frame available");
    serial_writeln!(
        "paging test: mapping page {:#x} -> frame {:#x}",
        page.start_address().as_u64(),
        frame.start_address().as_u64()
    );
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    unsafe {
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .expect("map_to failed")
            .flush();
    }

    // Write a marker through the mapping and read it back.
    let ptr = page.start_address().as_mut_ptr::<u64>();
    unsafe {
        ptr.write(0xDEAD_BEEF);
    }
    let read_back = unsafe { ptr.read() };
    let translated = mapper
        .translate_addr(page.start_address())
        .expect("new page not mapped");

    serial_writeln!(
        "paging test: wrote {:#x}, read back {:#x}, translate -> {:#x}",
        0xDEAD_BEEFu64,
        read_back,
        translated.as_u64()
    );
    assert_eq!(read_back, 0xDEAD_BEEF);
    assert_eq!(translated, frame.start_address());
    serial_writeln!("paging test PASSED");
}

/// M9.6-A6: frame-allocator v2 regression. Exercises the post-boot global
/// allocator through `with_global_frames` (IF=0 by contract): fresh-alloc
/// distinctness (bump path), free/realloc set identity + exact LIFO order
/// (free-list path), stats consistency, and net-outstanding recovery.
fn mem_test() {
    use x86_64::structures::paging::PhysFrame;

    const N: usize = 256;
    // The runtime snapshot is installed at M8, after the boot-path spawns —
    // poll for it instead of racing the boot sequence.
    let mut installed = false;
    for _ in 0..100 {
        if memory::frame_stats().is_some() {
            installed = true;
            break;
        }
        scheduler::sleep_kernel(100);
    }
    if !installed {
        serial_writeln!("[mem] FAIL: global frames not installed after 10 s");
        scheduler::exit_current();
    }

    let ok = memory::with_global_frames(|f| {
        let (u0, total, freed0, oom0) = f.stats();
        serial_writeln!(
            "[mem] frames: used={} total={} free={} freed={} oom={}",
            u0,
            total,
            total - u0,
            freed0,
            oom0
        );
        if total == 0 {
            serial_writeln!("[mem] FAIL: no usable frames in the memory map");
            return false;
        }

        // 1. N fresh allocations must all be distinct (bump path).
        let mut fresh: alloc::vec::Vec<PhysFrame> = alloc::vec::Vec::with_capacity(N);
        for _ in 0..N {
            match f.allocate_frame() {
                Some(fr) => fresh.push(fr),
                None => {
                    serial_writeln!("[mem] FAIL: OOM at {}/{} fresh allocs", fresh.len(), N);
                    return false;
                }
            }
        }
        let mut addrs: alloc::vec::Vec<u64> =
            fresh.iter().map(|fr| fr.start_address().as_u64()).collect();
        addrs.sort_unstable();
        for w in addrs.windows(2) {
            if w[0] == w[1] {
                serial_writeln!("[mem] FAIL: duplicate frame handed out");
                return false;
            }
        }
        serial_writeln!("[mem] {} fresh allocs, all distinct", N);

        // 2. Free everything, re-allocate: the free list must return the
        //    exact same frame set.
        for fr in fresh.iter().rev() {
            unsafe { f.deallocate_frame(*fr) };
        }
        let mut again: alloc::vec::Vec<PhysFrame> = alloc::vec::Vec::with_capacity(N);
        for _ in 0..N {
            match f.allocate_frame() {
                Some(fr) => again.push(fr),
                None => {
                    serial_writeln!("[mem] FAIL: OOM on realloc");
                    return false;
                }
            }
        }
        let mut back: alloc::vec::Vec<u64> =
            again.iter().map(|fr| fr.start_address().as_u64()).collect();
        back.sort_unstable();
        if back != addrs {
            serial_writeln!("[mem] FAIL: realloc frame set differs from freed set");
            return false;
        }
        serial_writeln!("[mem] free + realloc returns the same frame set");

        // 3. Exact LIFO order: free a then b, alloc c then d => c==b, d==a.
        let a = again[0];
        let b = again[1];
        unsafe {
            f.deallocate_frame(a);
            f.deallocate_frame(b);
        }
        let c = match f.allocate_frame() {
            Some(fr) => fr,
            None => {
                serial_writeln!("[mem] FAIL: OOM on LIFO probe");
                return false;
            }
        };
        let d = match f.allocate_frame() {
            Some(fr) => fr,
            None => {
                serial_writeln!("[mem] FAIL: OOM on LIFO probe");
                return false;
            }
        };
        if c != b || d != a {
            serial_writeln!("[mem] FAIL: free list not LIFO");
            return false;
        }
        serial_writeln!("[mem] LIFO reuse order exact");

        // 4. Stats: N frames outstanding (again[2..] + c + d), and the free
        //    counter advanced by N (step 2) + 2 (step 3).
        let (u1, _t, freed1, _o1) = f.stats();
        if u1 != u0 + N as u64 || freed1 != freed0 + N as u64 + 2 {
            serial_writeln!(
                "[mem] FAIL: stats used={} (want {}) freed={} (want {})",
                u1,
                u0 + N as u64,
                freed1,
                freed0 + N as u64 + 2
            );
            return false;
        }

        // 5. Free everything: net outstanding returns to the baseline.
        for fr in again.iter().skip(2) {
            unsafe { f.deallocate_frame(*fr) };
        }
        unsafe {
            f.deallocate_frame(c);
            f.deallocate_frame(d);
        }
        let (u2, _t2, _freed2, _oom2) = f.stats();
        if u2 != u0 {
            serial_writeln!("[mem] FAIL: net outstanding {} != baseline {}", u2, u0);
            return false;
        }
        serial_writeln!("[mem] net outstanding back to baseline after churn");
        true
    });

    match ok {
        Some(true) => serial_writeln!("[mem] A6 frame-alloc v2 PASSED"),
        Some(false) => serial_writeln!("[mem] A6 frame-alloc v2 FAILED"),
        None => serial_writeln!("[mem] FAIL: global frames not installed"),
    }
    scheduler::exit_current();
}

/// Called on panic; print the message over serial, then halt.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial::init();
    serial_writeln!("PANIC: {info}");
    loop {
        x86_64::instructions::hlt();
    }
}

