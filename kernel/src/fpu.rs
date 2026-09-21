//! FPU/SIMD (x87 + SSE [+ AVX]) state management.
//!
//! Every task owns one per-task state image that the scheduler's naked
//! `context_switch` swaps on every preemption — eager, not lazy: at 100 Hz
//! preemption the save/restore pair costs a few hundred cycles per switch and
//! needs no `#NM` trickery (the `#NM` handler stays a "must never happen"
//! diagnostic).
//!
//! Two code paths, chosen ONCE at boot from CPUID:
//!
//!   * **XSAVE** (`xsave64`/`xrstor64`, M9.8/f): used when the CPU has
//!     XSAVE + OSXSAVE. `CR4.OSXSAVE` is set and `XCR0` is programmed to
//!     x87|SSE (+AVX when both `CPUID.1.ECX.AVX` and `CPUID.0Dh.0.EAX.YMM`
//!     allow it). This is what makes AVX/YMM state survive a context switch.
//!     The image size comes from `CPUID.0Dh.0.EBX` for exactly that XCR0 —
//!     never a guess.
//!   * **FXSAVE** (`fxsave`/`fxrstor`): the original M9.6 path, kept as the
//!     fallback for CPUs without XSAVE. 512-byte image, 16-byte alignment.
//!
//! `XCR0` is per-CPU hardware state, so every AP programs it in [`init_ap`]
//! (mirroring the BSP's decision) *before* it is handed any task.
//!
//! Why this exists: kernel AND ring-3 code is compiled for baseline x86_64
//! (SSE2), so every task may keep live values in XMM registers across a
//! preemption. Without save/restore, the next task silently corrupts them —
//! a compile-time-invisible data-corruption bug.
//!
//! Fresh areas are seeded from `TEMPLATE` (captured right after `fninit`):
//! an all-zero area would make the restore unmask every FP exception
//! (FCW=0, MXCSR=0) and crash the first SSE instruction with #XM.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Capacity of a per-task state image, 64-byte aligned.
///
/// XCR0 is only ever programmed to x87|SSE|AVX (bits 0..2), whose XSAVE area
/// ends at byte 832 (the AVX `YMM_Hi128` component occupies 576..832); 2048
/// leaves headroom. The XSAVE path is *refused* at boot when
/// `CPUID.0Dh.0.EBX` reports a bigger requirement than this, so `xsave` can
/// never write past the buffer. 64-byte alignment is required by
/// `xsave64`/`xrstor64` (`fxsave`/`fxrstor` only need 16).
pub const AREA_CAPACITY: usize = 2048;

/// FXSAVE/XSAVE image for one task: 64-byte aligned, [`AREA_CAPACITY`] bytes.
#[repr(C, align(64))]
pub struct FpuArea {
    bytes: [u8; AREA_CAPACITY],
}

impl FpuArea {
    pub const fn zeroed() -> Self {
        Self {
            bytes: [0; AREA_CAPACITY],
        }
    }
}

/// Valid image, captured immediately after `fninit` in [`init`] with the
/// instruction pair boot selected.
static mut TEMPLATE: FpuArea = FpuArea::zeroed();

/// XSAVE is live machine-wide (CR4.OSXSAVE + XCR0 programmed on every online
/// CPU). Decides which `context_switch` body the scheduler calls.
static XSAVE_ENABLED: AtomicBool = AtomicBool::new(false);

/// AVX (YMM) is usable: XSAVE live *and* XCR0 bit 2 set.
static AVX_ENABLED: AtomicBool = AtomicBool::new(false);

/// Bytes one save writes / one restore reads (`CPUID.0Dh.0.EBX` on the XSAVE
/// path, 512 on the FXSAVE path).
static AREA_SIZE: AtomicUsize = AtomicUsize::new(512);

/// The `XCR0` value that was programmed (0 on the FXSAVE path).
static XCR0_VALUE: AtomicUsize = AtomicUsize::new(0);

/// Number of CPUs that programmed XCR0 (observability for the tests).
static XSAVE_CPUS: AtomicUsize = AtomicUsize::new(0);

