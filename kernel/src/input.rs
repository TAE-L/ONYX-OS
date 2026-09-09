//! M9.6-B4: unified raw input event ring.
//!
//! Kernel-side raw input: every PHYSICAL key event (KeyCode + Down/Up, from
//! the PS/2 keyboard decoder) and every completed mouse packet (relative
//! screen-space dx/dy + button bitmask) is pushed into one fixed-size record
//! ring, stamped with the A1 monotonic ns clock. Ring-3 programs read it via
//! SYS_INPUT_READ (blocking or non-blocking). This is the *raw-input half* of
//! the deferred M9.5 window server: the kernel stays out of graphics, but
//! every event a compositor/game will need is already timestamped and cheap
//! to consume.
//!
//! The shell's text path is untouched — the line discipline (fd 0 SYS_READ +
//! `keyboard::take_line`) reads the *decoded* key ring; this is a parallel
//! raw side channel fed from the same IRQ handlers.
//!
//! Record layout (little-endian, 24 bytes, repr(C)):
//!   +0  u8  kind   EV_KIND_KEY | EV_KIND_MOUSE
//!   +1  u8  flags  key: KEY_DOWN|KEY_UP ; mouse: BTN_* bitmask
//!   +2  u16 code   key: pc_keyboard KeyCode discriminant; mouse: 0
//!   +4  i32 x      mouse: relative dx (screen space); key: 0
//!   +8  i32 y      mouse: relative dy (screen space); key: 0
//!  +12  u32 (pad)
//!  +16  u64 ts_ns  A1 monotonic nanosecond timestamp
//!
//! The kernel test (`input_test` in main.rs) verifies the exact layout, the
//! drop-on-full behavior, and the scheduler block-on-raw wake; test-input.ps1
//! exercises the full path end-to-end from QEMU's monitor to a ring-3 program.

use crate::time;
use x86_64::instructions::interrupts::without_interrupts;

/// Key event (kind = EV_KIND_KEY).
pub const EV_KIND_KEY: u8 = 1;
/// Mouse event (kind = EV_KIND_MOUSE). code = 0; x/y = relative screen-space
/// deltas (Y grows downward, i.e. the same sign convention the framebuffer
/// cursor uses); flags = BTN_* bitmask of the buttons held at this packet.
pub const EV_KIND_MOUSE: u8 = 2;

/// Key event flag: the physical key went down (also set for SingleShot keys).
pub const KEY_DOWN: u8 = 1;
/// Key event flag: the physical key was released.
pub const KEY_UP: u8 = 2;

/// Mouse button bits (also the mouse `flags` value).
pub const BTN_LEFT: u8 = 1;
pub const BTN_RIGHT: u8 = 2;
pub const BTN_MIDDLE: u8 = 4;

/// Wire size of one event. The struct below is `repr(C, align(8))`: u8 + u8 +
/// u16 + i32 + i32 (offset 12) + padding to 16 for the u64 + u64 = 24 bytes.
/// `input_test` asserts events read back from the ring are exactly this size.
pub const EV_SIZE: usize = 24;

#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct InputEvent {
    pub kind: u8,
    pub flags: u8,
    pub code: u16,
    pub x: i32,
    pub y: i32,
    pub ts_ns: u64,
}

/// Ring capacity: 256 events @ 24 B = 6 KiB of static kernel memory. At the
/// PS/2 mouse's 200 Hz that is ~1.3 s of full-rate motion before either the
/// consumer drains or new events drop (drop-newest keeps the oldest events,
/// which is what a consumer behind schedule wants).
const RING_LEN: usize = 256;

static mut RING: [InputEvent; RING_LEN] = [InputEvent {
    kind: 0,
    flags: 0,
    code: 0,
    x: 0,
    y: 0,
    ts_ns: 0,
}; RING_LEN];
static mut HEAD: usize = 0; // next write slot
static mut TAIL: usize = 0; // next read slot
static mut COUNT: usize = 0; // pending events
/// Sum of all events ever pushed (test/observability counter).
static mut TOTAL: usize = 0;
/// Count of events dropped because the ring was full.
static mut DROPPED: usize = 0;

