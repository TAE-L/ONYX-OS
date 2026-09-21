//! M9.8: symmetric multiprocessing — AP bring-up + per-CPU state.
//!
//! Everything that was a single-core assumption in M9.6-A2 becomes per-CPU:
//!
//!   * **Identity.** Each application processor (AP) owns one [`PerCpu`] block
//!     whose address lives in `IA32_GS_BASE`. The block's first field is the
//!     CPU index, so `gs:[0]` answers "which CPU am I?" from any context —
//!     including the syscall entry stub, which reads `gs:[32]`/`gs:[40]`
//!     (kernel-stack top / parked user RSP) instead of the old single global
//!     scratch slot two CPUs would have clobbered.
//!   * **Bring-up.** The BSP starts each AP with the classic INIT → SIPI →
//!     SIPI sequence using a trampoline in one reserved page below 1 MiB
//!     (SIPI can only address real-mode pages). The AP starts in 16-bit real
//!     mode, enables PAE + the kernel's CR3, turns on long mode and jumps to
//!     [`ap_entry`], which mirrors the BSP's control state (CR0/CR4/EFER,
//!     FPU/SSE, GDT/TSS, IDT, SYSCALL MSRs) and then idles in the scheduler's
//!     round robin exactly like the boot context does.
//!   * **Interrupts.** Each AP runs its own LAPIC timer (same calibrated
//!     interval as the BSP) and a reschedule IPI (vector 0x31) the scheduler
//!     sends when a task becomes Ready. Device IRQs keep their IOAPIC
//!     destination = the BSP's LAPIC, so drivers stay single-CPU by design.
//!
//! Failure policy: if the platform does not line up (no MADT LAPIC records, no
//! sub-1 MiB frame, an AP that never reports online) the kernel logs it and
//! stays on the BSP — every single-CPU behavior is unchanged.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use x86_64::registers::model_specific::{Efer, Msr};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::PhysAddr;
use x86_64::VirtAddr;

use crate::fpu::FpuArea;
use crate::serial_writeln;

/// Maximum supported CPUs (`-smp 4` and below). Every CPU-indexed table in the
/// kernel is this long.
pub const MAX_CPUS: usize = 4;

/// Sentinel "no task": the CPU is idle. Shared with the scheduler, which uses
/// it as its `MAIN_INDEX`.
pub const IDLE_INDEX: usize = usize::MAX;

