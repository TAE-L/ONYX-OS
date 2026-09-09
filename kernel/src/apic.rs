//! M9.6-A2: Local APIC + I/O APIC interrupt architecture.
//!
//! Replaces the legacy 8259 PIC as the interrupt path (lower latency: EOI is
//! a single MMIO write with no mutex; MSI-capable; per-CPU timer groundwork
//! for SMP):
//!   * LAPIC (MMIO at 0xFEE00000, discovered via IA32_APIC_BASE): spurious
//!     vector 0xFF, LINT0/LINT1/error LVTs masked, timer in periodic mode at
//!     1000 Hz driving preemption (vector 0x30), calibrated against the
//!     M9.6-A1 TSC clock (±0.1%, no PIT involvement).
//!   * IOAPIC (MMIO at 0xFEC00000, QEMU's fixed location): routes the ISA
//!     GSIs — 0 (PIT uptime clock) -> vector 0x20, 1 (PS/2 keyboard) -> 0x21,
//!     12 (PS/2 mouse) -> 0x2C — edge-triggered, active-high, physical dest.
//!   * The 8259 PIC stays initialized but fully masked (fallback EOI path is
//!     kept behind [`active`] for the pre-APIC boot window).
//!
//! Division of labor after A2: the PIT keeps counting 100 Hz uptime ticks
//! (rtc.rs wall clock + ticker depend on it) but no longer preempts; the
//! LAPIC timer owns preemption at 1000 Hz — 10x finer slices, one MMIO EOI
//! per IRQ, no lock in the hot path.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use x86_64::registers::model_specific::Msr;

use crate::{memory, pic, serial_writeln};

/// Device MMIO is accessed THROUGH the bootloader's physical-memory window:
/// virtual = `phys_offset + physical`. No fresh page-table entries are ever
/// created for MMIO — a new entry in a shared higher-half table can replace
/// an existing physical-memory-window mapping and silently alias kernel
/// accesses onto device registers (the root cause of the M9.6-A2 boot flake).
const LAPIC_PHYS: u64 = 0xFEE0_0000;
const IOAPIC_PHYS: u64 = 0xFEC0_0000;
/// Vector of the LAPIC-timer preemption interrupt.
pub const APIC_TIMER_VECTOR: u8 = 0x30;
/// Spurious interrupt vector (programmed into SIVR).
const SPURIOUS_VECTOR: u8 = 0xFF;
/// Target preemption frequency.
const TIMER_HZ: u64 = 1000;

/// LAPIC register offsets (32-bit, from the LAPIC base; Intel SDM Table 11-6
/// "Local APIC Register Address Map" — note the EOI at 0x0B0 and SIVR at
/// 0x0F0, NOT the read-only Remote Read/ISR slots that neighbor them).
const OFF_ID: usize = 0x020;
const OFF_EOI: usize = 0x0B0; // End-of-Interrupt (write 0 to ack)
const OFF_SIVR: usize = 0x0F0; // Spurious Interrupt Vector Register
const OFF_LVT_TIMER: usize = 0x320;
const OFF_LVT_LINT0: usize = 0x350;
const OFF_LVT_LINT1: usize = 0x360;
const OFF_LVT_ERROR: usize = 0x370;
const OFF_TIMER_ICR: usize = 0x380; // initial count
const OFF_TIMER_CCR: usize = 0x390; // current count
const OFF_TIMER_DIV: usize = 0x3E0; // divide configuration

/// True once the LAPIC/IOAPIC path is live (EOI goes to the LAPIC).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Milliseconds since the LAPIC timer started (incremented at 1000 Hz).
static MS: AtomicU64 = AtomicU64::new(0);
/// Window-relative virtual bases, installed by `init` (0 = not ready).
static LAPIC_VIRT: AtomicU64 = AtomicU64::new(0);
static IOAPIC_VIRT: AtomicU64 = AtomicU64::new(0);

