//! P6b — **the compositor**: the screen surface, a window list, and the present
//! bridge.
//!
//! # What this is (and is not)
//!
//! A real, if small, compositor: it owns the screen surface, keeps an ordered
//! window list, draws wallpaper / taskbar / window chrome / title text, and
//! composites back-to-front. It is NOT a window server yet - no IPC, no
//! client-requested damage, and no per-window surface mapped to a virtio
//! resource (one window at a time, composited into the screen). Those are later
//! stages, and the shape here is arranged so they slot in rather than needing a
//! rewrite.
//!
//! # Why the compositor marks damage instead of presenting directly
//!
//! Everything goes through `surface::fill` / `blit` / `blend_mask`, which feed
//! the SAME dirty-rect accumulator the console already used, so the present
//! flusher picks the desktop up with no new present mechanism. That is why this
//! session's pacing work applies to the desktop instead of being thrown away.
//!
//! # Locking discipline - the thing to get right
//!
//! The compositor holds NO lock while drawing; `surface` takes the table lock per
//! operation. Two rules follow, and both exist because the alternative is a
//! deadlock or a torn frame:
//!   1. Never hold `SCHED_LOCK` across a context switch. The compositor is an
//!      ordinary preemptible task and never calls `preempt()`.
//!   2. Never wrap a whole frame in `without_interrupts`. The old clock task did
//!      exactly that and fired an IPI per pixel; a full-screen composite is ~2M
//!      pixels, so that would be catastrophic. Each draw call is individually
//!      cheap and the surface lock is per-call.

use alloc::vec::Vec;
use crate::surface::{self, Format, SurfaceId};

/// Screen-geometry constants, named so the layout reads as layout rather than
/// magic numbers scattered through draw calls.
pub const TASKBAR_H: usize = 28;
/// Window title-bar height.
pub const TITLEBAR_H: usize = 20;
/// A window's border thickness.
pub const BORDER: usize = 2;
/// Font size for window titles and the taskbar.
pub const UI_FONT: u16 = 13;

/// The desktop palette, centralised so a restyle is one edit and the contrast
/// relationships are visible together.
pub mod palette {
    /// Desktop backdrop.
    pub const BG: u32 = 0x0E1420;
    /// Taskbar body.
    pub const TASKBAR: u32 = 0x182338;
    /// Taskbar top highlight.
    pub const TASKBAR_HI: u32 = 0x24354F;
    /// Window body.
    pub const WINDOW: u32 = 0x1B2740;
    /// Window border.
    pub const BORDER: u32 = 0x3C5A8A;
    /// Title-bar fill.
    pub const TITLEBAR: u32 = 0x2B4A7A;
    /// Primary text.
    pub const TEXT: u32 = 0xE8EEF9;
    /// De-emphasised text.
    pub const TEXT_DIM: u32 = 0x8FA0BC;
    /// Accent (focus ring, clock, highlights).
    pub const ACCENT: u32 = 0x5AA9FF;
}

/// One composited window. `surface` is `None` for a window that has not been
/// given its own buffer yet - the next stage, when windows become real clients
/// with their own GPU resources.
pub struct Window {
    pub title: &'static str,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub focused: bool,
    pub surface: Option<SurfaceId>,
}

impl Window {
    /// Clip rect against the screen, excluding the taskbar strip (windows never
    /// cover it). `None` when the window is entirely off-screen.
    ///
    /// Both edges are clamped, not just the far one. A window positioned past
    /// the right/bottom edge has `x0 > sw`, and every caller then does
    /// `x1 - x0` in `usize` - which UNDERFLOWS into a near-maximum value and
    /// turns one fill into an attempt to paint four billion pixels. That is a
    /// hang, not a clipped window, and it is the single most important thing
    /// this function does.
    fn visible_rect(&self, sw: usize, sh: usize) -> Option<(usize, usize, usize, usize)> {
        let usable_h = sh.saturating_sub(TASKBAR_H);
        let x0 = self.x.min(sw);
        let y0 = self.y.min(usable_h);
        let x1 = (self.x + self.w).min(sw);
        let y1 = (self.y + self.h).min(usable_h);
        if x0 >= x1 || y0 >= y1 {
            return None; // fully off-screen, or zero-sized
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }
}

/// The compositor: one screen surface plus an ordered window list.
///
/// Windows are back-to-front: `windows[0]` is furthest back, the last is on
/// top. `focus` is an INDEX into that list, not a flag, because two windows can
/// both believe they are focused - exactly the split-brain shape P1A removed
/// from the task CPU tag. One authority, no duplicates.
pub struct Compositor {
    pub screen: SurfaceId,
    pub width: usize,
    pub height: usize,
    pub windows: Vec<Window>,
    pub focus: Option<usize>,
    /// Frames composited, for the self-test and to spot a stall.
    pub frames: u32,
    /// Set once the first frame has been composited.
    pub presented: bool,
}

impl Compositor {
    /// Create a compositor with a screen surface.
    pub fn new(width: usize, height: usize) -> Result<Compositor, &'static str> {
        let screen = surface::create(width, height, Format::B8G8R8X8)?;
        Ok(Compositor {
            screen,
            width,
            height,
            windows: Vec::new(),
            focus: None,
            frames: 0,
            presented: false,
        })
    }