/// XSAVE is available and in use on this machine.
pub fn xsave_enabled() -> bool {
    XSAVE_ENABLED.load(Ordering::Relaxed)
}

/// AVX/YMM state is being saved and restored (XSAVE with XCR0 bit 2).
pub fn avx_enabled() -> bool {
    AVX_ENABLED.load(Ordering::Relaxed)
}

/// Bytes `xsave` writes per task image (512 on the FXSAVE fallback).
pub fn area_size() -> usize {
    AREA_SIZE.load(Ordering::Relaxed)
}

/// How many CPUs have XCR0 programmed.
pub fn xsave_cpu_count() -> usize {
    XSAVE_CPUS.load(Ordering::Relaxed)
}

/// The programmed `XCR0` (0 when XSAVE is off).
pub fn xcr0() -> usize {
    XCR0_VALUE.load(Ordering::Relaxed)
}

/// This CPU's *live* `XCR0` (`xgetbv(0)`), or 0 when XSAVE is off.
///
/// `XCR0` is per-CPU hardware state (M9.8-f), so a value that differs between
/// cores would make a migrated task's image inconsistent: the CPU with `YMM`
/// disabled saves/restores only x87+SSE, and an image saved there makes a later
/// `xrstor` on a YMM-enabled core *initialize* the AVX upper halves to zero.
/// Tests print this per round for exactly that reason.
pub fn live_xcr0() -> usize {
    if !xsave_enabled() {
        return 0;
    }
    unsafe { xgetbv(0) as usize }
}

// ---------------------------------------------------------------------------
// CPUID / XCR0 plumbing
// ---------------------------------------------------------------------------

/// Raw CPUID (leaf in EAX, subleaf in ECX). EBX is reserved by LLVM, so it is
/// saved/restored around the instruction via r10 (same trick as `time::cpuid`).
unsafe fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov r10d, ebx",
            "pop rbx",
            inlateout("eax") leaf => eax,
            inlateout("ecx") sub => ecx,
            lateout("r10d") ebx,
            lateout("edx") edx,
            options(nostack, preserves_flags)
        );
    }
    (eax, ebx, ecx, edx)
}