/// IA32_GS_BASE — the kernel's per-CPU pointer (no FSGSBASE needed).
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Per-AP bring-up trace (one line per mirrored register per AP, ~12 lines per
/// core). Kept on: it is the only record of *where* an AP got stuck if bring-up
/// fails mid-way, and it is emitted once per boot. Flip to `false` for quiet
/// boots (`-smp 4` still prints the per-CPU "online"/summary lines either way).
const AP_TRACE: bool = true;

// ---------------------------------------------------------------------------
// Per-CPU block
// ---------------------------------------------------------------------------

/// Per-CPU state. The layout is ABI: the first field is read as `gs:[0]` and
/// `kstack_top`/`user_rsp` are read/written by the syscall entry stub as
/// `gs:[32]`/`gs:[40]` (`syscall::syscall_entry`). Those offsets are asserted
/// at compile time below.
#[repr(C, align(16))]
pub struct PerCpu {
    /// CPU index (0 = BSP). MUST stay at offset 0 (read via `gs:[0]`).
    pub index: u64,
    /// This CPU's xAPIC id. Atomic: the BSP writes it before releasing the AP.
    pub lapic_id: AtomicU32,
    /// Set by the AP itself once it runs in `ap_entry`.
    pub online: AtomicBool,
    _pad: [u8; 3],
    /// Task index running on this CPU (IDLE_INDEX = idle/boot context).
    /// Owner-only: written by this CPU under the scheduler lock.
    pub current: usize,
    /// Saved RSP of this CPU's idle context (the context-switch slot).
    pub idle_sp: u64,
    /// Kernel-stack top the syscall entry stub switches to (`gs:[32]`).
    pub kstack_top: u64,
    /// Parked user RSP used by the syscall entry stub (`gs:[40]`).
    pub user_rsp: u64,
    /// Top of this CPU's idle kernel stack (TSS.RSP0 while idle).
    pub idle_kstack_top: u64,
    /// Task this CPU suspended at its last context switch, left marked
    /// `Running` (owned by this CPU) so no other CPU can steal it before the
    /// incoming task really resumed. Released by the next `preempt` on this
    /// CPU (see `scheduler::release_pending_prev`).
    pub pending_prev: usize,
    /// AP bring-up stack top (0 on the BSP).
    pub ap_stack_top: u64,
    /// This CPU's LAPIC-timer tick count (M9.8 observability: proves each
    /// core's own timer is alive, independent of the global `MS` clock).
    pub timer_ticks: AtomicU64,
    /// Context switches performed on this CPU (M9.8-f: lets a test prove that
    /// preemptions really happened while a piece of code was executing).
    pub switches: AtomicU64,
    /// The interval this CPU's periodic timer was armed with. The timer
    /// handler re-arms on mismatch with `apic::TIMER_INTERVAL`, which is how
    /// the closed-loop correction (computed by a task on any core) reaches
    /// every CPU without a cross-CPU MMIO write.
    pub armed_interval: AtomicU32,
    /// Iterations completed by this CPU's SMP worker task (0 = never ran).
    pub worker_iters: AtomicU64,
    /// This CPU's idle-context FXSAVE image. Never read before the idle
    /// context was switched out once (that switch fills it).
    pub idle_fpu: FpuArea,
}

/// Compile-time ABI check for the GS-relative slots the syscall stub uses.
const _: () = {
    assert!(core::mem::offset_of!(PerCpu, index) == 0);
    assert!(core::mem::offset_of!(PerCpu, kstack_top) == GS_KSTACK_OFF);
    assert!(core::mem::offset_of!(PerCpu, user_rsp) == GS_USER_RSP_OFF);
};

/// GS-relative displacement of `kstack_top` (hardcoded in `syscall_entry`).
pub const GS_KSTACK_OFF: usize = 32;
/// GS-relative displacement of `user_rsp` (hardcoded in `syscall_entry`).
pub const GS_USER_RSP_OFF: usize = 40;

impl PerCpu {
    const fn new(index: u64) -> Self {
        Self {
            index,
            lapic_id: AtomicU32::new(0),
            online: AtomicBool::new(false),
            _pad: [0; 3],
            current: IDLE_INDEX,
            idle_sp: 0,
            kstack_top: 0,
            user_rsp: 0,
            idle_kstack_top: 0,
            pending_prev: IDLE_INDEX,
            ap_stack_top: 0,
            timer_ticks: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            armed_interval: AtomicU32::new(0),
            worker_iters: AtomicU64::new(0),
            idle_fpu: FpuArea::zeroed(),
        }
    }
}

/// The per-CPU blocks. Reached through raw pointers (one owner per block, plus
/// atomics for the fields another CPU may read) — the established `static mut`
/// pattern of this kernel. `static mut` avoids fabricating a shared reference
/// to a block another CPU is mutating.
static mut PERCPU: [PerCpu; MAX_CPUS] = [
    PerCpu::new(0),
    PerCpu::new(1),
    PerCpu::new(2),
    PerCpu::new(3),
];

/// Raw pointer to CPU `cpu`'s block. Always valid (the array is `static`).
#[inline]
pub fn per_cpu_ptr(cpu: usize) -> *mut PerCpu {
    assert!(cpu < MAX_CPUS, "cpu index out of range");
    unsafe { (&raw mut PERCPU[cpu]).cast::<PerCpu>() }
}

/// Which CPU is this? Reads `gs:[0]` — the block address installed by
/// [`set_gs_base`]. Only ever called after `init_bsp`/`set_gs_base`, which run
/// before any scheduled code exists.
#[inline]
pub fn cpu_index() -> usize {
    let i: u64;
    unsafe {
        core::arch::asm!(
            "mov {i}, qword ptr gs:[0]",
            i = out(reg) i,
            options(nostack, preserves_flags)
        );
    }
    i as usize
}

/// Install `cpu`'s block address as this CPU's kernel GS base.
///
/// # Safety
/// Call only on the CPU itself (each CPU has its own `IA32_GS_BASE`) and only
/// after that block is initialized.
unsafe fn set_gs_base(cpu: usize) {
    let mut msr = Msr::new(IA32_GS_BASE);
    msr.write(per_cpu_ptr(cpu) as u64);
}

/// This CPU's block (raw pointer: no reference to another CPU's state).
#[inline]
pub fn this_cpu() -> *mut PerCpu {
    per_cpu_ptr(cpu_index())
}

/// Task index running on this CPU (IDLE_INDEX = idle/boot context).
#[inline]
pub fn current_index() -> usize {
    unsafe { (*this_cpu()).current }
}

/// Set the task index running on this CPU (owner-only, scheduler lock held).
#[inline]
pub fn set_current_index(index: usize) {
    unsafe { (*this_cpu()).current = index };
}

/// The task this CPU suspended at its last switch (IDLE_INDEX = none).
#[inline]
pub fn pending_prev() -> usize {
    unsafe { (*this_cpu()).pending_prev }
}

/// Record the task this CPU is suspending (owner-only, scheduler lock held).
#[inline]
pub fn set_pending_prev(index: usize) {
    unsafe { (*this_cpu()).pending_prev = index };
}

/// Update this CPU's syscall kernel-stack top (`gs:[32]`).
pub fn set_kstack_top(top: u64) {
    unsafe { (*this_cpu()).kstack_top = top };
}

/// Top of this CPU's idle kernel stack (TSS.RSP0 / syscall stack while idle).
pub fn idle_kstack_top() -> u64 {
    unsafe { (*this_cpu()).idle_kstack_top }
}

/// How many CPUs reported online (always >= 1: the BSP counts itself).
pub fn online_cpus() -> usize {
    let mut n = 0;
    for cpu in 0..MAX_CPUS {
        if unsafe { (*per_cpu_ptr(cpu)).online.load(Ordering::Relaxed) } {
            n += 1;
        }
    }
    n
}

/// This CPU's LAPIC id (0 before `apic::init`).
pub fn lapic_id() -> u32 {
    unsafe { (*this_cpu()).lapic_id.load(Ordering::Relaxed) }
}

/// Is this CPU the boot processor?
pub fn is_bsp() -> bool {
    cpu_index() == 0
}

/// The reserved trampoline page (0 = none reserved → SMP stays off).
static LOW_PAGE: AtomicU64 = AtomicU64::new(0);

/// True once [`prepare`] copied + identity-mapped the trampoline page.
static PREPARED: AtomicBool = AtomicBool::new(false);

/// Record the sub-1 MiB page the AP trampoline is copied into. Called at boot
/// while the boot frame allocator is still owned by `kernel_main` (BEFORE the
/// heap claims frames), so the page is guaranteed untouched.
pub fn set_low_page(addr: u64) {
    LOW_PAGE.store(addr, Ordering::Relaxed);
    serial_writeln!("[smp] AP trampoline page reserved at {addr:#x}");
}

/// BSP control state, captured right before APs are started so each AP can
/// mirror it (an AP leaves reset with a bare CR0/CR4 and no EFER features).
static BSP_CR0: AtomicU64 = AtomicU64::new(0);
static BSP_CR4: AtomicU64 = AtomicU64::new(0);
static BSP_EFER: AtomicU64 = AtomicU64::new(0);

/// EFER bits an AP may mirror. Bit 10 (LMA) is READ-ONLY — writing it (the
/// BSP's raw EFER always has it set, since the BSP runs in long mode) raises
/// #GP, and on an AP that has not loaded an IDT yet that is a triple fault.
const EFER_WRITABLE_MASK: u64 = (1 << 0)  // SCE  — SYSCALL/SYSRET enable
    | (1 << 8)                            // LME  — long mode enable
    | (1 << 11)                           // NXE  — no-execute enable
    | (1 << 12)                           // SVME
    | (1 << 13)                           // LMSLE
    | (1 << 14)                           // FFXSR
    | (1 << 15)                           // TCE
    | (1 << 17)                           // MCOMMIT
    | (1 << 18)                           // INTWB
    | (1 << 19); // UAI


// ---------------------------------------------------------------------------
// AP trampoline (one page below 1 MiB)
// ---------------------------------------------------------------------------
//
// SIPI can only start an AP at a real-mode page (< 1 MiB), so the BSP copies
// this page there and patches four values into it (kernel CR3, the AP's
// bring-up stack top, the virtual address of `ap_entry`, and the page's own
// base). The listing below is the exact byte stream, hand-assembled so the
// milestone does not depend on the assembler's 16-bit mode support (verified
// with objdump -b binary -m i386 -M i8086 / i386:x86-64):
//
//   0x000 (16-bit, real mode — entered at the SIPI vector)
//     cli                           FA
//     xor   ax, ax                  31 C0
//     mov   ds, ax                  8E D8
//     mov   es, ax                  8E C0
//     mov   ss, ax                  8E D0
//     mov   sp, 0xF00(base)         BC xx xx     <- real-mode stack (patch)
//     mov   si, 0x198(base)         BE xx xx     <- &GDTR (patch)
//     lgdt  [si]                    66 0F 01 14  (m16&32: 32-bit GDT base)
//     mov   eax, cr4                66 0F 20 E0
//     or    eax, PAE|PGE            66 0D A0 00 00 00
//     mov   cr4, eax                66 0F 22 E0
//     mov   si, 0x080(base)         BE xx xx     <- &CR3 slot (patch)
//     mov   eax, [si]               66 8B 04
//     mov   cr3, eax                66 0F 22 D8  (kernel page tables)
//     mov   ecx, 0xC0000080         66 B9 80 00 00 C0
//     rdmsr                         0F 32
//     or    eax, SCE|NXE|LME        66 0D 01 09 00 00
//     wrmsr                         0F 30
//     mov   eax, cr0                66 0F 20 C0
//     or    eax, PG|PE              66 0D 01 00 00 80
//     mov   cr0, eax                66 0F 22 C0
//     jmp   0x08:0x100(base)        66 EA xx xx xx xx 08 00 (long mode)
//
//   0x100 (64-bit stub — the far jump lands here with the low page identity
//          mapped under the kernel's CR3)
//     mov   ax, 0x10                66 B8 10 00
//     mov   ds, ax                  8E D8
//     mov   es, ax                  8E C0
//     mov   ss, ax                  8E D0
//     mov   rsp, [0x088(base)]      48 8B 24 25 xx xx xx xx
//     mov   rax, [0x090(base)]      48 8B 04 25 xx xx xx xx
//     jmp   rax                     FF E0        (-> ap_entry, kernel VA)
//
//   0x180  GDT: null, code64 (selector 0x08), flat data (selector 0x10)
//   0x198  GDTR: limit 0x17 + 32-bit base 0x180(base)
//   0x080/0x088/0x090: CR3 / RSP / entry patch slots
//   0xF00  real-mode stack top (grows down, never leaves the page)

/// The 16-bit entry point inside the page.
const OFF_CODE: usize = 0x000;
/// Patch slot: the kernel's CR3 value (read as a dword in real mode).
const OFF_CR3: usize = 0x080;
/// Patch slot: the AP's bring-up stack top (kernel VA, 64-bit).
const OFF_RSP: usize = 0x088;
/// Patch slot: `ap_entry`'s virtual address (64-bit).
const OFF_ENTRY: usize = 0x090;
/// The 64-bit stub the real-mode code far-jumps into.
const OFF_STUB64: usize = 0x100;
/// The three GDT descriptors (null, code64, data).
const OFF_GDT: usize = 0x180;
/// The 6-byte GDTR operand of `lgdt [si]` (limit + 32-bit base).
const OFF_GDTR: usize = 0x198;
/// Real-mode stack top (the stack grows down from here, inside the page).
const OFF_RM_STACK: usize = 0xF00;

/// 16-bit real-mode → long-mode code (81 bytes; patch sites noted inline).
const LOW_CODE: [u8; 81] = [
    0xFA, // cli
    0x31, 0xC0, // xor ax, ax
    0x8E, 0xD8, // mov ds, ax
    0x8E, 0xC0, // mov es, ax
    0x8E, 0xD0, // mov ss, ax
    0xBC, 0x00, 0x00, // mov sp, imm16          (patch @0x0A)
    0xBE, 0x00, 0x00, // mov si, imm16          (patch @0x0D -> GDTR)
    0x66, 0x0F, 0x01, 0x14, // lgdt [si]        (m16&32)
    0x66, 0x0F, 0x20, 0xE0, // mov eax, cr4
    0x66, 0x0D, 0xA0, 0x00, 0x00, 0x00, // or eax, PAE|PGE
    0x66, 0x0F, 0x22, 0xE0, // mov cr4, eax
    0xBE, 0x00, 0x00, // mov si, imm16          (patch @0x22 -> CR3 slot)
    0x66, 0x8B, 0x04, // mov eax, [si]
    0x66, 0x0F, 0x22, 0xD8, // mov cr3, eax
    0x66, 0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, 0xC0000080 (EFER)
    0x0F, 0x32, // rdmsr
    0x66, 0x0D, 0x01, 0x09, 0x00, 0x00, // or eax, SCE|NXE|LME
    0x0F, 0x30, // wrmsr
    0x66, 0x0F, 0x20, 0xC0, // mov eax, cr0
    0x66, 0x0D, 0x01, 0x00, 0x00, 0x80, // or eax, PG|PE
    0x66, 0x0F, 0x22, 0xC0, // mov cr0, eax
    0x66, 0xEA, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, // jmp 0x08:imm32
];

/// 64-bit stub (28 bytes): flat data selector, RSP + entry from the patch
/// slots, then jump into Rust.
const STUB64: [u8; 28] = [
    0x66, 0xB8, 0x10, 0x00, // mov ax, 0x10
    0x8E, 0xD8, // mov ds, ax
    0x8E, 0xC0, // mov es, ax
    0x8E, 0xD0, // mov ss, ax
    0x48, 0x8B, 0x24, 0x25, 0x00, 0x00, 0x00, 0x00, // mov rsp, [imm32]
    0x48, 0x8B, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, // mov rax, [imm32]
    0xFF, 0xE0, // jmp rax
];

/// GDT: null, 64-bit code (selector 0x08), flat data (selector 0x10).
const GDT_BLOB: [u8; 24] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // null
    0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xAF, 0x00, // code64 (L=1, DPL 0)
    0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // data   (D/B=1)
];

