//! PIT (8254 Programmable Interval Timer) driver.
//!
//! Generates the periodic IRQ-0 interrupt that drives preemptive
//! multitasking. Channel 0 runs in mode 3 (square wave) at 100 Hz.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// PIT tick frequency: one interrupt every 10 ms.
pub const TICK_HZ: u32 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program PIT channel 0 to fire `TICK_HZ` times per second.
pub fn init() {
    // The PIT's base clock is 1.193182 MHz.
    let divisor: u16 = (1193182 / TICK_HZ) as u16;
    let mut command_port = Port::new(0x43);
    let mut data_port = Port::new(0x40);
    unsafe {
        // Channel 0, lobyte-then-hibyte access, mode 3 (square wave), binary.
        command_port.write(0x36u8);
        data_port.write((divisor & 0xFF) as u8);
        data_port.write((divisor >> 8) as u8);
    }
}

/// Called by the timer interrupt handler on every tick.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// The number of ticks since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}