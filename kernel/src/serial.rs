//! Minimal COM1 (serial port) UART driver used for kernel logging.

use uart_16550::backend::PioBackend;
use uart_16550::{Config, Uart16550Tty};

/// Initializes the serial port at COM1 (0x3F8) and returns a writable handle.
pub fn init_port() -> Uart16550Tty<PioBackend> {
    unsafe { Uart16550Tty::new_port(0x3F8, Config::default()) }
        .expect("failed to initialize COM1 serial port")
}

/// Global serial writer (safe to use from interrupt handlers that must not
/// allocate).
pub(crate) static SERIAL: spin::Mutex<Option<Uart16550Tty<PioBackend>>> = spin::Mutex::new(None);

/// Initialize the global serial writer once. Call at boot before any
/// `serial_writeln!`.
pub fn init() {
    let mut guard = SERIAL.lock();
    if guard.is_none() {
        *guard = Some(init_port());
    }
}

/// Write a formatted line to serial (re-using the global writer).
///
/// SAFETY (critical): runs the whole mutex+write with interrupts DISABLED.
/// The `SERIAL` lock is a spin::Mutex; with interrupts on, the 1 ms LAPIC
/// timer can preempt a task mid-write and the next task, trying to print,
/// spins forever on the lock the suspended task holds — a kernel-wide
/// deadlock (exposed hard by the PCI scan's long lines at 1000 Hz). Serial
/// writes are microseconds: blocking preemption for them is imperceptible
/// and matches the codebase's IF=0 resource-access discipline.
#[macro_export]
macro_rules! serial_writeln {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        x86_64::instructions::interrupts::without_interrupts(|| {
            if let Some(ref mut s) = *$crate::serial::SERIAL.lock() {
                let _ = writeln!(s, $($arg)*);
            }
        });
    }};
}

/// Write raw bytes to COM1 (used by the SYS_WRITE syscall handler).
/// Returns the number of bytes handed to the UART.
///
/// Note: `send_bytes_exact` busy-waits until the FIFO accepts everything
/// (same semantics as the `serial_writeln!` path). Fine for the short
/// strings the demo program writes.
pub fn write_bytes(buf: &[u8]) -> usize {
    // Same interrupt-safety rationale as `serial_writeln!`: the lock must
    // never be held across a preemption (see the macro docs above).
    let mut n = 0usize;
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = SERIAL.lock();
        match guard.as_mut() {
            Some(port) => {
                port.inner_mut().send_bytes_exact(buf);
                n = buf.len();
            }
            None => {}
        }
    });
    n
}