/// Producer entry — called from IRQ contexts (IF=0) only.
fn push(e: InputEvent) {
    without_interrupts(|| unsafe {
        // Full-ring check must use COUNT alone: when the ring is full,
        // HEAD == TAIL (indistinguishable from empty by pointers), and when
        // one slot is free, (HEAD + 1) % LEN == TAIL — a `next == TAIL` guard
        // would wrongly drop that last event (observed: kept=255/45 instead
        // of 256/44 in input_test's drop-on-full stage).
        if COUNT == RING_LEN {
            DROPPED += 1; // full: drop the NEW event, keep the oldest
            return;
        }
        RING[HEAD] = e;
        HEAD = (HEAD + 1) % RING_LEN;
        COUNT += 1;
        TOTAL += 1;
    });
}

/// Raw key event: `code` is a pc_keyboard `KeyCode` discriminant; `down` is
/// the physical state (KeyState::Down/SingleShot = true, Up = false).
pub fn push_key(code: u16, down: bool) {
    push(InputEvent {
        kind: EV_KIND_KEY,
        flags: if down { KEY_DOWN } else { KEY_UP },
        code,
        x: 0,
        y: 0,
        ts_ns: time::now_ns(),
    });
}

/// Raw mouse event: relative screen-space deltas + held-button bitmask.
pub fn push_mouse(dx: i32, dy: i32, buttons: u8) {
    push(InputEvent {
        kind: EV_KIND_MOUSE,
        flags: buttons,
        code: 0,
        x: dx,
        y: dy,
        ts_ns: time::now_ns(),
    });
}

/// Encode one event into its 24-byte little-endian wire form.
fn encode(dst: &mut [u8], e: &InputEvent) {
    dst[0] = e.kind;
    dst[1] = e.flags;
    dst[2] = e.code as u8;
    dst[3] = (e.code >> 8) as u8;
    let x = e.x as u32;
    dst[4] = x as u8;
    dst[5] = (x >> 8) as u8;
    dst[6] = (x >> 16) as u8;
    dst[7] = (x >> 24) as u8;
    let y = e.y as u32;
    dst[8] = y as u8;
    dst[9] = (y >> 8) as u8;
    dst[10] = (y >> 16) as u8;
    dst[11] = (y >> 24) as u8;
    dst[12] = 0;
    dst[13] = 0;
    dst[14] = 0;
    dst[15] = 0;
    let t = e.ts_ns;
    for i in 0..8 {
        dst[16 + i] = (t >> (i * 8)) as u8;
    }
}

/// Copy as many full 24-byte events as fit into `dst` (each event is one
/// `EV_SIZE` chunk). Returns the number of bytes copied (a multiple of
/// EV_SIZE). IF-safe (`without_interrupts`): the producer is an IRQ handler.
pub fn drain(dst: &mut [u8]) -> usize {
    let mut copied = 0usize;
    without_interrupts(|| unsafe {
        while COUNT > 0 && copied + EV_SIZE <= dst.len() {
            encode(&mut dst[copied..copied + EV_SIZE], &RING[TAIL]);
            copied += EV_SIZE;
            TAIL = (TAIL + 1) % RING_LEN;
            COUNT -= 1;
        }
    });
    copied
}

/// Drop every pending event (test/consumer "arm" helper: discard whatever was
/// queued before we started caring). Returns how many events were dropped.
pub fn clear() -> usize {
    without_interrupts(|| unsafe {
        let n = COUNT;
        TAIL = HEAD;
        COUNT = 0;
        n
    })
}

/// True when at least one event is waiting. Called by the scheduler's wake
/// pass (IF=0) to resume a task blocked in SYS_INPUT_READ.
pub fn pending() -> bool {
    unsafe { COUNT > 0 }
}

/// Number of pending events (tests / observability).
pub fn count() -> usize {
    unsafe { COUNT }
}

/// Events ever dropped for a full ring (tests assert 0 normally).
pub fn dropped() -> usize {
    unsafe { DROPPED }
}

/// Events ever pushed (tests / observability).
pub fn total() -> usize {
    unsafe { TOTAL }
}

/// Wire size self-check: the encode/decode and the E2E test both rely on the
/// 24-byte layout. Prints the compiler's actual struct size at boot so a
/// repr(C) change can never silently desync the protocol.
pub fn init() {
    crate::serial_writeln!(
        "input: event size = {} bytes (expect 24), ring = {} events",
        core::mem::size_of::<InputEvent>(),
        RING_LEN
    );
}