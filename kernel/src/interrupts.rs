//! Interrupt Descriptor Table (IDT) setup + exception handlers.
//!
//! We register handlers for the CPU exceptions (vectors 0–31) so that instead
//! of a silent triple-fault we print what went wrong and halt. Hardware-IRQ
//! handlers (keyboard/timer) come in a later milestone and send an EOI.

use crate::serial_writeln;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

/// Our IDT (256 entries, but we only populate what we handle).
static IDT: spin::Lazy<InterruptDescriptorTable> = spin::Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    // CPU exceptions (vectors 0–31)
    idt.divide_error.set_handler_fn(divide_error_handler);
    idt.debug.set_handler_fn(debug_handler);
    idt.non_maskable_interrupt.set_handler_fn(nmi_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.overflow.set_handler_fn(overflow_handler);
    idt.bound_range_exceeded.set_handler_fn(bound_range_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.device_not_available.set_handler_fn(device_not_available_handler);
    // Double fault runs on its own dedicated IST stack (index 0) from the TSS
    // so that a fault caused by stack corruption can still be handled.
    // `set_stack_index` is unsafe: it must point into the TSS IST we set up.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(0);
    }
    idt.invalid_tss.set_handler_fn(invalid_tss_handler);
    idt.segment_not_present.set_handler_fn(segment_not_present_handler);
    idt.stack_segment_fault.set_handler_fn(stack_segment_fault_handler);
    idt.general_protection_fault.set_handler_fn(general_protection_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.x87_floating_point.set_handler_fn(x87_floating_point_handler);
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.machine_check.set_handler_fn(machine_check_handler);
    idt.simd_floating_point.set_handler_fn(simd_floating_point_handler);
    idt.virtualization.set_handler_fn(virtualization_handler);
    idt.security_exception.set_handler_fn(security_exception_handler);
    // Hardware interrupts (IRQs remapped by the PIC to vectors 0x20..0x2F).
    idt[0x20].set_handler_fn(timer_handler); // PIT: uptime tick (pre-A2 only)
    idt[0x21].set_handler_fn(keyboard_handler); // PS/2 keyboard
    idt[0x2C].set_handler_fn(mouse_handler); // PS/2 mouse (IRQ 12)
    // M9.6-A2: LAPIC timer @1000 Hz — the preemption source once the APIC
    // stack is live. Also the LAPIC spurious vector, which needs an entry
    // (it is acknowledged by returning; no EOI for a spurious interrupt).
    idt[0x30].set_handler_fn(apic_timer_handler);
    idt[0xFF].set_handler_fn(spurious_handler);
    idt
});

/// Initialize the IDT (load it on this CPU).
pub fn init() {
    IDT.load();
}

/// A fatal CPU exception occurred: halt forever with interrupts off.
/// (Kept simple for M1 — a real unwinder/backtrace is added later.)
fn crash() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn divide_error_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Divide Error (#DE)");
    crash();
}

extern "x86-interrupt" fn debug_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Debug (#DB)");
    crash();
}

extern "x86-interrupt" fn nmi_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Non-Maskable Interrupt (NMI)");
    crash();
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    serial_writeln!(
        "EXCEPTION: Breakpoint (#BP) at RIP={:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn overflow_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Overflow (#OF)");
    crash();
}
extern "x86-interrupt" fn bound_range_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Bound Range Exceeded (#BR)");
    crash();
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    serial_writeln!(
        "EXCEPTION: Invalid Opcode (#UD) at RIP={:#x}",
        frame.instruction_pointer.as_u64()
    );
    crash();
}

extern "x86-interrupt" fn device_not_available_handler(frame: InterruptStackFrame) {
    serial_writeln!(
        "EXCEPTION: Device Not Available (#NM) at RIP={:#x} \
         — FPU state was not carried across the switch (scheduler bug)",
        frame.instruction_pointer.as_u64()
    );
    crash();
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _error_code: u64) -> ! {
    serial_writeln!(
        "EXCEPTION: Double Fault (#DF) at RIP={:#x}",
        frame.instruction_pointer.as_u64()
    );
    crash();
}

extern "x86-interrupt" fn invalid_tss_handler(_: InterruptStackFrame, _e: u64) {
    serial_writeln!("EXCEPTION: Invalid TSS (#TS)");
    crash();
}

extern "x86-interrupt" fn segment_not_present_handler(_: InterruptStackFrame, _e: u64) {
    serial_writeln!("EXCEPTION: Segment Not Present (#NP)");
    crash();
}

