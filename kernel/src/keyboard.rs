//! PS/2 keyboard driver (IRQ 1).
//!
//! The IRQ handler reads a scancode from the data port, decodes it with
//! `pc-keyboard`, and pushes an event into a small lock-free ring buffer
//! (no allocation; the producer runs with interrupts off). A kernel task
//! drains the ring and prints the events — input and multitasking working
//! together.
//!
//! Every key the decoder reports is forwarded, not just printable text:
//! modifier presses arrive as `DecodedKey::RawKey(KeyCode)` and are printed
//! by name (Shift, Ctrl, Alt, CapsLock, Windows keys, F1..F12, ...). The
//! decoder emits no event for releases of non-toggle keys, so only
//! presses/toggles are reported.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::serial_writeln;
use pc_keyboard::layouts::Us104Key;
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, KeyState, PS2Keyboard, ScancodeSet1};
use spin::Mutex;
use x86_64::instructions::port::Port;

/// Ring entry encoding:
///   bit 32 clear -> low 32 bits are a Unicode codepoint (printable key)
///   bit 32 set   -> low 8 bits are a `KeyCode` discriminant (named key)
const FLAG_RAW: u64 = 1 << 32;

/// Highest valid `KeyCode` discriminant. `KeyCode` is `#[repr(u8)]` with
/// contiguous, implicit discriminants starting at 0 (verified against
/// pc-keyboard 0.9.0 `src/lib.rs`), so every value in `0..=KEY_LAST` can be
/// soundly transmuted back into a `KeyCode` by the reader task; anything
/// above is impossible by construction.
const KEY_LAST: u64 = KeyCode::Unknown as u64;

/// PS/2 data port.
const DATA_PORT: u16 = 0x60;

/// Scancode -> key decoder (US 104-key layout, scancode set 1, which is what
/// the i8042 controller in QEMU/a standard PC provides after translation).
static KEYBOARD: Mutex<PS2Keyboard<Us104Key, ScancodeSet1>> = Mutex::new(
    // HandleControl::Ignore: report keys as-is (Ctrl press -> <LControl>,
    // letter -> letter). Shortcut composition (Ctrl+C etc.) is the shell's
    // job once it tracks modifier state; mapping letters to control codes
    // in the driver would destroy information.
    PS2Keyboard::new(ScancodeSet1::new(), Us104Key, HandleControl::Ignore),
);

/// Data port, accessed only from the IRQ handler.
static PORT: Mutex<Port<u8>> = Mutex::new(Port::new(DATA_PORT));

const RING_LEN: usize = 256;

/// Fixed ring of pending key events (SPSC).
///
/// M9.8-(d): the slots, HEAD and TAIL are atomics now. The "producer is an IRQ,
/// consumer is a task, never concurrent" argument died with CPU affinity —
/// with task migration the consumer can run on another CPU while the keyboard
/// IRQ is delivered *here*, so plain `static mut` fields would be a real data
/// race (stale HEAD/TAIL, lost or duplicated events). Release/Acquire ordering
/// is all that is needed and keeps the producer lock-free: the IRQ path must
/// never spin on a lock a task can hold.
static RING: [AtomicU64; RING_LEN] = [const { AtomicU64::new(0) }; RING_LEN];
/// Next write slot (producer-owned).
static HEAD: AtomicUsize = AtomicUsize::new(0);
/// Next read slot (consumer-owned).
static TAIL: AtomicUsize = AtomicUsize::new(0);

fn ring_push(value: u64) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RING_LEN;
    // The consumer may have advanced TAIL since we last looked; a *stale*
    // TAIL only makes this check stricter (drop instead of overwrite), which
    // is the safe direction for a ring this size.
    if next != TAIL.load(Ordering::Acquire) {
        RING[head].store(value, Ordering::Relaxed);
        HEAD.store(next, Ordering::Release); // publish after the slot write
    }
}

fn ring_pop() -> Option<u64> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        None
    } else {
        let v = RING[tail].load(Ordering::Relaxed);
        TAIL.store((tail + 1) % RING_LEN, Ordering::Release);
        Some(v)
    }
}

/// No extra setup needed for the PS/2 keyboard at boot.
pub fn init() {}