/// GDTR limit for the three descriptors above (24 bytes - 1).
const GDTR_LIMIT: u16 = 0x17;
#[inline]
fn put_u16(page: &mut [u8; 4096], off: usize, v: u16) {
    page[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn put_u32(page: &mut [u8; 4096], off: usize, v: u32) {
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn put_u64(page: &mut [u8; 4096], off: usize, v: u64) {
    page[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Build the trampoline page for one AP: copy the blobs in place and patch the
/// absolute values (the page's own base is a runtime choice, so every
/// absolute address inside is computed here).
fn build_page(base: u64, cr3: u64, ap_stack_top: u64, entry: u64) -> [u8; 4096] {
    let mut page = [0u8; 4096];
    page[OFF_CODE..OFF_CODE + LOW_CODE.len()].copy_from_slice(&LOW_CODE);
    page[OFF_STUB64..OFF_STUB64 + STUB64.len()].copy_from_slice(&STUB64);
    page[OFF_GDT..OFF_GDT + GDT_BLOB.len()].copy_from_slice(&GDT_BLOB);
    // GDTR operand: limit + 32-bit base of the GDT inside this page.
    put_u16(&mut page, OFF_GDTR, GDTR_LIMIT);
    put_u32(&mut page, OFF_GDTR + 2, (base + OFF_GDT as u64) as u32);
    // Data slots consumed (in 64-bit mode) by the stub's absolute loads.
    put_u64(&mut page, OFF_CR3, cr3);
    put_u64(&mut page, OFF_RSP, ap_stack_top);
    put_u64(&mut page, OFF_ENTRY, entry);
    // Code patches (16-bit immediates / the far jump's 32-bit offset).
    put_u16(&mut page, OFF_CODE + 0x0A, (base + OFF_RM_STACK as u64) as u16);
    put_u16(&mut page, OFF_CODE + 0x0D, (base + OFF_GDTR as u64) as u16);
    put_u16(&mut page, OFF_CODE + 0x22, (base + OFF_CR3 as u64) as u16);
    put_u32(&mut page, OFF_CODE + 0x4B, (base + OFF_STUB64 as u64) as u32);
    // 64-bit stub: the identity-mapped physical address of the slots.
    put_u32(&mut page, OFF_STUB64 + 0x0E, (base + OFF_RSP as u64) as u32);
    put_u32(&mut page, OFF_STUB64 + 0x16, (base + OFF_ENTRY as u64) as u32);
    page
}

/// Copy the trampoline page into the reserved low frame (through the
/// bootloader's physical-memory window) and identity-map it so the AP can run
/// from it after it turns paging on.
///
/// Runs ONCE (all APs share the page; only its contents are re-patched per
/// AP — see [`start_aps`]). MUST be called with interrupts disabled: the
/// page-table edit must not interleave with another task's `map_to`
/// (`userspace::spawn_from_vfs` etc.), and the caller's IF=0 guarantees that.
///
/// Returns false if the mapping is impossible — SMP then stays off instead of
/// hanging.
pub fn prepare(mapper: &mut OffsetPageTable<'static>) -> bool {
    let base = LOW_PAGE.load(Ordering::Relaxed);
    // The trampoline patches its own addresses as 16-bit immediates, so it
    // cannot live above 0xE000.
    if base == 0 || base > 0xE000 {
        serial_writeln!("[smp] no sub-1 MiB trampoline page - staying on the BSP");
        return false;
    }
    let Some(off) = crate::memory::phys_offset() else {
        serial_writeln!("[smp] phys window unavailable - cannot build AP trampoline");
        return false;
    };
    // Install a first copy so the page is never empty (its per-AP patches are
    // written by `start_aps`).
    write_page(base, off, 0, 0, 0);
    let mut ok = false;
    unsafe {
        crate::memory::with_global_frames(|frames| {
            let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(base));
            let virt = Page::<Size4KiB>::containing_address(VirtAddr::new(base));
            match mapper.map_to(
                virt,
                frame,
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                frames,
            ) {
                Ok(flush) => {
                    flush.flush();
                    ok = true;
                }
                Err(e) => serial_writeln!("[smp] identity map of {base:#x} failed: {e:?}"),
            }
        });
    }
    if !ok {
        serial_writeln!("[smp] could not identity-map {base:#x} - AP bring-up skipped");
        return false;
    }
    PREPARED.store(true, Ordering::Relaxed);
    serial_writeln!("[smp] trampoline page {base:#x} installed and identity-mapped");
    true
}

/// Write one AP's trampoline page contents into the reserved frame (plain
/// memory writes through the physical-memory window: the MAPPING is done once
/// by [`prepare`], so per-AP patching touches no page tables).
fn write_page(base: u64, phys_off: u64, cr3: u64, ap_stack_top: u64, entry: u64) {
    let page = build_page(base, cr3, ap_stack_top, entry);
    unsafe {
        core::ptr::copy_nonoverlapping(
            page.as_ptr(),
            (phys_off + base) as *mut u8,
            page.len(),
        );
    }
}
/// Raw CR0/CR4 accessors (inline asm instead of the crate's bitflags wrapper:
/// the AP must mirror the BSP's control registers bit-for-bit, and these are
/// the bytes the trampoline already programmed).
#[inline]
unsafe fn read_cr0() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, cr0", out(reg) v, options(nostack, preserves_flags));
    v
}
#[inline]
unsafe fn write_cr0(v: u64) {
    core::arch::asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags));
}
#[inline]
unsafe fn read_cr4() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, cr4", out(reg) v, options(nostack, preserves_flags));
    v
}
#[inline]
unsafe fn write_cr4(v: u64) {
    core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags));
}