/// `xgetbv` — read an extended control register.
unsafe fn xgetbv(index: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "xgetbv",
            in("ecx") index,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

/// `xsetbv` — write an extended control register (requires CR4.OSXSAVE=1 and
/// CPL 0; it `#GP`s if any bit outside `CPUID.0Dh.0`'s supported mask is set,
/// which is why the caller masks `XCR0` with that value first).
unsafe fn xsetbv(index: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "xsetbv",
            in("ecx") index,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// What this CPU can do for XSAVE, decided from CPUID only (no writes).
struct XsavePlan {
    /// x87|SSE (+AVX) bits this CPU's `CPUID.0Dh.0` allows.
    xcr0: u64,
    /// Bytes this XCR0 needs.
    size: usize,
    /// YMM (bit 2) included.
    avx: bool,
}

/// Probe CPUID for the XSAVE/AVX plan, or `None` when this CPU has no XSAVE —
/// or would need a bigger state image than [`AREA_CAPACITY`], in which case the
/// FXSAVE path is used instead of risking an overrun.
///
/// Two CPUID subtleties this encodes (both cost a boot each to find):
///   * `CPUID.1.ECX.OSXSAVE` (bit 27) is a **read-only reflection of
///     `CR4.OSXSAVE`**, so it reads 0 until *we* set the bit — it cannot be
///     used as an availability test. Only bit 26 (XSAVE) says the CPU has the
///     instructions.
///   * `CPUID.0Dh.0.EBX` ("size required") is computed from the XCR0 *currently
///     programmed* (0 at boot → 576 bytes, header + legacy region only), so the
///     requirement must be derived from the per-component `CPUID.0Dh.<i>`
///     offset/size pairs for the bits we intend to enable.
fn plan() -> Option<XsavePlan> {
    /// x87 component (legacy region + 64-byte XSAVE header).
    const LEGACY_AND_HEADER: usize = 512 + 64;
    const X87: u64 = 1 << 0;
    const SSE: u64 = 1 << 1;
    const YMM: u64 = 1 << 2; // AVX
    unsafe {
        let (_, _, c1, _) = cpuid(1, 0);
        let has_xsave = c1 & (1 << 26) != 0;
        let has_avx = c1 & (1 << 28) != 0;
        if !has_xsave {
            return None;
        }
        // Subleaf 0: EAX/EDX = supported XCR0 bits, EBX = size for the bits
        // currently enabled in XCR0, ECX = max size for all supported features.
        let (eax, _ebx, _ecx, edx) = cpuid(0x0D, 0);
        let supported = ((edx as u64) << 32) | eax as u64;
        let mut want = 0u64;
        if supported & X87 != 0 {
            want |= X87;
        }
        if supported & SSE != 0 {
            want |= SSE;
        }
        // AVX needs all three: CPUID.1.ECX.AVX, XCR0.YMM, and XCR0.SSE
        // (YMM without SSE is an invalid XCR0 image and `xsetbv` #GPs).
        let mut avx = has_avx && supported & YMM != 0 && want & SSE != 0;
        if want & SSE == 0 {
            return None; // no SSE state to save: not a usable XSAVE target
        }
        let mut size = LEGACY_AND_HEADER;
        if avx {
            // Subleaf 2 = the AVX (YMM_Hi128) component: EAX = size,
            // EBX = offset within the state image. The requirement is the end
            // of the last enabled component.
            let (csize, coff, _, _) = cpuid(0x0D, 2);
            if csize == 0 {
                avx = false; // no YMM component after all
            } else {
                size = size.max(coff as usize + csize as usize);
            }
        }
        if avx {
            want |= YMM;
        }
        if size > AREA_CAPACITY {
            crate::serial_writeln!(
                "fpu: XSAVE image would need {size} bytes (> {AREA_CAPACITY} capacity) - FXSAVE fallback"
            );
            return None;
        }
        Some(XsavePlan {
            xcr0: want,
            size,
            avx,
        })
    }
}

/// Program CR4.OSXSAVE + XCR0 on the CALLING CPU and return its plan.
/// Called once per CPU (`init` on the BSP, `init_ap` on every AP).
///
/// # Safety
/// CPL 0, during single-threaded per-CPU bring-up.
unsafe fn enable_on_this_cpu() -> Option<XsavePlan> {
    use x86_64::registers::control::{Cr4, Cr4Flags};
    let plan = plan()?;
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::OSXSAVE);
    unsafe { Cr4::write(cr4) };
    // OSXSAVE is set, so `xsetbv` is legal now (and CPUID now reports
    // OSXSAVE=1 on this CPU).
    unsafe { xsetbv(0, plan.xcr0) };
    let got = unsafe { xgetbv(0) };
    let (_, _, c1, _) = unsafe { cpuid(1, 0) };
    if got != plan.xcr0 || c1 & (1 << 27) == 0 {
        // The CPU did not take the state we programmed. Rather than run tasks
        // with a state image whose size no longer matches `AREA_SIZE`, undo
        // OSXSAVE and stay on the FXSAVE path (self-consistent fallback).
        crate::serial_writeln!(
            "fpu: XCR0 readback mismatch (want {:#x}, got {got:#x}, OSXSAVE bit {}) - XSAVE disabled",
            plan.xcr0,
            if c1 & (1 << 27) != 0 { 1 } else { 0 }
        );
        cr4.remove(Cr4Flags::OSXSAVE);
        unsafe { Cr4::write(cr4) };
        return None;
    }
    XSAVE_CPUS.fetch_add(1, Ordering::Relaxed);
    Some(plan)
}

/// The control-register state both paths need, plus `fninit`: CR0 setup (SSE
/// usable, x87 native error reporting), CR4.OSFXSR, and a reset FPU with every
/// FP exception masked.
fn common_setup() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    // CR0: MP=1 (SSE instructions honor CR0.TS), EM=0 (no x87 emulation),
    // NE=1 (native x87 error reporting -> #MF, which has a handler).
    let mut cr0 = Cr0::read();
    cr0.remove(Cr0Flags::EMULATE_COPROCESSOR);
    cr0.insert(Cr0Flags::MONITOR_COPROCESSOR | Cr0Flags::NUMERIC_ERROR);
    unsafe { Cr0::write(cr0) };
    // CR4: OSFXSR=1 — the save/restore instructions cover XMM and SSE
    // instructions are usable.
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::OSFXSR);
    unsafe { Cr4::write(cr4) };
    // x87 + MXCSR back to power-up defaults (every exception masked).
    unsafe { core::arch::asm!("fninit") };
}