/// Called from the IRQ-1 handler: read and decode one scancode.
pub fn handle_irq() {
    let mut port = PORT.lock();
    let scancode = unsafe { port.read() };
    let mut keyboard = KEYBOARD.lock();
    if let Ok(Some(event)) = keyboard.add_byte(scancode) {
        // B4: raw physical event — before decode, so key-releases reach the
        // raw ring too (process_keyevent suppresses releases of non-toggle
        // keys, and the compositor needs down+up pairs).
        let down = match event.state {
            KeyState::Up => false,
            _ => true, // Down, SingleShot
        };
        crate::input::push_key(event.code as u16, down);
        if let Some(decoded) = keyboard.process_keyevent(event) {
            let value = match decoded {
                DecodedKey::Unicode(c) => c as u64,
                DecodedKey::RawKey(k) => FLAG_RAW | (k as u64),
            };
            ring_push(value);
        }
    }
}

/// Laptop-embedded numpads (and the real numpad with NumLock off) send
/// `NumpadX` scancodes that traditionally double as navigation keys. Report
/// both interpretations, e.g. `<Numpad8/ArrowUp>`, so the key is usable no
/// matter which convention a future application expects.
fn numpad_alias(k: KeyCode) -> Option<&'static str> {
    match k {
        KeyCode::Numpad7 => Some("Home"),
        KeyCode::Numpad8 => Some("ArrowUp"),
        KeyCode::Numpad9 => Some("PageUp"),
        KeyCode::Numpad4 => Some("ArrowLeft"),
        KeyCode::Numpad6 => Some("ArrowRight"),
        KeyCode::Numpad1 => Some("End"),
        KeyCode::Numpad2 => Some("ArrowDown"),
        KeyCode::Numpad3 => Some("PageDown"),
        KeyCode::Numpad0 => Some("Insert"),
        KeyCode::NumpadPeriod => Some("Delete"),
        _ => None,
    }
}

/// Line buffer for `take_line`: characters typed since the last Enter.
///
/// M9.8-(d): guarded by [`LINE_LOCK`] instead of IF=0. IF=0 only serializes
/// against interrupts on the *same* CPU, while the line discipline is a single
/// shared terminal that any task on any CPU may read (SYS_READ(0)) once tasks
/// can migrate — two CPUs assembling the same line buffer would interleave
/// characters and corrupt `LINE_LEN`.
static mut LINE: [u8; 128] = [0; 128];
static mut LINE_LEN: usize = 0;