/// Bring-up stacks for the APs (16 KiB each; one per non-BSP CPU slot).
///
/// `static mut` on purpose: a plain `static` lands in `.rodata`, and the AP
/// writes to its stack from the moment it mirrors the BSP's `CR0.WP` — a
/// read-only stack faults on the first push, and the resulting #DF cannot run
/// on its (also read-only) IST stack, i.e. a silent triple fault.
#[repr(align(16))]
struct ApStack([u8; 16 * 1024]);
static mut AP_STACKS: [ApStack; MAX_CPUS] = [const { ApStack([0; 16 * 1024]) }; MAX_CPUS];

/// 16-byte-aligned top of the AP bring-up stack for CPU `cpu`.
fn ap_stack_top(cpu: usize) -> u64 {
    // Raw-pointer arithmetic: no reference to the `static mut` is formed.
    let base = core::ptr::addr_of!(AP_STACKS) as *const u8;
    let off = cpu * core::mem::size_of::<ApStack>();
    let top = unsafe { base.add(off) as u64 } + 16 * 1024;
    top & !0xF
}

/// Bring CPU 0's per-CPU state up (identity, GS base, idle stack) and mark it
/// online. Call once at boot, before the GDT is built (the per-CPU TSS takes
/// its idle RSP0 from here) and before the LAPIC timer can preempt.
pub fn init_bsp() {
    unsafe {
        let p = per_cpu_ptr(0);
        (*p).online.store(true, Ordering::Relaxed);
        (*p).current = IDLE_INDEX;
        (*p).pending_prev = IDLE_INDEX;
        // The boot context keeps its bootloader-provided stack while it runs;
        // this is the *idle* stack TSS.RSP0/syscall entry use whenever no task
        // is current, so it only has to be valid — slot 0 of the AP stacks.
        let top = ap_stack_top(0);
        (*p).idle_kstack_top = top;
        (*p).kstack_top = top;
        set_gs_base(0);
    }
    serial_writeln!("[smp] BSP per-CPU state armed (gs base = per-CPU block 0)");
}