/// Async-interrupt SIMD guard (M9.8-f): saves the interrupted context's
/// x87/SSE state on construction and restores it on drop.
///
/// Why this exists — found by the AVX regression test, which failed with
/// *only the low 128 bits* of `ymm0` clobbered (all four lanes equal), while
/// the AVX upper halves survived:
///
///   An interrupt is delivered in the *task's* context, but the task's state
///   image is only written by `context_switch` — i.e. at the *end* of the
///   handler. Any compiler-generated SSE in the handler body (perf timing,
///   struct copies, memcpy-style moves) therefore overwrites the interrupted
///   task's XMM registers *before* they are saved, and the task resumes with
///   the handler's garbage. The AVX upper halves were untouched because the
///   kernel is compiled for baseline x86_64 (SSE2 only), which cannot touch
///   them — that asymmetry is what makes the bug recognisable in a log.
///
/// Scope: **asynchronous** interrupts only (timer, IPIs, device IRQs) — they
/// have no call-ABI contract with the interrupted code. Synchronous traps and
/// syscalls behave like a function call, and the SysV ABI already declares
/// XMM0-15 volatile across calls, so they need no guard.
///
/// `fxsave`/`fxrstor` (512 bytes) is sufficient here *because* kernel SIMD
/// code is SSE2-only: the IRQ path cannot modify the AVX/YMM extension state,
/// so restoring x87+SSE+x87-control is a complete repair. The per-task image
/// still uses XSAVE/XCRT0, which is what preserves a task's YMM values across
/// a switch.
///
/// The image lives on the handler's own stack frame, which travels with the
/// task: if the handler preempts to another task, this frame stays on the
/// interrupted task's kernel stack and its `fxrstor` runs when that task is
/// resumed — after `context_switch` already restored the (possibly clobbered)
/// image, so the guard's copy wins and repairs the state. 512 bytes of
/// transient stack per IRQ, and one instruction pair at ~1000 IRQ/s per CPU.
#[repr(C, align(16))]
pub struct IrqFpuGuard {
    area: [u8; 512],
}

impl IrqFpuGuard {
    /// Capture the current x87/SSE state. Make this the FIRST statement of an
    /// asynchronous interrupt handler.
    #[inline]
    pub fn new() -> Self {
        let mut guard = Self { area: [0u8; 512] };
        unsafe {
            core::arch::asm!(
                "fxsave [{}]",
                in(reg) guard.area.as_mut_ptr(),
                options(nostack, preserves_flags)
            );
        }
        guard
    }
}

impl Drop for IrqFpuGuard {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            core::arch::asm!(
                "fxrstor [{}]",
                in(reg) self.area.as_ptr(),
                options(nostack, preserves_flags)
            );
        }
    }
}

/// Save the calling context's FPU/SIMD state into `area` with the selected
/// instruction pair. Used for the template capture; the hot path is the
/// scheduler's naked `context_switch`.
pub fn save_into(area: *mut FpuArea) {
    unsafe {
        if xsave_enabled() {
            // EDX:EAX is the state-component mask operand for xsave64 — with
            // a garbage mask this would silently save only a subset (the same
            // bug class the context switch had; see scheduler.rs).
            let xcr0 = xcr0();
            core::arch::asm!(
                "mov eax, {low:e}",
                "xor edx, edx",
                "xsave64 [{ptr}]",
                low = in(reg) xcr0 as u64,
                ptr = in(reg) area,
                lateout("rax") _, lateout("rdx") _,
                options(nostack, preserves_flags)
            );
        } else {
            core::arch::asm!("fxsave [{}]", in(reg) area, options(nostack, preserves_flags));
        }
    }
}