#[inline]
unsafe fn lapic_read(off: usize) -> u32 {
    core::ptr::read_volatile((LAPIC_VIRT.load(Ordering::Relaxed) + (off as u64)) as *const u32)
}
#[inline]
unsafe fn lapic_write(off: usize, val: u32) {
    core::ptr::write_volatile((LAPIC_VIRT.load(Ordering::Relaxed) + (off as u64)) as *mut u32, val)
}
/// IOAPIC register access: write the register index to IOREGSEL, then
/// read/write the 32-bit window at +0x10.
unsafe fn ioapic_read(reg: u32) -> u32 {
    core::ptr::write_volatile(IOAPIC_VIRT.load(Ordering::Relaxed) as *mut u32, reg);
    core::ptr::read_volatile((IOAPIC_VIRT.load(Ordering::Relaxed) + 0x10) as *const u32)
}
unsafe fn ioapic_write(reg: u32, val: u32) {
    core::ptr::write_volatile(IOAPIC_VIRT.load(Ordering::Relaxed) as *mut u32, reg);
    core::ptr::write_volatile((IOAPIC_VIRT.load(Ordering::Relaxed) + 0x10) as *mut u32, val)
}

/// Is the APIC interrupt path live? Gates the EOI choice in IRQ handlers.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Milliseconds since the LAPIC timer started (0 before A2 init completes).
pub fn ms_since_boot() -> u64 {
    MS.load(Ordering::Relaxed)
}

/// Called by the LAPIC-timer IRQ handler (1000 Hz).
pub fn tick_ms() {
    let n = MS.fetch_add(1, Ordering::Relaxed);
    // Bring-up heartbeat: if these stop while the PIT diag keeps printing,
    // the LAPIC timer itself died; if they continue but sleepers stay
    // overdue, the wake pass is broken.
    if n % 500 == 499 {
        crate::serial_writeln!("[apic] ms={}", n + 1);
    }
}

/// End-of-interrupt for any LAPIC-delivered IRQ: a single MMIO write —
/// no lock, unlike the legacy PIC path.
pub fn eoi() {
    unsafe { lapic_write(OFF_EOI, 0) };
}

/// Route one IOAPIC redirection entry: `gsi` -> `vector`, edge-triggered,
/// active-high, fixed delivery, physical destination = this LAPIC.
unsafe fn route_gsi(gsi: u32, vector: u8) {
    let lapic_id = (lapic_read(OFF_ID) >> 24) & 0xFF;
    let low = vector as u32; // vector | delivery=fixed | pol=high | trig=edge | unmasked
    let high = lapic_id << 24; // physical destination
    ioapic_write(0x10 + 2 * gsi, low);
    ioapic_write(0x11 + 2 * gsi, high);
}

/// Read the LAPIC Error Status Register (raw; not cleared — diagnostics only).
pub fn error_status() -> u32 {
    unsafe { lapic_read(0x280) }
}