/// Wait ~`n` PIT ticks (10 ms each at 100 Hz).
fn wait_pit_ticks(n: u64) {
    let t0 = crate::pit::ticks();
    while crate::pit::ticks() - t0 < n {
        core::hint::spin_loop();
    }
}

/// Short (~0.2 ms under TCG, far above the 200 µs SIPI requirement) busy wait.
fn delay_short() {
    for _ in 0..200_000u32 {
        core::hint::spin_loop();
    }
}
/// Start every AP the MADT advertises. Requires [`prepare`] to have run.
///
/// Called from a scheduled kernel task (`bringup_task`) on the BSP, with
/// interrupts ENABLED: the INIT/SIPI waits are measured in PIT ticks, which
/// only advance while the timer IRQ is delivered. Every per-AP step here is
/// either a plain memory write into the already-mapped trampoline page or an
/// MMIO write to the LAPIC (the ICR belongs to the *sender*), so no page
/// tables change while other tasks run.
pub fn start_aps() {
    if !PREPARED.load(Ordering::Relaxed) {
        serial_writeln!("[smp] trampoline not prepared - staying on the BSP");
        return;
    }
    let base = LOW_PAGE.load(Ordering::Relaxed);
    let Some(off) = crate::memory::phys_offset() else {
        return;
    };
    if !crate::apic::active() {
        serial_writeln!("[smp] APIC path not live - staying on the BSP");
        return;
    }
    let (ids, count) = crate::acpi::lapic_ids();
    let bsp_id = crate::apic::lapic_id();
    serial_writeln!(
        "[smp] MADT lists {count} LAPIC(s); BSP lapic id {bsp_id:#x}, trampoline {base:#x}"
    );

    // Capture the BSP's control state: an AP leaves reset with a bare CR0/CR4
    // and no EFER features, and must end up bit-identical (PAE/PGE, FPU/SSE,
    // NX, SYSCALL enable).
    unsafe {
        BSP_CR0.store(read_cr0(), Ordering::Relaxed);
        BSP_CR4.store(read_cr4(), Ordering::Relaxed);
        let mut efer = Efer::MSR;
        BSP_EFER.store(efer.read(), Ordering::Relaxed);
    }
    let cr3 = Cr3::read().0.start_address().as_u64();

    let mut cpu = 1usize;
    let mut started = 0usize;
    for k in 0..count {
        let id = u32::from(ids[k]);
        if id == bsp_id {
            continue;
        }
        if cpu >= MAX_CPUS {
            serial_writeln!("[smp] lapic id {id:#x}: no PerCpu slot left (max {MAX_CPUS})");
            break;
        }
        // Publish this CPU's identity + stacks BEFORE the AP can run.
        let top16 = ap_stack_top(cpu);
        unsafe {
            let p = per_cpu_ptr(cpu);
            (*p).lapic_id.store(id, Ordering::Relaxed);
            (*p).online.store(false, Ordering::Relaxed);
            (*p).current = IDLE_INDEX;
            (*p).pending_prev = IDLE_INDEX;
            (*p).idle_kstack_top = top16;
            // `jmp` (not `call`) enters ap_entry: present the SysV invariant
            // RSP ≡ 8 (mod 16), exactly like the scheduler's fresh stacks.
            (*p).ap_stack_top = top16 - 8;
        }
        let sipi_vector = (base >> 12) as u8;
        write_page(base, off, cr3, top16 - 8, ap_entry as *const () as usize as u64);
        serial_writeln!(
            "[smp] starting cpu {cpu}: lapic id {id:#x}, sipi vector {sipi_vector:#x}, stack {top16:#x}"
        );

        // INIT (assert) -> ~10 ms -> INIT (deassert) -> ~10 ms -> SIPI ->
        // ~200 µs -> SIPI. Two SIPIs are the SDM sequence: the first can be
        // lost while the AP is still resetting.
        crate::apic::send_icr(id, 0x0000_4500); // INIT, level assert, edge
        wait_pit_ticks(1);
        crate::apic::send_icr(id, 0x0000_0500); // INIT deassert
        wait_pit_ticks(1);
        crate::apic::send_icr(id, 0x0000_4600 | u32::from(sipi_vector));
        delay_short();
        crate::apic::send_icr(id, 0x0000_4600 | u32::from(sipi_vector));

        // Wait up to ~200 ms for the AP to raise its online flag.
        let mut online = false;
        for _ in 0..20 {
            wait_pit_ticks(1);
            if unsafe { (*per_cpu_ptr(cpu)).online.load(Ordering::Acquire) } {
                online = true;
                break;
            }
        }
        if online {
            started += 1;
        } else {
            serial_writeln!("[smp] cpu {cpu} (lapic id {id:#x}) never came online - skipping");
        }
        cpu += 1;
    }

    serial_writeln!("[smp] {} CPU(s) online after bring-up", online_cpus());
    if started > 0 {
        // Let the APs pick up work immediately rather than waiting for their
        // own timer tick.
        kick_others();
    }
}
/// Send a fixed-delivery IPI with `vector` to LAPIC `dest_id` (physical
/// destination, edge-triggered, level asserted).
pub fn send_ipi(dest_id: u32, vector: u8) {
    crate::apic::send_icr(dest_id, 0x0000_4000 | u32::from(vector));
}