extern "x86-interrupt" fn stack_segment_fault_handler(_: InterruptStackFrame, _e: u64) {
    serial_writeln!("EXCEPTION: Stack-Segment Fault (#SS)");
    crash();
}

extern "x86-interrupt" fn general_protection_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    serial_writeln!(
        "EXCEPTION: General Protection Fault (#GP) err={:#x} at RIP={:#x}",
        error_code,
        frame.instruction_pointer.as_u64()
    );
    crash();
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    serial_writeln!(
        "EXCEPTION: Page Fault (#PF) err={:?} at RIP={:#x}",
        error_code,
        frame.instruction_pointer.as_u64()
    );
    crash();
}

extern "x86-interrupt" fn x87_floating_point_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: x87 FPU (#MF)");
    crash();
}

extern "x86-interrupt" fn alignment_check_handler(_: InterruptStackFrame, _e: u64) {
    serial_writeln!("EXCEPTION: Alignment Check (#AC)");
    crash();
}

extern "x86-interrupt" fn machine_check_handler(_: InterruptStackFrame) -> ! {
    serial_writeln!("EXCEPTION: Machine Check (#MC)");
    crash();
}

extern "x86-interrupt" fn simd_floating_point_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: SIMD Floating-Point (#XM)");
    crash();
}

extern "x86-interrupt" fn virtualization_handler(_: InterruptStackFrame) {
    serial_writeln!("EXCEPTION: Virtualization (#VC)");
    crash();
}

extern "x86-interrupt" fn security_exception_handler(_: InterruptStackFrame, _e: u64) {
    serial_writeln!("EXCEPTION: Security Exception (#SX)");
    crash();
}

// Double-fault ISR registered in the IDT points here, using our dedicated
// IST stack from the GDT/TSS so that even a stack-smashed fault can be caught.
pub fn double_fault_isr() {
    serial_writeln!("DOUBLE FAULT on dedicated IST stack");
    crash();
}

// ---------------------------------------------------------------------------
// Hardware interrupt handlers (vectors 0x20..0x2F + 0x30).
// Each must acknowledge the interrupt before returning: through the LAPIC
// once the APIC stack is live (M9.6-A2), through the 8259 PIC in the boot
// window before that (interrupts are off until apic::init completes, so the
// PIC branch only ever runs pre-A2).
// ---------------------------------------------------------------------------

/// EOI for a device IRQ routed through whichever controller is live.
fn device_eoi(legacy_pic_irq_vector: u8) {
    if crate::apic::active() {
        crate::apic::eoi();
    } else {
        crate::pic::send_eoi(legacy_pic_irq_vector);
    }
}

/// PIT timer (IRQ 0, vector 0x20): uptime/wall-clock tick only. Preemption
/// moved to the LAPIC timer in M9.6-A2 — rtc.rs and the ticker depend on
/// these 100 Hz ticks, so the PIT keeps counting through the IOAPIC.
extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    crate::pit::tick();
    device_eoi(crate::pic::PIC_1_OFFSET);
}

/// LAPIC timer (vector 0x30, 1000 Hz): the preemption source.
extern "x86-interrupt" fn apic_timer_handler(_frame: InterruptStackFrame) {
    // B5: lateness vs. the 1 ms schedule measured at entry; the service-cost
    // record happens after EOI but BEFORE preempt — switch-away time belongs
    // to the descheduled task, not to the interrupt.
    let t0 = crate::perf::irq_timer_enter();
    crate::apic::tick_ms();
    crate::apic::eoi();
    crate::perf::irq_timer_exit(t0);
    crate::scheduler::preempt();
}
/// LAPIC spurious interrupt (vector 0xFF): acknowledge by returning — a
/// spurious interrupt must NOT be EOI'd (Intel SDM §11.9).
extern "x86-interrupt" fn spurious_handler(_frame: InterruptStackFrame) {}

/// PS/2 keyboard (IRQ 1).
extern "x86-interrupt" fn keyboard_handler(_frame: InterruptStackFrame) {
    let t0 = crate::perf::irq_kbd_enter();
    crate::keyboard::handle_irq();
    device_eoi(crate::pic::PIC_1_OFFSET + 1);
    crate::perf::irq_kbd_exit(t0);
}

/// PS/2 mouse (IRQ 12, on the slave PIC -> vector 0x2C).
extern "x86-interrupt" fn mouse_handler(_frame: InterruptStackFrame) {
    let t0 = crate::perf::irq_mouse_enter();
    crate::mouse::handle_irq();
    device_eoi(crate::pic::PIC_2_OFFSET + 4);
    crate::perf::irq_mouse_exit(t0);
}