//! FPU/SIMD (x87 + SSE) state management.
//!
//! Every task owns a 512-byte FXSAVE area. The scheduler's naked
//! `context_switch` saves the outgoing task's x87+XMM state with `fxsave`
//! and loads the incoming task's with `fxrstor` — eager, not lazy: at
//! 100 Hz preemption the two instructions cost ~200 cycles per switch and
//! need no `#NM` trickery (the `#NM` handler stays a "must never happen"
//! diagnostic).
//!
//! Why this exists: kernel AND ring-3 code is compiled for baseline x86_64
//! (SSE2), so every task may keep live values in XMM registers across a
//! preemption. Without save/restore, the next task silently corrupts them —
//! a compile-time-invisible data-corruption bug.
//!
//! Fresh areas are seeded from `TEMPLATE` (captured right after `fninit`):
//! an all-zero area would make `fxrstor` unmask every FP exception
//! (FCW=0, MXCSR=0) and crash the first SSE instruction with #XM.

use alloc::boxed::Box;

/// FXSAVE/FXRSTOR image: 512 bytes, 16-byte aligned (a hard requirement of
/// both instructions; the wrapper type guarantees the alignment).
#[repr(C, align(16))]
pub struct FpuArea {
    bytes: [u8; 512],
}

impl FpuArea {
    pub const fn zeroed() -> Self {
        Self { bytes: [0; 512] }
    }
}

/// Valid FXRSTOR image, captured immediately after `fninit` in [`init`].
static mut TEMPLATE: FpuArea = FpuArea::zeroed();

/// Ensure the control-register state matches "SSE usable, x87 native error
/// reporting, FP exceptions masked", reset the FPU, and capture the template
/// image every fresh task area starts from. Call once at boot, before any
/// task is spawned and before any `fxsave`/`fxrstor` runs.
pub fn init() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    // CR0: MP=1 (SSE instructions honor CR0.TS), EM=0 (no x87 emulation),
    // NE=1 (native x87 error reporting -> #MF, which has a handler).
    let mut cr0 = Cr0::read();
    cr0.remove(Cr0Flags::EMULATE_COPROCESSOR);
    cr0.insert(Cr0Flags::MONITOR_COPROCESSOR | Cr0Flags::NUMERIC_ERROR);
    unsafe { Cr0::write(cr0) };
    // CR4: OSFXSR=1 — FXSAVE saves XMM state and SSE instructions are usable.
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::OSFXSR);
    unsafe { Cr4::write(cr4) };
    // Reset x87 + MXCSR to power-up defaults (every exception masked), then
    // capture that state as the template for fresh task areas.
    unsafe { core::arch::asm!("fninit") };
    let template_ptr = core::ptr::addr_of_mut!(TEMPLATE) as *mut u8;
    unsafe { core::arch::asm!("fxsave [{}]", in(reg) template_ptr) };
}

/// Allocate a fresh, template-seeded FXSAVE area (16-aligned, 'static).
/// Called by the scheduler when a task is spawned (IF=0 context).
pub fn new_area() -> &'static mut FpuArea {
    unsafe {
        let mut area = Box::new(FpuArea::zeroed());
        let src = &*core::ptr::addr_of!(TEMPLATE);
        area.bytes = src.bytes;
        Box::leak(area)
    }
}