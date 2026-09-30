//! PS/2 mouse driver (IRQ 12).
//!
//! Implemented directly instead of using the `ps2-mouse` crate: that crate
//! (0.1.4) depends on `x86_64` 0.14, which does not compile on the 2026
//! nightly. The protocol is small — enable the aux device in the i8042
//! controller's config byte, then enable packet streaming, then decode 3-byte
//! packets:
//!
//! ```text
//! byte 0: | YOVF | XOVF | YSGN | XSGN | bm1b0 | bm1b1 | bm1b2 | 1 |
//! byte 1: X movement (signed; sign bit = XSGN)
//! byte 2: Y movement (signed; sign bit = YSGN)
//! ```

use crate::serial_writeln;
use spin::Mutex;
use x86_64::instructions::port::Port;

/// i8042 command port (status/commands).
const CMD_PORT: u16 = 0x64;
/// i8042 data port.
const DATA_PORT: u16 = 0x60;

const OUTPUT_BUFFER_FULL: u8 = 0x01;
const INPUT_BUFFER_EMPTY: u8 = 0x02;

static CMD: Mutex<Port<u8>> = Mutex::new(Port::new(CMD_PORT));
static DATA: Mutex<Port<u8>> = Mutex::new(Port::new(DATA_PORT));

/// Last completed packet. Written by the ISR (interrupts off) and read by the
/// mouse reader task (interrupts off around the read).
#[derive(Copy, Clone)]
struct Packet {
    dx: i16,
    dy: i16,
    left: bool,
    right: bool,
}

static mut PACKET: Packet = Packet { dx: 0, dy: 0, left: false, right: false };

// Packet assembly state (ISR only, interrupts off).
static mut IDX: usize = 0;
static mut FLAGS: u8 = 0;
static mut DX: i16 = 0;

/// Enable the mouse and start packet streaming. Call before enabling IRQ 12.
pub fn init() -> Result<(), &'static str> {
    // Read the controller config byte, set the "aux enable" bit (1) and clear
    // the "disable clock" bit (5), then write it back.
    write_command(0x20)?; // GET_STATUS_BYTE
    let status = read_data()? | 0x02;
    write_command(0x60)?; // SET_STATUS_BYTE
    write_data(status & !0x20)?;

    // (Re)set defaults, then configure and enable packet streaming. Each
    // mouse command is routed through `0xD4` and acknowledged with `0xFA`.
    send_command(0xF6)?; // SetDefaults
    // Sample rate 200 Hz (gaming standard for PS/2): packets arrive 3-5x
    // more often with smaller deltas -> visibly smoother cursor motion and
    // almost no catch-up drift after the hand stops.
    send_command(0xF3)?; // SetSampleRate
    send_command(200)?;  // 200 samples / second
    send_command(0xF4)?; // EnablePacketStreaming
    Ok(())
}

/// Called from the IRQ-12 handler: assemble the 3-byte packet.
///
/// M9.8-(d): the packet-assembly state (`IDX`/`FLAGS`/`DX`/`PACKET`) is shared
/// with `read_packet` (which any task on any CPU may call), so the critical
/// section is the assembly only — it is kept short and released *before* the
/// input-ring push and the cursor move, since both of those take locks of their
/// own (a nested KSL acquisition would be a reentrancy panic).
pub fn handle_irq() {
    let byte = {
        let mut data = DATA.lock();
        unsafe { data.read() }
    };
    let mut completed: Option<(i16, i16, u8)> = None;
    {
        let _ksl = crate::ksl::lock();
        unsafe {
            match IDX {
                0 => {
                    // Byte 0: valid packets always have bit 3 set.
                    if (byte & 0x08) != 0 {
                        FLAGS = byte;
                        IDX = 1;
                    }
                }
                1 => {
                    let sign = (FLAGS & 0x10) != 0; // XSGN
                    DX = if sign {
                        (byte as u16 | 0xFF00) as i16
                    } else {
                        byte as i16
                    };
                    IDX = 2;
                }
                2 => {
                    let sign = (FLAGS & 0x20) != 0; // YSGN
                    let dy = if sign {
                        (byte as u16 | 0xFF00) as i16
                    } else {
                        byte as i16
                    };
                    PACKET.dx = DX;
                    PACKET.dy = dy;
                    PACKET.left = (FLAGS & 0x01) != 0;
                    PACKET.right = (FLAGS & 0x02) != 0;
                    IDX = 0;
                    // Button bitmask for the input ring, computed here while
                    // the assembly state is still owned by this section.
                    let buttons = (if (FLAGS & 0x01) != 0 {
                        crate::input::BTN_LEFT
                    } else {
                        0
                    }) | (if (FLAGS & 0x02) != 0 {
                        crate::input::BTN_RIGHT
                    } else {
                        0
                    });
                    completed = Some((DX, dy, buttons));
                }
                _ => IDX = 0,
            }
        }
    }
    if let Some((dx, dy, buttons)) = completed {
        // B4: raw mouse event into the unified input ring (screen-space
        // deltas + held-button bitmask). Pushed from IRQ context.
        crate::input::push_mouse(dx as i32, -(dy as i32), buttons);
        // Move the on-screen cursor immediately in IRQ context: lowest
        // possible input latency (a few hundred byte writes). PS/2 reports
        // "up" as positive Y, but screen coordinates grow downward — negate Y
        // to match. X is already correct.
        crate::framebuffer::move_cursor(dx as i32, -(dy as i32));
    }
}