/// Ask every other online CPU to re-run its scheduler right now (a task just
/// became Ready). A no-op while only the BSP is online.
pub fn kick_others() {
    if online_cpus() <= 1 {
        return;
    }
    let me = cpu_index();
    for cpu in 0..MAX_CPUS {
        if cpu == me {
            continue;
        }
        let p = per_cpu_ptr(cpu);
        let online = unsafe { (*p).online.load(Ordering::Relaxed) };
        if !online {
            continue;
        }
        let id = unsafe { (*p).lapic_id.load(Ordering::Relaxed) };
        send_ipi(id, crate::apic::RESCHED_VECTOR);
    }
}

/// First Rust code an AP runs: identify itself, mirror the BSP's CPU state,
/// initialize its per-CPU tables and join the scheduler's idle loop.
extern "C" fn ap_entry() -> ! {
    // Per-step trace (AP_TRACE): these lines run before the AP has a working
    // console of its own, so they are the only evidence of how far an AP got.
    macro_rules! ap_trace {
        ($($t:tt)*) => {
            if AP_TRACE {
                serial_writeln!($($t)*);
            }
        };
    }
    ap_trace!("[smp] AP: reached 64-bit ap_entry (long mode + kernel CR3 live)");
    let id = crate::apic::lapic_id();
    ap_trace!("[smp] AP: lapic id {id:#x}");
    let mut cpu = MAX_CPUS;
    for i in 0..MAX_CPUS {
        if unsafe { (*per_cpu_ptr(i)).lapic_id.load(Ordering::Relaxed) } == id {
            cpu = i;
            break;
        }
    }
    if cpu == MAX_CPUS {
        serial_writeln!("[smp] AP with unknown lapic id {id:#x} - halting");
        loop {
            x86_64::instructions::hlt();
        }
    }
    // GS base first: every helper below (and the syscall entry stub) identifies
    // the CPU through it.
    unsafe { set_gs_base(cpu) };
    ap_trace!("[smp] AP {cpu}: per-CPU identity armed (gs base installed)");

    // GDT + TSS FIRST: `ltr` is what makes the IST/ring-3 stacks real. Without
    // a valid TSS any fault is a triple fault (the #DF handler needs IST[0]),
    // which shows up as a silent machine reset.
    crate::gdt::init_ap(cpu);
    ap_trace!("[smp] AP {cpu}: own GDT + TSS loaded (TR valid)");

    // Then the shared IDT, so every fault from here on reports.
    crate::interrupts::init();
    ap_trace!("[smp] AP {cpu}: shared IDT loaded");

    // Mirror the BSP. The trampoline already set PAE/PGE + EFER.LME (its own
    // read-modify-write on the AP's EFER), so this finishes the job.
    unsafe {
        write_cr4(BSP_CR4.load(Ordering::Relaxed));
        ap_trace!(
            "[smp] AP {cpu}: CR4={:#x} mirrored",
            BSP_CR4.load(Ordering::Relaxed)
        );
        write_cr0(BSP_CR0.load(Ordering::Relaxed));
        ap_trace!(
            "[smp] AP {cpu}: CR0={:#x} mirrored",
            BSP_CR0.load(Ordering::Relaxed)
        );
        // EFER.LMA (bit 10) is READ-ONLY — writing the BSP's raw value (which
        // has LMA set, because the BSP is in long mode) faults. Read this
        // CPU's EFER and OR in only the writable feature bits.
        let mut efer = Efer::MSR;
        let raw = efer.read();
        efer.write(raw | (BSP_EFER.load(Ordering::Relaxed) & EFER_WRITABLE_MASK));
        ap_trace!(
            "[smp] AP {cpu}: EFER={:#x} -> {:#x}",
            BSP_EFER.load(Ordering::Relaxed),
            raw | (BSP_EFER.load(Ordering::Relaxed) & EFER_WRITABLE_MASK)
        );
    }
    crate::fpu::init_ap();
    ap_trace!(
        "[smp] AP {cpu}: FPU/SSE control registers ready (live XCR0={:#x})",
        crate::fpu::live_xcr0()
    );
    crate::syscall::init_ap();
    ap_trace!("[smp] AP {cpu}: SYSCALL MSRs programmed");
    crate::apic::init_ap();
    ap_trace!("[smp] AP {cpu}: local APIC + timer armed");

    unsafe { (*per_cpu_ptr(cpu)).online.store(true, Ordering::Release) };
    serial_writeln!("[smp] cpu {cpu} online (lapic id {id:#x})");

    // Give this CPU its own worker task: the scheduler only ever runs a task on
    // the CPU it was spawned on (affinity), so the APs get real, independent
    // work without making the single-CPU kernel subsystems concurrent.
    crate::scheduler::spawn_on_cpu(ap_worker_task, crate::scheduler::PRIO_NORMAL, cpu);
    ap_trace!("[smp] AP {cpu}: worker task queued for this core");

    // Idle: this CPU's LAPIC timer preempts every millisecond and the
    // reschedule IPI wakes it the moment another CPU makes a task Ready.
    x86_64::instructions::interrupts::enable();
    loop {
        x86_64::instructions::hlt();
        crate::scheduler::preempt();
    }
}

