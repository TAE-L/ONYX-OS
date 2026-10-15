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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Left-button state as of the PREVIOUS completed packet.
///
/// P6c: button EDGE detection for the sign-in screen. It cannot be derived from
/// `PACKET.left`, because that field has already been overwritten with the new
/// packet's state by the time anything outside the assembly section runs — so
/// comparing against it would always read "no change" and no click would ever
/// be delivered. This is captured before the overwrite, while the previous
/// value is still intact, and is ISR-only like the rest of the packet state.
static mut PREV_LEFT: bool = false;

// Packet assembly state (ISR only, interrupts off).
static mut IDX: usize = 0;
static mut FLAGS: u8 = 0;
static mut DX: i16 = 0;

/// Enable the mouse and start packet streaming. Call before enabling IRQ 12.
///
/// P6c (user report: "click the QEMU window while it is booting and the cursor
/// never works again until I reboot"): the sequence used to abort on the FIRST
/// bad acknowledgement, because `?` propagated out of `init`. A single stray
/// byte in the i8042 output buffer - and clicking the window is exactly what
/// produces one, since the click is a motion/button packet - was enough to make
/// `send_command` read that byte instead of the `0xFA` ack. `init` then returned
/// early, **before `0xF4`**, so the mouse was left with packet streaming
/// disabled: it never sent another packet, and nothing in the system ever
/// re-ran the sequence. A dead pointer until reboot.
///
/// Three changes, in order of importance:
/// 1. The steps BEFORE `0xF4` no longer abort the sequence. Only the final
///    enable is load-bearing, so it is the only one whose result is returned.
/// 2. Streaming is switched OFF first (`0xF5`), so the mouse cannot inject new
///    bytes into the middle of the handshake we are running.
/// 3. The output buffer is drained before the config-byte exchange, discarding
///    the POST `0xAA` and any already-in-flight motion byte.
pub fn init() -> Result<(), &'static str> {
    let _ = send_command(0xF5); // DisablePacketStreaming - stop the byte flood
    drain_output_buffer();

    // Controller config byte: enable the aux port and clear "disable clock".
    // Best effort - a failure here is survivable, `0xF4` below is not optional.
    if write_command(0x20).is_ok() {
        if let Ok(status) = read_data() {
            let _ = write_command(0x60); // SET_STATUS_BYTE
            let _ = write_data((status | 0x02) & !0x20);
        }
    }
    drain_output_buffer();

    // (Re)set defaults, then configure and enable packet streaming. Each mouse
    // command is routed through `0xD4` and acknowledged with `0xFA`.
    let _ = send_command(0xF6); // SetDefaults
    // Sample rate 200 Hz (gaming standard for PS/2): packets arrive 3-5x
    // more often with smaller deltas -> visibly smoother cursor motion and
    // almost no catch-up drift after the hand stops.
    let _ = send_command(0xF3); // SetSampleRate
    let _ = send_command(200); // 200 samples / second
    // The one step that must succeed: without it the mouse is silent forever.
    send_command(0xF4) // EnablePacketStreaming
}

