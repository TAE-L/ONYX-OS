//! Global Descriptor Table (GDT) + Task State Segment (TSS).
//!
//! In long mode, the GDT is used for kernel/user segment selection and to load
//! a TSS, which lets the CPU switch to a dedicated kernel stack when an
//! interrupt/exception happens (stack isolation). It also provides a separate
//! double-fault stack so a #DF can be handled without tripling.

use x86_64::instructions::segmentation::{Segment, CS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Size of the IST stack for double faults.
pub const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024; // 16 KiB
/// Index into the TSS interrupt stack table used for double faults.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// The TSS holds the kernel stack and IST pointers. Its IST[0] is set up for
/// double-fault handling during lazy initialization; RSP0 (the stack the CPU
/// switches to when ring 3 is interrupted/syscalls) is updated per-task by
/// the scheduler on every context switch, hence the Mutex.
static TSS: spin::Lazy<spin::Mutex<TaskStateSegment>> = spin::Lazy::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = double_fault_stack_top();
    // Safe dummy until the scheduler points it at the current task's kernel
    // stack (`preempt()` re-points it before every switch, IF off).
    tss.privilege_stack_table[0] = double_fault_stack_top();
    spin::Mutex::new(tss)
});

/// Point TSS.RSP0 at `top` — the stack that ring-3 interrupts and exceptions
/// land on. Called by the scheduler on every context switch (IF off).
pub fn set_kernel_stack(top: VirtAddr) {
    TSS.lock().privilege_stack_table[0] = top;
}

/// Once cell storing the GDT for its `'static` lifetime (a CPU `lgdt` needs a
/// stable address for as long as the table is used).
static GDT: spin::Once<GlobalDescriptorTable> = spin::Once::new();

/// A static holding the segment selectors produced while building the GDT.
pub struct Selectors {
    pub code_selector: SegmentSelector,
    pub data_selector: SegmentSelector,
    /// Ring-3 data selector. Must sit exactly 8 below the code selector:
    /// SYSRET derives SS = STAR[63:48]+8 and CS = STAR[63:48]+16.
    pub user_data_selector: SegmentSelector,
    /// Ring-3 code selector.
    pub user_code_selector: SegmentSelector,
    pub tss_selector: SegmentSelector,
}

static SELECTORS: spin::Once<Selectors> = spin::Once::new();

/// Initialize the GDT + TSS and load them on this CPU.
pub fn init() {
    // Build the GDT: null entry is implied, then code/data segments + TSS.
    let mut gdt = GlobalDescriptorTable::new();
    let code_selector = gdt.append(Descriptor::kernel_code_segment());
    let data_selector = gdt.append(Descriptor::kernel_data_segment());
    // SAFETY: the TSS lives in a `spin::Lazy` static, so its address is
    // stable for the program's whole life and `tss_segment` can keep the
    // required `'static` reference. We only ever write RSP0 through the
    // Mutex afterwards (the TSS is never moved), which the GDT descriptor
    // does not capture — it stores the address itself.
    let tss: &'static TaskStateSegment =
        unsafe { &*(&*TSS.lock() as *const TaskStateSegment) };
    let tss_selector = gdt.append(Descriptor::tss_segment(tss));
    // Ring-3 segments, appended after the (16-byte-wide) TSS descriptor.
    // Order matters: user_data must end up exactly 8 below user_code so that
    // STAR[63:48] = user_code - 16 selects SS=+8/CS=+16 correctly on SYSRET.
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());

    // Store it in a `'static` cell, then load it (safe: it lives forever now).
    let gdt = GDT.call_once(|| gdt);
    gdt.load();
    unsafe {
        CS::set_reg(code_selector);
        load_tss(tss_selector);
    }

    // Remember selectors for later use (double-fault ISR, user mode, ...).
    SELECTORS.call_once(|| Selectors {
        code_selector,
        data_selector,
        user_data_selector,
        user_code_selector,
        tss_selector,
    });
}

/// Return the read-only selectors for this CPU.
pub fn selectors() -> &'static Selectors {
    SELECTORS.wait() // panics if GDT not initialized yet
}

/// Top address of the dedicated double-fault IST stack.
fn double_fault_stack_top() -> VirtAddr {
    static DOUBLE_FAULT_STACK: spin::Lazy<[u8; DOUBLE_FAULT_STACK_SIZE]> =
        spin::Lazy::new(|| [0; DOUBLE_FAULT_STACK_SIZE]);
    let stack = &*DOUBLE_FAULT_STACK;
    let top = stack.as_ptr() as u64 + stack.len() as u64;
    VirtAddr::new(top)
}