//! Global Descriptor Table (GDT) + Task State Segment (TSS) — per CPU (M9.8).
//!
//! In long mode, the GDT is used for kernel/user segment selection and to load
//! a TSS, which lets the CPU switch to a dedicated kernel stack when an
//! interrupt/exception happens (stack isolation). It also provides a separate
//! double-fault stack so a #DF can be handled without tripling.
//!
//! Every CPU needs its OWN GDT + TSS: `TSS.RSP0` (the ring-3 entry stack) and
//! the IST pointers are CPU state, and two CPUs sharing one TSS would fight
//! over RSP0 on every context switch. The descriptor *layout* is identical
//! everywhere, so the selectors are shared (`SELECTORS`); only the tables are
//! per-CPU. The boot CPU builds slot 0 in [`init`], each AP its slot in
//! [`init_ap`] (from `smp::ap_entry`, before it joins the scheduler).

use x86_64::instructions::segmentation::{Segment, CS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::smp::{self, MAX_CPUS};

/// Size of the IST stack for double faults (one per CPU).
pub const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024; // 16 KiB
/// Index into the TSS interrupt stack table used for double faults.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Dedicated double-fault IST stacks, one per CPU (a #DF must be handled even
/// if the faulting CPU's kernel stack is corrupt).
///
/// `static mut` on purpose: this is written to *while handling a fault*, so it
/// must be in writable memory. A plain `static` would be placed in `.rodata`,
/// and a #DF handled with `CR0.WP=1` would fault on its first push — the #DF
/// handler could never run, i.e. a silent triple fault.
#[repr(align(16))]
struct DfStack([u8; DOUBLE_FAULT_STACK_SIZE]);
static mut DF_STACKS: [DfStack; MAX_CPUS] =
    [const { DfStack([0; DOUBLE_FAULT_STACK_SIZE]) }; MAX_CPUS];

/// Per-CPU TSS. `IST[0]` is set up once per CPU while building; RSP0 (the
/// stack the CPU switches to when ring 3 is interrupted/syscalls) is updated
/// per-task by the scheduler on every context switch, hence the Mutex.
static TSSES: [spin::Once<spin::Mutex<TaskStateSegment>>; MAX_CPUS] =
    [const { spin::Once::new() }; MAX_CPUS];

/// A static holding the segment selectors produced while building the GDT.
/// Identical for every CPU (the descriptor order is the same), so it is filled
/// by the first CPU and read by all.
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

/// One CPU's GDT together with the selectors it produced, kept for the
/// program's lifetime (a `lgdt` needs a stable address as long as it is used).
struct CpuGdt {
    gdt: GlobalDescriptorTable,
    sels: Selectors,
}

/// Per-CPU GDTs, built once per CPU.
static GDTS: [spin::Once<CpuGdt>; MAX_CPUS] = [const { spin::Once::new() }; MAX_CPUS];

static SELECTORS: spin::Once<Selectors> = spin::Once::new();

/// Point the CURRENT CPU's TSS.RSP0 at `top` — the stack that ring-3
/// interrupts and exceptions land on. Called by the scheduler on every context
/// switch (IF off), so it always targets the CPU doing the switch.
pub fn set_kernel_stack(top: VirtAddr) {
    let tss = TSSES[smp::cpu_index()].wait();
    tss.lock().privilege_stack_table[0] = top;
}

/// Build + load the GDT/TSS of the boot CPU (slot 0).
pub fn init() {
    init_cpu(0);
}

/// Build + load the GDT/TSS of AP `cpu` (its `PerCpu` slot index). Called by
/// `smp::ap_entry` on the AP itself.
pub fn init_ap(cpu: usize) {
    init_cpu(cpu);
}

/// Build (once) and load this CPU's GDT + TSS. Must run on the CPU itself.
fn init_cpu(cpu: usize) {
    // This CPU's idle RSP0 (the scheduler re-points it on every switch). The
    // boot CPU's slot is filled by `smp::init_bsp`, an AP's by `smp::start_aps`
    // before the AP is released.
    let idle_top = unsafe { (*smp::per_cpu_ptr(cpu)).idle_kstack_top };

    // TSS: this CPU's own double-fault IST stack + idle RSP0.
    let tss_mutex = TSSES[cpu].call_once(|| {
        let mut tss = TaskStateSegment::new();
        let stack = unsafe { core::ptr::addr_of_mut!(DF_STACKS[cpu].0) } as *mut u8;
        let df_top = VirtAddr::new(unsafe { stack.add(DOUBLE_FAULT_STACK_SIZE) } as u64);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_top;
        tss.privilege_stack_table[0] = VirtAddr::new(idle_top);
        spin::Mutex::new(tss)
    });

    // SAFETY: the TSS lives in a `static`, so its address is stable for the
    // program's whole life and `tss_segment` can keep the required `'static`
    // reference. We only ever write RSP0 through the Mutex afterwards (the TSS
    // is never moved), which the GDT descriptor does not capture — it stores
    // the address itself.
    let tss: &'static TaskStateSegment =
        unsafe { &*(&*tss_mutex.lock() as *const TaskStateSegment) };

    let cpu_gdt = GDTS[cpu].call_once(|| build_gdt(tss));
    cpu_gdt.gdt.load();
    unsafe {
        CS::set_reg(cpu_gdt.sels.code_selector);
        load_tss(cpu_gdt.sels.tss_selector);
    }
    // Remember the selectors (the first CPU wins; every CPU builds the same
    // descriptor order, so the values are identical).
    SELECTORS.call_once(|| Selectors {
        code_selector: cpu_gdt.sels.code_selector,
        data_selector: cpu_gdt.sels.data_selector,
        user_data_selector: cpu_gdt.sels.user_data_selector,
        user_code_selector: cpu_gdt.sels.user_code_selector,
        tss_selector: cpu_gdt.sels.tss_selector,
    });
}

/// Build one CPU's GDT: null entry is implied, then code/data segments + TSS +
/// the ring-3 pair. The order is fixed so every CPU yields the same selectors
/// (and user_data sits exactly 8 below user_code, which SYSRET needs).
fn build_gdt(tss: &'static TaskStateSegment) -> CpuGdt {
    let mut gdt = GlobalDescriptorTable::new();
    let code_selector = gdt.append(Descriptor::kernel_code_segment());
    let data_selector = gdt.append(Descriptor::kernel_data_segment());
    let tss_selector = gdt.append(Descriptor::tss_segment(tss));
    // Ring-3 segments, appended after the (16-byte-wide) TSS descriptor.
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());
    CpuGdt {
        gdt,
        sels: Selectors {
            code_selector,
            data_selector,
            user_data_selector,
            user_code_selector,
            tss_selector,
        },
    }
}

/// Return the read-only selectors for this CPU (identical on every CPU).
pub fn selectors() -> &'static Selectors {
    SELECTORS.wait() // panics if the GDT was never initialized
}