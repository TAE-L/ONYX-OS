//! 8259 Programmable Interrupt Controller (PIC) driver.
//!
//! We remap the PIC's hardware interrupts from their default (overlapping)
//! vectors 0x08–0x0F to the standard 0x20–0x2F range so they don't collide
//! with CPU exceptions. For M1 we keep the IRQs masked; the keyboard/timer
//! milestones unmask specific ones.

use pic8259::ChainedPics;
use spin;

/// Primary PIC (master) interrupt offset.
pub const PIC_1_OFFSET: u8 = 0x20;
/// Secondary PIC (slave) interrupt offset.
pub const PIC_2_OFFSET: u8 = 0x28;

/// The two chained 8259 PICs, remapped to 0x20..0x2F.
static PICS: spin::Mutex<ChainedPics> = spin::Mutex::new(unsafe {
    ChainedPics::new_contiguous(PIC_1_OFFSET)
});

/// Initialize both PICs with their remapped offsets. Call once at boot.
pub fn init() {
    unsafe {
        PICS.lock().initialize();
    }
}

/// Acknowledge an interrupt from the PIC (send End-Of-Interrupt).
/// Always call at the end of every hardware-IRQ handler.
pub fn send_eoi(interrupt_id: u8) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_id);
    }
}

/// Enable (unmask) a hardware IRQ line (0–15). A clear bit means enabled.
pub fn enable_irq(irq: u8) {
    unsafe {
        let mut pics = PICS.lock();
        let mut masks = pics.read_masks();
        if irq < 8 {
            masks[0] &= !(1u8 << irq);
        } else {
            masks[1] &= !(1u8 << (irq - 8));
        }
        pics.write_masks(masks[0], masks[1]);
    }
}

/// Disable (mask) a hardware IRQ line (0–15). A set bit means masked.
pub fn disable_irq(irq: u8) {
    unsafe {
        let mut pics = PICS.lock();
        let mut masks = pics.read_masks();
        if irq < 8 {
            masks[0] |= 1u8 << irq;
        } else {
            masks[1] |= 1u8 << (irq - 8);
        }
        pics.write_masks(masks[0], masks[1]);
    }
}

/// Mask every legacy PIC IRQ line (used by the APIC switch in M9.6-A2: all
/// device interrupts arrive via the IOAPIC/LAPIC from then on).
pub fn mask_all() {
    unsafe {
        PICS.lock().write_masks(0xFF, 0xFF);
    }
}