/// Body of the M9.8 per-AP worker task: an independent arithmetic +
/// floating-point accumulator that runs *only* on its own CPU.
///
/// Beyond giving the APs genuine work, this is the SMP regression test:
///   * `iters` advances only while this task is actually scheduled, so a
///     nonzero, growing count proves the AP really executes kernel tasks;
///   * the XMM accumulators are exact integers (< 2^24) checked every 100k
///     iterations, so any FPU/SSE state corruption — including between two
///     CPUs running simultaneously — shows up as a FAILED line instead of
///     silent noise.
fn ap_worker_task() {
    let me = cpu_index();
    let mut acc = 1.0f32 + me as f32;
    let mut iters: u64 = 0;
    let mut loops: u64 = 0;
    crate::serial_writeln!("[smp] cpu {me} worker task running (fp+int load; trace={AP_TRACE})");
    loop {
        let (mut a0, mut a1) = (acc, acc + 1.0);
        for _ in 0..100_000u32 {
            a0 += 1.0;
            a1 += 1.0;
            iters += 1;
        }
        if a0 != acc + 100_000.0 || a1 != acc + 100_001.0 {
            crate::serial_writeln!(
                "[smp] cpu {me} worker FAILED: fp state corrupted (a0={a0}, a1={a1})"
            );
            crate::scheduler::exit_current();
        }
        acc = a0 - 100_000.0;
        loops += 1;
        unsafe {
            (*per_cpu_ptr(me)).worker_iters.store(iters, Ordering::Relaxed);
        }
        // Heartbeat: a line per CPU every ~5 s proves this core's task really
        // runs AND that its own LAPIC timer keeps preempting (ticks grow).
        if loops % 50 == 1 {
            let ticks = unsafe { (*per_cpu_ptr(me)).timer_ticks.load(Ordering::Relaxed) };
            crate::serial_writeln!(
                "[smp] cpu {me} alive: worker iters {iters}, timer ticks {ticks}"
            );
        }
        // Polite: also exercises this CPU's sleep/wake path (the global clock
        // is advanced by the BSP, so this AP is woken by its own timer).
        crate::scheduler::sleep_kernel(100);
    }
}