/// Boot-time setup: control registers, XSAVE/AVX enablement when the CPU
/// supports it, and the template image every fresh task area starts from.
/// Call once, before any task is spawned.
pub fn init() {
    common_setup();
    match unsafe { enable_on_this_cpu() } {
        Some(p) => {
            XSAVE_ENABLED.store(true, Ordering::Relaxed);
            AVX_ENABLED.store(p.avx, Ordering::Relaxed);
            AREA_SIZE.store(p.size, Ordering::Relaxed);
            XCR0_VALUE.store(p.xcr0 as usize, Ordering::Relaxed);
            crate::serial_writeln!(
                "fpu: XSAVE enabled (XCR0={:#x}, area {} bytes, AVX={})",
                p.xcr0,
                p.size,
                if p.avx { "yes" } else { "no" }
            );
        }
        None => {
            XSAVE_ENABLED.store(false, Ordering::Relaxed);
            AVX_ENABLED.store(false, Ordering::Relaxed);
            AREA_SIZE.store(512, Ordering::Relaxed);
            XCR0_VALUE.store(0, Ordering::Relaxed);
            crate::serial_writeln!("fpu: FXSAVE fallback (no XSAVE/OSXSAVE on this CPU)");
        }
    }
    let template_ptr = core::ptr::addr_of_mut!(TEMPLATE).cast::<FpuArea>();
    save_into(template_ptr);
}

/// Per-CPU FPU/SIMD bring-up for an AP (M9.8): the same control-register
/// state `init` installs on the BSP, including **XCR0** — which is per-CPU
/// hardware state. An AP that never programmed it would take `#UD` on the
/// first AVX instruction and (worse) its `xrstor` would restore a state image
/// whose components this CPU considers disabled. No template capture: APs
/// seed their areas from the BSP's template.
pub fn init_ap() {
    common_setup();
    if xsave_enabled() {
        match unsafe { enable_on_this_cpu() } {
            Some(p) if p.size == area_size() => {}
            Some(p) => crate::serial_writeln!(
                "fpu: AP XSAVE image {} bytes but the BSP uses {} - cores not equivalent",
                p.size,
                area_size()
            ),
            None => crate::serial_writeln!(
                "fpu: AP could not enable XSAVE while the BSP has it on - this core must not run AVX tasks"
            ),
        }
    }
}

/// Allocate a fresh, template-seeded state image (64-aligned, 'static).
/// Called by the scheduler when a task is spawned (IF=0 context).
pub fn new_area() -> &'static mut FpuArea {
    unsafe {
        let mut area = Box::new(FpuArea::zeroed());
        let src = &*core::ptr::addr_of!(TEMPLATE);
        area.bytes = src.bytes;
        Box::leak(area)
    }
}

// ---------------------------------------------------------------------------
// AVX/YMM regression task (M9.8-f)
// ---------------------------------------------------------------------------

/// Adds per AVX round. Each round must comfortably outlive the 8-tick time
/// slice, otherwise it can finish before a single preemption and its result
/// says nothing about YMM preservation (that case reports `avx WEAK`).
/// Measured under TCG: ~2 ticks per 2M adds, so 20M adds ≈ 20 ticks alone.
const AVX_CHECK_ITERS: u64 = 20_000_000;

