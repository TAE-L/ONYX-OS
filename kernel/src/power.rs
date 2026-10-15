//! Machine power control: shut down and restart.
//!
//! # Why this is its own module
//!
//! The sign-in screen is the only caller, but a power action is not UI code — it
//! is a firmware hand-off that has to keep working when the UI is rewritten. It
//! lives here so the (irreversible, untestable-in-place) port writes are in one
//! auditable file rather than scattered through a drawing module.
//!
//! # QEMU-only, deliberately
//!
//! Everything here is a well-known **emulator** power interface, not a
//! specification any real PC is obliged to honour:
//!
//! * **Shut down** writes the ACPI-style "enter S5" value to the PM1a control
//!   register QEMU exposes at `0x604` (mirrored at `0xB004`). QEMU turns that
//!   into a clean guest power-off.
//! * **Restart** pulses the ACPI/PCI reset control register at `0xCF9` with
//!   `0x0E` — the "cold boot" request every x86 firmware honours, and the one
//!   QEMU maps straight onto `system_reset`.
//!
//! On real hardware the ACPI route would require programming `_S5`/`_GPE`
//! blocks found by walking the DSDT, and the reboot register is not guaranteed
//! to exist at all. The project is QEMU-only (see `docs/PLAN.md`), so this is
//! the correct scope — but it is a real limitation, not a portable
//! implementation, and `docs/PLAN.md` should grow a real ACPI power-off stage
//! before this is ever run on a physical machine.
//!
//! # Neither call returns on success
//!
//! Both hand control to the firmware. A caller that needs to finish work first
//! (flush serial, paint a final frame) must do it BEFORE calling — which is why
//! `signin::on_click` closes the menu and repaints before invoking these.

use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::instructions::port::Port;

/// Set once either action has been invoked.
///
/// Guards against a second attempt: these registers are write-only side-effect
/// ports, and a double power-off from a queued second click is a confusing way
/// to end a session.
static POWER_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Has a power action already been invoked?
pub fn requested() -> bool {
    POWER_REQUESTED.load(Ordering::Acquire)
}

/// Request a clean shutdown. Does not return if the request is honoured.
pub fn shutdown() -> ! {
    crate::serial_writeln!("[power] requesting shutdown");
    if POWER_REQUESTED.swap(true, Ordering::AcqRel) {
        halt_forever();
    }
    unsafe {
        // Log FIRST, so the "about to power off" line is on the wire before the
        // machine stops. `crate::serial_writeln!` already runs with interrupts off.
        let mut pm = Port::<u16>::new(0x604);
        // SLP_TYP = S5, SLP_EN = 1. Both bits set is the canonical S5 request.
        pm.write(0x2000);
        // Mirror at the alternate QEMU PM base for older machine types.
        let mut pm_alt = Port::<u16>::new(0xB004);
        pm_alt.write(0x2000);
    }
    // Reaching here means neither register took the request (a machine without
    // the emulator's PM interface). Say so, rather than appearing to hang.
    crate::serial_writeln!("[power] shutdown port not honoured - halting");
    halt_forever();
}

/// Request a restart. Does not return if the request is honoured.
pub fn restart() -> ! {
    crate::serial_writeln!("[power] requesting restart");
    if POWER_REQUESTED.swap(true, Ordering::AcqRel) {
        halt_forever();
    }
    unsafe {
        // 0xCF9 is the ACPI/PCI reset control register. The conventional
        // full-reset sequence sets bit 1 (system reset) with bits 2 (power off)
        // and 3 (PCI reset) also set, so a single write is enough on QEMU.
        let mut reset = Port::<u8>::new(0xCF9);
        reset.write(0x0E);
    }
    crate::serial_writeln!("[power] restart port not honoured - halting");
    halt_forever();
}

/// Park forever with interrupts off, if power control was not available.
///
/// `hlt` in a tight loop is the correct terminal state: it consumes no CPU, so
/// the rest of the kernel keeps running and the session stays inspectable. If
/// interrupts are left ON the machine idles in HLT and only wakes on the 1 kHz
/// LAPIC tick, which is busy-ish but harmless; the explicit disable keeps it
/// genuinely still.
fn halt_forever() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}