/// M9.8 kernel task: bring the APs up once the system is live.
///
/// It must run as a TASK, not from `kernel_main`: by the time the LAPIC timer
/// is running, the boot context is no longer scheduled (nothing is Ready as
/// long as the peripheral tasks are), so code after `apic::init` in
/// `kernel_main` never executes. A task is scheduled, so this always runs.
///
/// The page-table edit in [`prepare`] happens with interrupts disabled (no
/// other task on this CPU can touch the mapper while it runs, and no AP exists
/// yet), while the INIT/SIPI waits in [`start_aps`] run with interrupts on —
/// they are measured in PIT ticks.
fn bringup_task() {
    // Let the boot tasks settle and give `apic::init` time to program (and
    // publish) the BSP's periodic timer — the APs arm their own timers from
    // that value. Bounded wait: an unpublished interval is not fatal (the APs
    // then just run IPI-driven with their timer masked), but it must not hold
    // the bring-up back forever.
    for _ in 0..20 {
        if crate::apic::timer_interval() != 0 {
            break;
        }
        crate::scheduler::sleep_kernel(100);
    }
    serial_writeln!(
        "[smp] bring-up task: starting AP bring-up (BSP timer interval = {})",
        crate::apic::timer_interval()
    );
    let Some(mut mapper) = crate::memory::runtime_mapper() else {
        serial_writeln!("[smp] no runtime mapper - staying on the BSP");
        crate::scheduler::exit_current();
    };
    let prepared = x86_64::instructions::interrupts::without_interrupts(|| prepare(&mut mapper));
    if !prepared {
        crate::scheduler::exit_current();
    }
    start_aps();
    // Summary after the APs have had a moment to run: the per-CPU timer tick
    // and worker-iteration counters are what prove real parallel execution.
    log_cpu_summary();
    serial_writeln!(
        "[smp] bring-up complete: {} CPU(s) online; scheduler lock + IPI path live; this CPU's run queue: {}",
        online_cpus(),
        if crate::scheduler::has_ready_tasks() {
            "Ready work present"
        } else {
            "empty"
        }
    );
    crate::scheduler::exit_current();
}

/// Spawn the AP bring-up task (called from `kernel_main` before the scheduler
/// saturates the BSP). Idempotent: the task itself no-ops on a 1-CPU boot.
pub fn spawn_bringup_task() {
    crate::scheduler::spawn(bringup_task);
}

/// Per-CPU tick + worker progress summary (one line per online CPU). Called at
/// the end of `start_aps` so a single boot log proves which cores are live.
pub fn log_cpu_summary() {
    for cpu in 0..MAX_CPUS {
        let p = per_cpu_ptr(cpu);
        let online = unsafe { (*p).online.load(Ordering::Relaxed) };
        if !online {
            continue;
        }
        let ticks = unsafe { (*p).timer_ticks.load(Ordering::Relaxed) };
        let iters = unsafe { (*p).worker_iters.load(Ordering::Relaxed) };
        serial_writeln!(
            "[smp] cpu {cpu}: lapic id {:#x}, timer ticks {ticks}, worker iters {iters}",
            unsafe { (*p).lapic_id.load(Ordering::Relaxed) }
        );
    }
}