/// Discard every byte currently queued in the i8042 output buffer.
///
/// The lock guards are taken and released one at a time (the `CMD` temporary
/// drops at the end of its statement), so this cannot invert against
/// `read_data`/`write_data`, which also never hold both at once.
fn drain_output_buffer() {
    for _ in 0..64 {
        let full = (unsafe { CMD.lock().read() } & OUTPUT_BUFFER_FULL) != 0;
        if !full {
            break;
        }
        let _ = unsafe { DATA.lock().read() };
    }
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
    let mut completed: Option<(i16, i16, u8, bool)> = None;
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
                    // Snapshot the button state BEFORE overwriting it, so the
                    // edge check below sees the transition rather than
                    // comparing the new state against itself.
                    let left_was_down = PREV_LEFT;
                    PACKET.left = (FLAGS & 0x01) != 0;
                    PACKET.right = (FLAGS & 0x02) != 0;
                    PREV_LEFT = PACKET.left;
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
                    completed = Some((DX, dy, buttons, left_was_down));
                }
                _ => IDX = 0,
            }
        }
    }
    if let Some((dx, dy, buttons, left_was_down)) = completed {
        // Liveness stamp for the stall watchdog in `run_reader`.
        SAW_PKT.store(true, Ordering::Relaxed);
        LAST_PKT_MS.store(crate::apic::ms_since_boot(), Ordering::Relaxed);
        // B4: raw mouse event into the unified input ring (screen-space
        // deltas + held-button bitmask). Pushed from IRQ context.
        crate::input::push_mouse(dx as i32, -(dy as i32), buttons);
        // Move the on-screen cursor immediately in IRQ context: lowest
        // possible input latency (a few hundred byte writes). PS/2 reports
        // "up" as positive Y, but screen coordinates grow downward — negate Y
        // to match. X is already correct.
        crate::framebuffer::move_cursor(dx as i32, -(dy as i32));
        // P6c: route a button PRESS to the sign-in screen.
        //
        // The edge is (was-up -> now-down) against `left_was_down`, the state
        // captured BEFORE this packet overwrote it. Detecting only the press
        // (not the release) is what stops one physical click from toggling the
        // power menu twice.
        //
        // The click is QUEUED, not acted on here: `signin::on_click` repaints
        // the whole screen, and doing that in IRQ context would hold the
        // framebuffer lock across a full-screen redraw with interrupts off.
        // `signin::run_pending`, a task, drains the queue.
        if (buttons & crate::input::BTN_LEFT) != 0 && !left_was_down {
            let (cx, cy) = crate::framebuffer::cursor_pos();
            crate::signin::queue_click(cx, cy);
        }
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

/// Liveness stamps for the stall watchdog, written by the IRQ handler and read
/// by the reader task.
static SAW_PKT: AtomicBool = AtomicBool::new(false);
static LAST_PKT_MS: AtomicU64 = AtomicU64::new(0);

/// Silence after which a previously-working mouse is presumed dead. QEMU
/// streams continuously, so a gap this long means the device stopped sending.
const MOUSE_STALL_MS: u64 = 1_500;
/// Minimum gap between recovery attempts, so a genuinely absent mouse costs a
/// bounded amount of work instead of retrying forever.
const MOUSE_RETRY_COOLDOWN_MS: u64 = 5_000;

/// Re-run the enable sequence after a stall, so a mouse that was knocked into a
/// silent state recovers on its own instead of needing a reboot.
///
/// Gated on `SAW_PKT`: on a machine with no PS/2 mouse (the headless test runs)
/// this never fires, so the suite cannot be disturbed by it.
fn recover_streaming(now: u64) {
    // `init` polls the i8042 with bounded loops and takes the port mutexes;
    // holding the KSL keeps it off the IRQ's back, and IF=0 keeps this CPU out
    // of the handler for the duration.
    let _ksl = crate::ksl::lock();
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _ = init();
    });
    LAST_PKT_MS.store(now, Ordering::Relaxed);
    serial_writeln!(
        "[mouse] no packet for {MOUSE_STALL_MS}ms - re-ran the PS/2 enable sequence"
    );
}

pub fn run_reader() {
    let mut last: (i16, i16, bool, bool) = (0, 0, false, false);
    let mut last_logged = 0u64;
    let mut next_retry = 0u64;
    loop {
        // Watchdog: a mouse that worked and has since gone silent is re-armed
        // rather than left dead until reboot. This is the general safety net
        // behind the `init` fix - it covers any future way the device can wedge.
        if SAW_PKT.load(Ordering::Relaxed) {
            let now = crate::apic::ms_since_boot();
            if now.saturating_sub(LAST_PKT_MS.load(Ordering::Relaxed)) >= MOUSE_STALL_MS
                && now >= next_retry
            {
                next_retry = now + MOUSE_RETRY_COOLDOWN_MS;
                recover_streaming(now);
            }
        }
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
    // The ack is normally the very next byte, but a motion packet already in
    // flight can sit in the output buffer ahead of it. Consume a few stray
    // bytes instead of failing outright: one mis-read ack must not be able to
    // abort the enable sequence (see `init`).
    for _ in 0..8 {
        match read_data() {
            Ok(0xFA) => return Ok(()),
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
    Err("mouse did not ack")
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