/// AVX/YMM context-switch regression task. Spawned only when [`avx_enabled`],
/// so its AVX instructions can never be executed on a CPU without AVX.
///
/// Two live 256-bit integer accumulators (`ymm0`/`ymm1`) are re-seeded and then
/// incremented with [`AVX_CHECK_ITERS`] exact 256-bit adds; the result is
/// verified lane by lane against scalar expectations. The point is not the
/// arithmetic but *when* it happens: each round takes far longer than the 8 ms
/// time slice, so the task is preempted many times with those YMM registers
/// live, and other tasks run in between (the SSE fpu tests, the shell, per-CPU
/// workers on other cores). With the old FXSAVE-only path the YMM upper halves
/// would be lost at the first switch and every following round would mismatch.
/// Each round also reports how many context switches happened inside it, so
/// "the test passed" cannot silently mean "the test never ran anything".
///
/// Why hand-written asm instead of `#[target_feature(enable = "avx")]`: on
/// `x86_64-unknown-none`, enabling `sse` (which `avx` implies) is a hard error
/// (`x86_soft_float_sse` / rust-lang#117938), and enabling AVX crate-wide is
/// not an option — the kernel must keep booting on CPUs without AVX (that is
/// what the FXSAVE fallback is for). The whole AVX loop therefore lives inside
/// ONE `asm!` block:
///   * the only AVX instructions in the kernel are these, executed only when
///     boot detected AVX/`XCR0.YMM`;
///   * the YMM registers stay live *inside* the block across many preemptions
///     (no compiler-generated code can touch them in between), so a context
///     switch that fails to preserve them is caught by the comparison.
///
/// Emits `fpu: avx PASS round N (… switches …)` on success, `fpu: avx FAILED …`
/// on mismatch, and `fpu: avx WEAK round N` if a round somehow saw no
/// preemption at all (which would make its result meaningless).
/// The task-table index of the AVX regression task, recorded by the task
/// itself on its first line (`scheduler::current_index()` in task context).
/// Used by the scheduler's switch tracer to log every context switch that
/// involves this task (AVX corruption debugging, M9.8-f).
pub static AVX_TASK_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);
/// When `true`, the scheduler logs every context switch involving the AVX
/// task (`[avxsw]` lines). Set by `avx_test_task` while it runs.
pub static AVX_TRACE: AtomicBool = AtomicBool::new(false);

pub fn avx_task_index() -> usize {
    AVX_TASK_INDEX.load(Ordering::Relaxed)
}

/// The calling task's saved-state image (its own `fpu_area`). The AVX test
/// reads the image's `XSTATE_BV` header on corruption to tell a bad *save*
/// (bit 2 clear at save time) from a bad *restore* (image fine, wrong area
/// loaded).
pub fn current_fpu_image_xstate_bv() -> u64 {
    let area = crate::scheduler::current_fpu_area();
    if area.is_null() {
        return 0;
    }
    // XSTATE_BV: first qword of the header, which starts right after the
    // 512-byte legacy region.
    unsafe { area.cast::<u64>().add(64).read_volatile() }
}

/// The AVX test calls this on entry to register itself with the scheduler's
/// switch tracer. The per-switch `[avxsw]` trace stays OFF by default (each
/// line costs ~9 ms of serial time); flip this `false` to `true` when
/// debugging YMM state loss again.
pub fn avx_register_task() {
    AVX_TASK_INDEX.store(crate::scheduler::current_index(), Ordering::Relaxed);
    AVX_TRACE.store(false, Ordering::Relaxed);
}

