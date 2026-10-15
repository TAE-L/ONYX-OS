//! P6c: the **desktop** — what the user reaches after signing in.
//!
//! # Scope
//!
//! This is deliberately an EMPTY desktop: a backdrop and the signed-in identity,
//! with no applications on it yet. It exists because "click Sign in" has to lead
//! SOMEWHERE, and because the screen-to-screen transition (sign-in -> desktop) is
//! the part that has to work before any app can be built on it.
//!
//! # Why this is not the P6b compositor
//!
//! `compositor` + `surface` render real windows, but their damage is tracked
//! PER-SURFACE (`Surface::take_damage`) while the present flusher reads
//! `framebuffer::take_dirty_rect` — a separate accumulator. A composited frame
//! therefore sits in a surface and never reaches scanout. Bridging those is the
//! next stage, and it is what a real window manager will need.
//!
//! Until then, this draws into the framebuffer directly, reusing the primitives
//! in [`crate::signin`]. That is also why the earlier Text Lab prototype was
//! removed rather than grown: it was a useful instrument for proving the two text
//! knobs it exposed, but it was an app, and apps belong on top of the compositor
//! rather than as a hand-drawn exception to it.
//!
//! # Damage discipline
//!
//! Everything here goes through `framebuffer::with_writer` / `with_writer_region`,
//! which is what makes a repaint cheap AND invisible: the live backend is dispi,
//! which maps the linear framebuffer directly, so QEMU scans out the very memory
//! being written. A full-surface repaint therefore shows up on screen as a
//! top-to-bottom wipe. Repaint only what changed.

use crate::signin::{draw_text_centered, TEXT, TEXT_DIM};
use core::sync::atomic::{AtomicBool, Ordering};

/// Set once the desktop has been painted, so the transition is observable and
/// testable without reading pixels.
static SHOWN: AtomicBool = AtomicBool::new(false);

/// Has the desktop been shown?
pub fn shown() -> bool {
    SHOWN.load(Ordering::Acquire)
}
/// The desktop backdrop. Flat, and darker than the sign-in gradient, so the
/// change of screen reads as a change rather than as a flicker.
const BG: u32 = 0x0E1420;

fn fill_rect(w: &mut crate::framebuffer::FrameBufferWriter, rgb: u32) {
    let (width, height) = w.dims();
    for y in 0..height {
        w.fill_row(y, rgb, width);
    }
}

/// Paint the desktop. One FB acquisition for the frame.
pub fn draw() {
    let _ = crate::framebuffer::with_writer(|w, width, height| {
        fill_rect(w, BG);

        // An empty desktop still says what it is and who is in it — otherwise a
        // black screen after Sign in is indistinguishable from a crash.
        let cx = (width / 2) as i64;
        let cy = (height / 2) as i64;
        draw_text_centered(
            w,
            cx,
            cy,
            "OnyxOS",
            34,
            TEXT,
        );
        draw_text_centered(w, cx, cy + 44, crate::signin::USER_NAME, 20, TEXT_DIM);
        draw_text_centered(w, cx, cy + 80, "no applications yet", 16, TEXT_DIM);
    });
    SHOWN.store(true, Ordering::Release);
}

/// Handle a click on the desktop.
///
/// A no-op until there is something to click. It exists (rather than being
/// omitted at the call site) so the phase routing in `signin::run_pending` has
/// one obvious place to grow, and so an empty desktop cannot accidentally fall
/// through to the sign-in screen's hit-testing.
pub fn on_click(_x: usize, _y: usize) {}