/// Serializes `take_line` bodies (the only writer of `LINE`/`LINE_LEN`).
///
/// A dedicated lock, deliberately *not* the KSL: `take_line` echoes each
/// consumed key to serial + console, which takes the framebuffer lock, and a
/// mouse IRQ needs the KSL to push into the input ring — holding the KSL for
/// the duration of an echo would make that IRQ spin for milliseconds. With
/// this lock the only losers of a contended acquisition are concurrent line
/// readers, which are rare and short.
static LINE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// M8 line discipline: drain pending key events into the line buffer. On an
/// Enter ('\n' or '\r') copies one completed line (newline stripped) into
/// `out` and returns its length; otherwise returns 0 ("nothing yet" — the
/// caller keeps polling). Typed characters and backspaces are echoed to the
/// serial console as they are consumed.
///
/// CRITICAL: this runs inside a syscall (IF=0, the syscall entry stub parked
/// the user RSP in a shared scratch slot). The interrupt state must be
/// *restored*, never blindly re-enabled: enabling IF mid-syscall lets the PIT
/// preempt the syscall, and the next task's syscall entry overwrites the
/// scratch slot — the suspended syscall would then sysretq onto another
/// task's user stack (observed as random user-data corruption). Keys still
/// arrive: the ring fills while userland runs with IF=1 between polls.
pub fn take_line(out: &mut [u8]) -> usize {
    let mut completed: Option<usize> = None;
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _line = LINE_LOCK.lock();
        while completed.is_none() {
            let Some(v) = ring_pop() else { break };
            // Named keys (Shift, F1, arrows, ...) are not line input yet.
            if v & FLAG_RAW != 0 {
                continue;
            }
            let Some(c) = char::from_u32(v as u32) else { continue };
            unsafe {
                match c {
                    '\n' | '\r' => {
                        let len = LINE_LEN;
                        let n = len.min(out.len());
                        out[..n].copy_from_slice(&LINE[..n]);
                        LINE_LEN = 0;
                        // Serial: one atomic [key] line per keypress (the
                        // IF=0 syscall context keeps the line un-garbled).
                        serial_writeln!("[key] Enter");
                        // Screen: newline the console.
                        crate::framebuffer::console_bytes(b"\r\n");
                        completed = Some(n);
                    }
                    '\u{8}' | '\u{7f}' => {
                        if LINE_LEN > 0 {
                            LINE_LEN -= 1;
                            serial_writeln!("[key] Backspace");
                            crate::framebuffer::console_bytes(b"\x08");
                        }
                    }
                    c if !c.is_control() => {
                        let mut tmp = [0u8; 4];
                        let s = c.encode_utf8(&mut tmp);
                        if LINE_LEN + s.len() <= LINE.len() {
                            LINE[LINE_LEN..LINE_LEN + s.len()].copy_from_slice(s.as_bytes());
                            LINE_LEN += s.len();
                            // Serial: atomic [key] line; Screen: char echo.
                            serial_writeln!("[key] '{}'", c);
                            crate::framebuffer::console_bytes(s.as_bytes());
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    completed.unwrap_or(0)
}

/// True if a complete line ('\n' or '\r') is waiting in the ring, WITHOUT
/// consuming anything. The scheduler polls this (every 1 ms, from inside the
/// timer handler) to wake tasks blocked in SYS_READ(0).
///
/// M9.8-(d): lock-free — it only *reads* the SPSC ring (the producer publishes
/// slots before advancing HEAD, and this walks with Acquire loads), so it can
/// be called from the scheduler's wake pass on any CPU without taking a lock
/// that the scheduled tasks might hold.
pub fn line_pending() -> bool {
    let mut i = TAIL.load(Ordering::Relaxed);
    while i != HEAD.load(Ordering::Acquire) {
        let v = RING[i].load(Ordering::Relaxed);
        if v & FLAG_RAW == 0 {
            if let Some(c) = char::from_u32(v as u32) {
                if c == '\n' || c == '\r' {
                    return true;
                }
            }
        }
        i = (i + 1) % RING_LEN;
    }
    false
}

/// Reader task body: print any keyboard events waiting in the ring.
/// (M8 note: no longer spawned at boot — the shell consumes the ring via
/// `take_line`. Kept as the M3 demo / manual-drain debugging aid.)
#[allow(dead_code)]
pub fn run_reader() {
    loop {
        // Copy pending events out while interrupts are off, then print with
        // interrupts on (so a keyboard IRQ can queue while we print).
        let mut pending = [0u64; 16];
        let mut n = 0;
        x86_64::instructions::interrupts::disable();
        while n < pending.len() {
            match ring_pop() {
                Some(v) => {
                    pending[n] = v;
                    n += 1;
                }
                None => break,
            }
        }
        x86_64::instructions::interrupts::enable();
        for &v in &pending[..n] {
            if v & FLAG_RAW == 0 {
                // Unicode path. Control characters that arrive as Unicode
                // (Tab/Enter/Backspace/Escape from the layout) get friendly
                // names; everything printable prints as itself.
                let ch = char::from_u32(v as u32);
                let name = match ch {
                    Some('\t') => Some("Tab"),
                    Some('\n') => Some("Enter"),
                    Some('\r') => Some("Return"),
                    Some('\u{8}') => Some("Backspace"),
                    Some('\u{1b}') => Some("Escape"),
                    Some('\u{7f}') => Some("Delete"),
                    Some(' ') => Some("Space"),
                    _ => None,
                };
                match (name, ch) {
                    (Some(n), _) => serial_writeln!("[keyboard] <{}>", n),
                    (None, Some(c)) if c.is_control() => {
                        serial_writeln!("[keyboard] <ctrl 0x{:02x}>", c as u32)
                    }
                    (None, Some(c)) => serial_writeln!("[keyboard] {:?}", c),
                    (None, None) => serial_writeln!("[keyboard] <bad codepoint 0x{:x}>", v),
                }
            } else {
                let disc = v & !FLAG_RAW;
                if disc <= KEY_LAST {
                    // Sound: `KeyCode` is #[repr(u8)] with contiguous implicit
                    // discriminants 0..=KEY_LAST (pc-keyboard 0.9.0), so this
                    // transmute can never produce an invalid variant.
                    let kc: KeyCode = unsafe { core::mem::transmute(disc as u8) };
                    match numpad_alias(kc) {
                        Some(alias) => serial_writeln!("[keyboard] <{:?}/{}>", kc, alias),
                        None => serial_writeln!("[keyboard] <{:?}>", kc),
                    }
                } else {
                    serial_writeln!("[keyboard] <key {}>", disc);
                }
            }
        }
    }
}