pub fn avx_test_task() {
    if !avx_enabled() {
        return;
    }
    avx_register_task();
    const ITERS: u64 = AVX_CHECK_ITERS;
    // Every round uses a *different* seed, so a failure tells us what was
    // restored: the seed identifies the snapshot (a state image from an earlier
    // round, or another task's image) instead of just "a number".
    let mut seed = 100i32;
    let one: [i32; 8] = [1; 8];
    // M9.8-f debug: these buffers live on the heap, not the kernel stack — a
    // first-pass run corrupted exactly the stack-resident copies under task
    // migration (upper YMM lanes zeroed in memory), so the heap copy isolates
    // "image/restore bug" from "someone else wrote to our kernel stack".
    let mut lanes0: &'static mut [i32; 8] = Box::leak(Box::new([0i32; 8]));
    let mut lanes1: &'static mut [i32; 8] = Box::leak(Box::new([0i32; 8]));
    let mut rounds: u64 = 0;
    loop {
        let cpu = crate::smp::cpu_index();
        let mut seed0 = [0i32; 8];
        let mut seed1 = [0i32; 8];
        for i in 0..8 {
            seed0[i] = seed + i as i32;
            seed1[i] = seed + 1000 + i as i32;
        }
        // ymm0 = seed0, ymm1 = seed1, ymm2 = 1 (all 8 lanes): then `ITERS`
        // iterations of two 256-bit adds. The pointers are handed to the asm as
        // plain GP registers; mnemonics are written out so the encoder is the
        // compiler's job.
        let sw0 = crate::scheduler::switches_this_cpu();
        let t0 = crate::pit::ticks();
        unsafe {
            core::arch::asm!(
                "vmovdqu ymm0, [{p0}]",
                "vmovdqu ymm1, [{p1}]",
                "vmovdqu ymm2, [{pone}]",
                "2:",
                "vpaddd ymm0, ymm0, ymm2",
                "vpaddd ymm1, ymm1, ymm2",
                "dec {n}",
                "jnz 2b",
                "vmovdqu [{o0}], ymm0",
                "vmovdqu [{o1}], ymm1",
                // Leave the upper halves clean before SSE-compiled code runs
                // again (avoids the AVX->SSE transition penalty on Intel).
                "vzeroupper",
                p0 = in(reg) seed0.as_ptr(),
                p1 = in(reg) seed1.as_ptr(),
                pone = in(reg) one.as_ptr(),
                o0 = in(reg) lanes0.as_mut_ptr(),
                o1 = in(reg) lanes1.as_mut_ptr(),
                n = inout(reg) ITERS => _,
                options(nostack),
            );
        }
        // One round = ITERS adds to both accumulators, each re-seeded by the
        // asm block: every lane must read exactly `seed + ITERS`.
        rounds += 1;
        let switches = crate::scheduler::switches_this_cpu().wrapping_sub(sw0);
        let elapsed = crate::pit::ticks().wrapping_sub(t0);
        let mut bad = false;
        for (lanes, base) in [(&lanes0, seed), (&lanes1, seed + 1000)] {
            for (i, got) in lanes.iter().enumerate() {
                let want = base + i as i32 + ITERS as i32;
                if *got != want {
                    // Full dump: the *pattern* identifies the failure mode
                    // (a rollback to an older snapshot of this same task, or
                    // another task's image restored into these registers).
                    if !bad {
                        crate::serial_writeln!(
                            "fpu: avx image XSTATE_BV={:#x} (bit2 set = image holds YMM)",
                            current_fpu_image_xstate_bv()
                        );
                        // Dump the image's own YMM region (component 2 starts
                        // at offset 576 in the standard layout): if it holds
                        // the expected mid-round values, the SAVE is fine and
                        // the RESTORE dropped them; if it holds zeros, the
                        // save itself lost the upper halves.
                        let area = crate::scheduler::current_fpu_area();
                        if !area.is_null() {
                            let ymm = unsafe {
                                core::slice::from_raw_parts(area.cast::<i32>().add(144), 8)
                            };
                            crate::serial_writeln!(
                                "fpu: avx image ymm0 region = {:?}",
                                ymm
                            );
                        }
                        crate::serial_writeln!(
                            "fpu: avx FAILED round {rounds} seed={seed} cpu={cpu} live_xcr0={:#x} ({switches} switches, {elapsed} ticks) - dumping lanes:",
                            live_xcr0()
                        );
                    }
                    bad = true;
                    crate::serial_writeln!(
                        "fpu: avx   lane {i}: got {got}, want {want} (delta {})",
                        want - *got
                    );
                }
            }
        }
        if bad {
            crate::serial_writeln!("fpu: avx FAILED: YMM state corrupted (see lanes above)");
            crate::scheduler::exit_current();
        }
        if switches == 0 {
            crate::serial_writeln!(
                "fpu: avx WEAK round {rounds} (seed {seed}, cpu {cpu}): 0 context switches in {elapsed} ticks - round too short to prove YMM preservation"
            );
        } else {
            crate::serial_writeln!(
                "fpu: avx PASS round {rounds} (seed {seed}, cpu {cpu}, 16 YMM lanes intact across {switches} switches in {elapsed} ticks, XCR0={:#x})",
                live_xcr0()
            );
        }
        seed = seed.wrapping_add(10_000);
    }
}