    /// Add a window, clipped to the screen. Returns its index. The window is not
    /// focused; call [`Compositor::focus_newest`].
    pub fn add_window(
        &mut self,
        title: &'static str,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Result<usize, &'static str> {
        let w = w.min(self.width);
        let h = h.min(self.height.saturating_sub(TASKBAR_H));
        if w == 0 || h == 0 {
            return Err("compositor: window has no area");
        }
        self.windows.push(Window {
            title,
            x,
            y,
            w,
            h,
            focused: false,
            surface: None,
        });
        let idx = self.windows.len() - 1;
        self.focus_newest();
        Ok(idx)
    }

    /// Focus the topmost window, clearing every other window's flag.
    ///
    /// Single source of truth: rather than letting each window carry its own
    /// `focused` bool (which two windows can both set), the list index IS the
    /// focus. The bool exists only so `render` can read it in one place.
    pub fn focus_newest(&mut self) {
        self.focus = if self.windows.is_empty() {
            None
        } else {
            Some(self.windows.len() - 1)
        };
        let f = self.focus;
        for (i, win) in self.windows.iter_mut().enumerate() {
            win.focused = Some(i) == f;
        }
    }

    /// Draw one complete frame: backdrop, windows back-to-front, then the
    /// taskbar on top (so a window can never cover it).
    pub fn render(&mut self) -> Result<(), &'static str> {
        let (sw, sh) = (self.width, self.height);
        // 1. Backdrop. ONE fill for the whole screen - a per-pixel loop here
        //    would be the compositor's entire cost budget.
        surface::fill(self.screen, 0, 0, sw, sh, palette::BG)?;
        // 2. Windows, back to front.
        for i in 0..self.windows.len() {
            self.draw_window(i)?;
        }
        // 3. Taskbar last, so it is always on top and always full width.
        self.draw_taskbar()?;
        self.frames = self.frames.wrapping_add(1);
        self.presented = true;
        Ok(())
    }

    fn draw_window(&mut self, idx: usize) -> Result<(), &'static str> {
        let (sw, sh) = (self.width, self.height);
        let Some((vx, vy, vw, vh)) = self.windows[idx].visible_rect(sw, sh) else {
            return Ok(()); // entirely off-screen
        };
        let (wx, wy, ww, wh, title, focused, wsrc) = {
            let w = &self.windows[idx];
            (w.x, w.y, w.w, w.h, w.title, w.focused, w.surface)
        };

        // Border, then the body inset by it, so the border reads as a frame
        // rather than a line the body paints over.
        let border = if focused { palette::ACCENT } else { palette::BORDER };
        surface::fill(self.screen, vx, vy, vw, vh, border)?;
        if vw > BORDER * 2 && vh > BORDER * 2 {
            surface::fill(
                self.screen,
                vx + BORDER,
                vy + BORDER,
                vw - BORDER * 2,
                vh - BORDER * 2,
                palette::WINDOW,
            )?;
        }

        // Title bar, clipped to the window's VISIBLE rect. Every coordinate here is
        // derived from the CLIPPED rect, never from the window's own x/y: a
        // window scrolled off the left or top must not make `bar_bottom -
        // bar_y` underflow, which would ask `fill` for a ~4-billion-pixel rect.
        let bar_y = wy.max(vy).min(vy + vh);
        let bar_bottom = (wy + TITLEBAR_H).min(vy + vh);
        if bar_bottom > bar_y {
            let hh = bar_bottom - bar_y;
            surface::fill(self.screen, vx, bar_y, vw, hh, palette::TITLEBAR)?;
            // The title bar's baseline must sit INSIDE the bar. A 13px font has
            // an ascent of ~13 and the bar is only TITLEBAR_H tall, so anchoring
            // the baseline at the bar's BOTTOM edge put every glyph's mask above
            // the visible area and the title rendered as nothing. `line_ascent` is
            // the honest way to place it: bar_y + ascent, clamped to the bar.
            let asc = surface::line_ascent(UI_FONT) as usize;
            let baseline = (bar_y + asc).min(bar_bottom.saturating_sub(1));
            if vw > BORDER * 2 + 12 {
            let _ = surface::draw_text(
                self.screen,
                vx + BORDER + 2,
                baseline,
                title,
                UI_FONT,
                palette::TEXT,
            );
        }
        }

        // Client area: a window with its own surface is blitted here (next
        // stage); without one it is the flat body drawn above.
        if let Some(src) = wsrc {
            let cx = vx.max(wx + BORDER);
            let cy = (vy + TITLEBAR_H).max(wy + BORDER);
            let cw = (wx + ww).min(vx + vw).saturating_sub(cx - wx);
            let ch = (wy + wh).min(vy + vh).saturating_sub(cy - wy);
            if cw > 0 && ch > 0 {
                let _ = surface::blit(self.screen, src, cx as i32, cy as i32, 0, 0, cw, ch);
            }
        }
        let _ = sh;
        Ok(())
    }

    fn draw_taskbar(&mut self) -> Result<(), &'static str> {
        let (sw, sh) = (self.width, self.height);
        let top = sh.saturating_sub(TASKBAR_H);
        surface::fill(self.screen, 0, top, sw, TASKBAR_H, palette::TASKBAR)?;
        // A 1px highlight along the top edge reads as a lit bar for one row of
        // fill.
        surface::fill(self.screen, 0, top, sw, 1, palette::TASKBAR_HI)?;
        // The focused window's title in the taskbar, so the user can see which
        // window has focus without reading the stack.
        if let Some(f) = self.focus {
            let title = self.windows[f].title;
            if sw > 24 {
                let _ = surface::draw_text(
                    self.screen,
                    8,
                    top + TASKBAR_H - 8,
                    title,
                    UI_FONT,
                    palette::TEXT_DIM,
                );
            }
        }
        Ok(())
    }

    /// Surface damage accumulated by the last [`Compositor::render`], cleared.
    /// The caller presents it; the compositor never touches the device.
    pub fn take_frame_damage(&self) -> Option<(usize, usize, usize, usize)> {
        surface::take_damage(self.screen)
    }
}