/// Reader task body: report movement / button events. The position is the
/// framebuffer cursor's ABSOLUTE coordinates, so the serial log directly
/// shows the cursor animation: `pos` must track the injected mouse deltas
/// (verifiable headless via `test-input.ps1`).
///
/// P2 - THIS USED TO BE A BUSY-SPIN AND A REAL BOTTLENECK. Three problems, all
/// fixed here:
///
///  1. `loop { ... spin_loop() }` never blocked, so this was an always-runnable
///     Normal task permanently occupying a CPU - the same starvation shape as
///     `ticker_task`, found by the hop-B attribution counter. Moving the mouse
///     made it WORSE, because the faster the input the more it looped.
///  2. It printed on EVERY packet. `serial_writeln!` takes a global lock and
///     pushes bytes out the UART one at a time, so a fast mouse could spend a
///     measurable fraction of the CPU inside the serial driver.
///  3. It woke on nothing - it just re-read the ring, so the "work" was
///     re-doing the same comparison thousands of times per millisecond.
///
/// Now it parks instead of spinning. The sleep is a real block (not a yield),
/// which is safe here: this task owns no lock and holds no critical section
/// across the park. Button state is reported IMMEDIATELY and
/// unconditionally (a press must never be dropped or delayed - `test-input.ps1`
/// asserts on it); only the high-frequency position stream is rate-limited.
const MOUSE_LOG_INTERVAL_MS: u64 = 100;
/// Park length when nothing changed at all. Long, because there is nothing to
/// observe until the next move - the hardware cursor moves on the IRQ, this
/// task only REPORTS.
const MOUSE_IDLE_PARK_MS: u64 = 40;
/// Park length right after a change, so a burst of moves still reports promptly.
const MOUSE_ACTIVE_PARK_MS: u64 = 5;

pub fn run_reader() {
    let mut last: (i16, i16, bool, bool) = (0, 0, false, false);
    let mut last_logged = 0u64;
    loop {
        let cur = read_packet();
        let moved = cur.0 != 0 || cur.1 != 0;
        let buttons = cur.2 || cur.3;
        let changed = cur != last;
        if changed {
            last = cur;
            if moved || buttons {
                let (x, y) = crate::framebuffer::cursor_pos();
                let now = crate::apic::ms_since_boot();
                // Rate-limit the POSITION stream only. A button press/release is
                // rare and semantically important, so it always reports at once
                // even inside the quiet window.
                let due = now.saturating_sub(last_logged) >= MOUSE_LOG_INTERVAL_MS;
                if buttons || due {
                    last_logged = now;
                    serial_writeln!(
                        "[mouse] pos=({},{}) dx={} dy={} L={} R={}",
                        x,
                        y,
                        cur.0,
                        cur.1,
                        cur.2 as u8,
                        cur.3 as u8
                    );
                }
            }
        }
        crate::scheduler::sleep_kernel(if changed {
            MOUSE_ACTIVE_PARK_MS
        } else {
            MOUSE_IDLE_PARK_MS
        });
    }
}

/// Consume the latest completed packet (dx/dy are cleared; buttons persist).
/// Public so the position-reporting task in main.rs can observe raw events.
pub fn read_packet() -> (i16, i16, bool, bool) {
    // M9.8-(d): KSL instead of disable()/enable(). The old pair was wrong on
    // two counts: it re-enabled interrupts unconditionally (destroying an
    // outer IF=0 section such as a syscall), and IF=0 only excludes IRQs on
    // *this* CPU — the mouse IRQ may be delivered elsewhere. The guard
    // restores the caller's interrupt state and serializes against the IRQ.
    let _ksl = crate::ksl::lock();
    let p = unsafe { PACKET };
    unsafe {
        PACKET.dx = 0;
        PACKET.dy = 0;
    }
    (p.dx, p.dy, p.left, p.right)
}

fn write_command(value: u8) -> Result<(), &'static str> {
    let mut cmd = CMD.lock();
    for _ in 0..100_000 {
        if (unsafe { cmd.read() } & INPUT_BUFFER_EMPTY) == 0 {
            unsafe {
                cmd.write(value);
            }
            return Ok(());
        }
    }
    Err("write_command timeout")
}

fn write_data(value: u8) -> Result<(), &'static str> {
    let mut data = DATA.lock();
    for _ in 0..100_000 {
        if (unsafe { CMD.lock().read() } & INPUT_BUFFER_EMPTY) == 0 {
            unsafe {
                data.write(value);
            }
            return Ok(());
        }
    }
    Err("write_data timeout")
}

/// Route a command to the mouse and verify the `0xFA` acknowledgement.
fn send_command(value: u8) -> Result<(), &'static str> {
    write_command(0xD4)?; // next byte goes to the aux/mouse device
    write_data(value)?;
    let ack = read_data()?;
    if ack == 0xFA {
        Ok(())
    } else {
        Err("mouse did not ack")
    }
}

fn read_data() -> Result<u8, &'static str> {
    for _ in 0..100_000 {
        if (unsafe { CMD.lock().read() } & OUTPUT_BUFFER_FULL) != 0 {
            let mut data = DATA.lock();
            return Ok(unsafe { data.read() });
        }
    }
    Err("read_data timeout")
}