/// Bring up the APIC interrupt architecture. On any failure the kernel keeps
/// the legacy PIC path (interrupts are still off at this point) and simply
/// returns — every caller-visible behavior stays identical.
pub fn init() {
    // IA32_APIC_BASE (0x1B): bit 11 = APIC global enable, bits 12-51 = base.
    let msr = Msr::new(0x1B);
    let base = unsafe { msr.read() };
    if base & (1 << 11) == 0 {
        serial_writeln!("apic: APIC globally disabled in IA32_APIC_BASE - staying on PIC");
        return;
    }
    let lapic_phys = base & 0xFFFF_F000;
    // Access the LAPIC/IOAPIC through the bootloader's physical-memory
    // window (virtual = phys_offset + physical) — no new page-table entries.
    let Some(off) = memory::phys_offset() else {
        serial_writeln!("apic: physical-memory window unavailable - staying on PIC");
        return;
    };
    let Some(lapic_virt) = off.checked_add(lapic_phys) else {
        serial_writeln!("apic: LAPIC window address overflow - staying on PIC");
        return;
    };
    let Some(ioapic_virt) = off.checked_add(IOAPIC_PHYS) else {
        serial_writeln!("apic: IOAPIC window address overflow - staying on PIC");
        return;
    };
    LAPIC_VIRT.store(lapic_virt, Ordering::Relaxed);
    IOAPIC_VIRT.store(ioapic_virt, Ordering::Relaxed);

    // Bring-up runs with interrupts OFF: the still-firing PIT takes the
    // pic::send_eoi path (PICS spin lock) until mask_all() below, and MAIN
    // holding that same lock with IF=1 would self-deadlock if IRQ0 landed
    // mid-write (observed as a rare silent boot hang).
    x86_64::instructions::interrupts::without_interrupts(|| {
        unsafe {
            // Enable the LAPIC + set the spurious vector (bit 8 = enable).
            lapic_write(OFF_SIVR, u32::from(SPURIOUS_VECTOR) | (1 << 8));
            // Mask the pin LVTs we don't use (LINT0 ExtINT / LINT1 NMI legacy).
            lapic_write(OFF_LVT_LINT0, 1 << 16);
            lapic_write(OFF_LVT_LINT1, 1 << 16);
            lapic_write(OFF_LVT_ERROR, 1 << 16);

            // Route the ISA GSIs we use. The PIT keeps ticking for uptime and
            // the wall clock; it no longer preempts. Real ICH/QEMU wiring
            // delivers the 8254 output on GSI 2 (the ACPI MADT carries the
            // classic "IRQ0 -> GSI 2" interrupt-source override) and GSI 0 is
            // connected to nothing — routing both is free and machine-agnostic.
            // GSI 1/12 = PS/2 keyboard/mouse.
            route_gsi(0, 0x20);
            route_gsi(2, 0x20);
            route_gsi(1, 0x21);
            route_gsi(12, 0x2C);

            // Silence the legacy PIC entirely: every device IRQ now arrives
            // via the IOAPIC/LAPIC pair.
            pic::mask_all();
        }

        // EOI path switches to the LAPIC from here on (PIT ticks keep
        // flowing through the IOAPIC while we calibrate below).
        ACTIVE.store(true, Ordering::Relaxed);
    });

    // Calibrate the LAPIC timer against the PIT — both are QEMU
    // virtual-clock devices under TCG (and real clocks on hardware), so
    // this is correct in every environment. The previous TSC-anchored
    // window mis-scaled under TCG (TSC does not track virtual time), which
    // made the programmed interval ~3.8x too large: apic_ms lagged
    // pit_ticks by 1567 vs 600 in the M9.6-B3 regression — sleeps and
    // preemption all ran ~4x slow.
    let lapic_hz = unsafe {
        lapic_write(OFF_TIMER_DIV, 0xB); // divide by 1 (max resolution)
        lapic_write(OFF_LVT_TIMER, (1 << 16) | u32::from(APIC_TIMER_VECTOR)); // masked one-shot
        lapic_write(OFF_TIMER_ICR, 0xFFFF_FFFF);
        // 50 PIT ticks = 500 virtual ms. The window needs IF=1 so PIT IRQ0
        // keeps flowing — but `init` may be called with IF either state
        // (boot path calls it after "scheduler active", i.e. IF=1), so SAVE
        // and RESTORE the flag. Blindly leaving IF=0 here silently froze
        // the whole kernel right after init returned (M9.6-B3 regression).
        let if_was_on = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::enable();
        let t0 = crate::pit::ticks();
        let mut guard = 0u64;
        while crate::pit::ticks() - t0 < 50 {
            guard += 1;
            if guard > 4_000_000_000 {
                break; // PIT dead — fall through, the sanity check below catches it
            }
            core::hint::spin_loop();
        }
        if if_was_on {
            x86_64::instructions::interrupts::enable();
        } else {
            x86_64::instructions::interrupts::disable();
        }
        let ccr = lapic_read(OFF_TIMER_CCR) as u64;
        lapic_write(OFF_LVT_TIMER, 1 << 16); // mask while reprogramming
        (0xFFFF_FFFFu64 - ccr) * 2 // ticks per 500 virtual ms -> per second
    };
    if lapic_hz < 1_000_000 {
        serial_writeln!("apic: timer calibration bogus ({} Hz) - staying on PIC", lapic_hz);
        ACTIVE.store(false, Ordering::Relaxed);
        return;
    }
    let interval = (lapic_hz / TIMER_HZ).max(16) as u32;

    // All diagnostics + MMIO reads happen BEFORE the preemption clock goes
    // live: once the periodic timer runs, every statement below can be
    // interrupted mid-way, so nothing that touches device registers should
    // remain after this point.
    serial_writeln!("apic: LAPIC at {lapic_phys:#x} enabled (version {:#x})", unsafe {
        lapic_read(0x030) & 0xFF
    });
    serial_writeln!(
        "apic: timer calibrated: {} LAPIC ticks/s -> interval {} ({} Hz preemption)",
        lapic_hz,
        interval,
        TIMER_HZ
    );
    serial_writeln!("apic: IOAPIC routed GSI0+2->0x20 GSI1->0x21 GSI12->0x2C");
    let (ver, entries) = unsafe { (ioapic_read(0x01) & 0xFF, ((ioapic_read(0x01) >> 16) & 0xFF) + 1) };
    serial_writeln!("apic: IOAPIC version {ver:#x}, {entries} redirection entries");
    serial_writeln!("apic: PIC fully masked - APIC-only interrupt path live");

    // Program the periodic timer atomically (IF=0) and LAST: an interrupt
    // arriving between the ICR and LVT writes would fire the one-shot
    // mid-setup, and after these writes every 1 ms lands in the handlers.
    //
    // Closed-loop calibration: the one-shot measurement above can land in a
    // different clock domain than QEMU's periodic-LAPIC delivery (observed
    // ~3.4x skew under TCG: apic_ms advanced 3.43x faster than PIT ticks).
    // So: program the first-guess interval, count how many ticks ACTUALLY
    // fire in 50 PIT ticks (= 500 virtual ms), and rescale the interval by
    // the measured ratio. Self-correcting in any clock domain; on real
    // hardware the measured ratio is ~1.0 and the interval barely moves.
    // First-guess interval, programmed atomically.
    serial_writeln!("apic: closed-loop A (programming first guess)");
    let first_guess = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let interval = ((lapic_hz / TIMER_HZ).max(16)) as u32;
        lapic_write(OFF_TIMER_DIV, 0xB);
        lapic_write(OFF_TIMER_ICR, interval);
        // Periodic mode (bit 17), vector 0x30, unmasked.
        lapic_write(OFF_LVT_TIMER, u32::from(APIC_TIMER_VECTOR) | (1 << 17));
        interval
    });

    // Measure with interrupts ENABLED (the PIT and LAPIC must both fire to
    // be compared): count how many LAPIC ticks actually fire in 50 PIT
    // ticks (= 500 virtual ms). ~3.4x domain skew observed under TCG.
    let p0 = crate::pit::ticks();
    let m0 = ms_since_boot();
    let mut guard = 0u64;
    while crate::pit::ticks() - p0 < 50 {
        guard += 1;
        if guard > 4_000_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
    let dm = ms_since_boot() - m0;
    let interval = if dm > 10 && dm != 50 {
        // fired `dm` ticks where we wanted 50: rescale proportionally
        // (guarded against a zero/absurd measurement).
        (((first_guess as u64) * dm) / 50).max(16) as u32
    } else {
        first_guess
    };

    // Reprogram with the corrected interval atomically (IF=0): an interrupt
    // arriving between the ICR and LVT writes would fire the timer mid-setup.
    serial_writeln!("apic: closed-loop B dm={}", dm);
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        lapic_write(OFF_LVT_TIMER, 1 << 16); // mask while reprogramming
        lapic_write(OFF_TIMER_ICR, interval);
        lapic_write(OFF_LVT_TIMER, u32::from(APIC_TIMER_VECTOR) | (1 << 17));
    });
    serial_writeln!(
        "apic: periodic closed-loop: {dm} ticks/500ms -> interval {interval} (1000 Hz)"
    );
}
