//! Minimal framebuffer text renderer for the bootloader-provided pixel buffer.
//!
//! The bootloader (v0.11+) sets up a pixel-based framebuffer (not VGA text
//! mode), so text must be drawn pixel-by-pixel. We clear the screen to black
//! and draw 8x8 bitmap glyphs from the `font8x8` crate's legacy glyph table.

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

/// `font8x8::legacy::BASIC_LEGACY` holds raw glyphs for ASCII 0..=127.
const MAX_ASCII: usize = 128;

pub struct FrameBufferWriter<'a> {
    framebuffer: &'a mut [u8],
    info: FrameBufferInfo,
    x_pos: usize,
    y_pos: usize,
    /// Text pixel scale: 2 on high-res panels (>= 1600 px wide), else 1.
    /// Keeps the apparent glyph size constant as resolution rises.
    scale: usize,
}

impl<'a> FrameBufferWriter<'a> {
    pub fn new(framebuffer: &'a mut [u8], info: FrameBufferInfo) -> Self {
        let scale = if info.width >= 1600 { 2 } else { 1 };
        let mut w = Self {
            framebuffer,
            info,
            x_pos: 0,
            y_pos: 0,
            scale,
        };
        w.clear_bg();
        w
    }

    /// Fill the whole framebuffer with a dark slate background so the OS
    /// visibly owns every pixel of the canvas (no "unused margin" look).
    fn clear_bg(&mut self) {
        for y in 0..self.info.height {
            for x in 0..self.info.width {
                self.set_pixel(x, y, 0x0E1420);
            }
        }
    }

    /// Full-width header bar with the OS name (drawn once at boot).
    /// Leaves the text cursor just below the bar, ready for `write_str`.
    pub fn draw_header(&mut self, title: &str) {
        let bar_h = 48;
        for y in 0..bar_h.min(self.info.height) {
            for x in 0..self.info.width {
                self.set_pixel(x, y, 0x0B5ED7);
            }
        }
        self.x_pos = 12;
        self.y_pos = 14;
        for c in title.chars() {
            if (c as usize) < MAX_ASCII {
                self.write_char(c);
            }
        }
        self.x_pos = 0;
        self.y_pos = bar_h + 8;
    }

    pub fn write_str(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '\n' => self.newline(),
                c if (c as usize) < MAX_ASCII => self.write_char(c),
                _ => {}
            }
        }
    }

    fn write_char(&mut self, c: char) {
        // `legacy::BASIC_LEGACY` is `[[u8; 8]; 128]` — one glyph per ASCII char.
        let glyph = font8x8::legacy::BASIC_LEGACY[c as usize]; // [u8; 8]
        let s = self.scale;
        for (row, &byte) in glyph.iter().enumerate() {
            for col in 0..8u8 {
                if (byte >> col) & 1 == 1 {
                    // Each glyph pixel becomes an s×s block.
                    for sy in 0..s {
                        for sx in 0..s {
                            self.set_pixel(
                                self.x_pos + col as usize * s + sx,
                                self.y_pos + row * s + sy,
                                0xFFFFFF,
                            );
                        }
                    }
                }
            }
        }
        self.x_pos += 9 * s; // (8px glyph + 1px spacing) × scale
    }

    fn newline(&mut self) {
        self.x_pos = 0;
        self.y_pos += 10 * self.scale; // (8px glyph + 2px spacing) × scale
    }

    fn set_pixel(&mut self, x: usize, y: usize, rgb: u32) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let pixel_offset = y * self.info.stride + x;
        let i = pixel_offset * self.info.bytes_per_pixel;
        // Guard against out-of-bounds writes for unusual layouts.
        if i + 3 > self.framebuffer.len() {
            return;
        }
        let r = ((rgb >> 16) & 0xff) as u8;
        let g = ((rgb >> 8) & 0xff) as u8;
        let b = (rgb & 0xff) as u8;
        match self.info.pixel_format {
            PixelFormat::Rgb => {
                self.framebuffer[i] = r;
                self.framebuffer[i + 1] = g;
                self.framebuffer[i + 2] = b;
            }
            PixelFormat::Bgr => {
                self.framebuffer[i] = b;
                self.framebuffer[i + 1] = g;
                self.framebuffer[i + 2] = r;
            }
            PixelFormat::U8 => {
                self.framebuffer[i] = ((r as u32 + g as u32 + b as u32) / 3) as u8;
            }
            // `PixelFormat` is non-exhaustive (foreign crate): must cover future variants.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Mouse cursor sprite
// ---------------------------------------------------------------------------
// The writer is installed once at boot and from then on the ONLY writer is
// `move_cursor`, called from IRQ 12 — a single-writer discipline, so cursor
// movement costs a few hundred in-memory byte writes per packet (no syscalls,
// no locks contention beyond one uncontended Mutex): minimal input latency.

/// The global writer, populated by `init_global`.
static FB: Mutex<Option<FrameBufferWriter<'static>>> = Mutex::new(None);

/// The scrolling text console, installed at boot by `init_global`. It mirrors
/// terminal input echo + `SYS_WRITE(1/2)` output so typed keys and shell
/// results actually appear on the framebuffer (the GUI window).
static CONSOLE: Mutex<Option<TextConsole>> = Mutex::new(None);
/// Bytes mirrored to the console (observable on serial: proves the path lives).
static CONSOLE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The damaged region since the last [`take_dirty_rect`]: an inclusive
/// bounding box, or `None` for "nothing dirty".
///
/// Why a bounding box and not a list of rects: the present path pushes one
/// `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` pair per flush, so a single
/// rectangle is exactly what the device API wants. A keystroke touches one
/// glyph (8x8 at scale 1, 16 px at scale 2), so the box stays tiny even when
/// a burst of text arrives — the win comes from the box being small, not from
/// the count of draws inside it.
///
/// Why a lock and not a lock-free CAS: an earlier version tried to make the
/// four edges lock-free with a CAS on X0 as an ownership token. That is
/// subtly wrong — when a marker widens only the *other* three edges (x0 is
/// already the minimum), its CAS writes the same value and does not actually
/// exclude a concurrent taker, so a rect can still be lost. A spin mutex is
/// a few instructions uncontended, the critical sections are pure arithmetic
/// with no blocking, and it is *provably* mutually exclusive. Correctness of
/// the present path outranks shaving nanoseconds off `set_pixel`, which is
/// already inside a CONSOLE lock for every console write.
static DIRTY: Mutex<Option<(usize, usize, usize, usize)>> = Mutex::new(None);

/// Widen the dirty box by one pixel.
#[inline]
fn mark_dirty(x: usize, y: usize) {
    mark_dirty_rect(x, y, x, y);
}

/// Widen the dirty box by a rectangle (inclusive bounds).
#[inline]
fn mark_dirty_rect(x0: usize, y0: usize, x1: usize, y1: usize) {
    let mut slot = DIRTY.lock();
    *slot = Some(match *slot {
        Some((ax0, ay0, ax1, ay1)) => (
            ax0.min(x0),
            ay0.min(y0),
            ax1.max(x1),
            ay1.max(y1),
        ),
        None => (x0, y0, x1, y1),
    });
}

/// Take the accumulated dirty rectangle, clearing the accumulator.
///
/// Returns `None` when nothing was drawn since the last call — the caller's
/// cue to skip the transfer entirely. On `Some`, the box is inclusive in both
/// axes and the accumulator is re-armed empty.
///
/// Taking the box and re-arming it under ONE lock acquisition is what makes
/// this safe: a marker either merges into the box we are about to return (and
/// is therefore included in the transfer), or lands after we have cleared it
/// (and waits for the next tick). There is no window in which damage is
/// recorded but never presented — the one failure a present path must not
/// have, which is stale pixels that nothing will ever redraw.
pub fn take_dirty_rect() -> Option<(usize, usize, usize, usize)> {
    DIRTY.lock().take()
}

/// Mark the whole surface dirty — used by the present path's own first frame
/// and by anything that rebuilds the console from scratch.
pub fn mark_all_dirty(width: usize, height: usize) {
    if width == 0 || height == 0 {
        return;
    }
    mark_dirty_rect(0, 0, width - 1, height - 1);
}

/// Re-arm a rect that a flush FAILED to present, so the pixels are retried
/// next tick instead of being lost. Dimensions (not bounds), matching the
/// device rect the flusher sent.
pub fn mark_dirty_public(x: usize, y: usize, w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    mark_dirty_rect(x, y, x + w - 1, y + h - 1);
}

/// Current cursor position (top-left of the sprite).
static CUR_X: AtomicUsize = AtomicUsize::new(0);
static CUR_Y: AtomicUsize = AtomicUsize::new(0);

/// Sub-pixel accumulators in 1/8-pixel units (lossless motion integration).
static mut ACC_X8: i32 = 0;
static mut ACC_Y8: i32 = 0;

/// Convert a raw packet delta into cursor pixels. IRQ-12 only (single writer).
///
/// RAW 1:1 — NO acceleration, NO curve. The pointer stops exactly where the
/// hand stops (the gamers' preference). Sub-pixel remainders accumulate so
/// slow motion stays perfectly smooth and no counts are ever lost.
fn apply_sensitivity(dx: i32, dy: i32) -> (i32, i32) {
    // Windows-style acceleration, per axis, in fixed-point 1/8 steps:
    //   |v| <= 7   -> 1.0x   (pixel-perfect slow motion, zero loss)
    //   |v| ~ 32   -> 2.0x   (everyday desktop speed)
    //   |v| >= 96  -> 4.0x   (fast flicks cross the screen)
    // Motion is integrated in 1/8-pixel units, so remainders carry over and
    // no raw counts are ever dropped — slow drags glide instead of stepping.
    let gain = |v: i32| -> i32 { (8 + (v.abs() >> 2)).min(32) };
    unsafe {
        let ax8 = *core::ptr::addr_of!(ACC_X8) + dx * gain(dx);
        let ay8 = *core::ptr::addr_of!(ACC_Y8) + dy * gain(dy);
        let px = ax8 >> 3;
        let py = ay8 >> 3;
        *core::ptr::addr_of_mut!(ACC_X8) = ax8 & 7;
        *core::ptr::addr_of_mut!(ACC_Y8) = ay8 & 7;
        (px, py)
    }
}

/// Sprite: `X` = black outline, `#` = white fill, `.` = transparent.
/// Sized for 1080p (a 10x16 sprite disappears on a 1920x1080 desktop).
const CURSOR_W: usize = 16;
const CURSOR_H: usize = 24;
const CURSOR: [&str; CURSOR_H] = [
    "X...............",
    "XX..............",
    "X#X.............",
    "X##X............",
    "X###X...........",
    "X####X..........",
    "X#####X.........",
    "X######X........",
    "X#######X.......",
    "X########X......",
    "X#########X.....",
    "X##########X....",
    "X###########X...",
    "X#####XXXXXXX...",
    "X#X##X..........",
    "X##..X##X.......",
    "X#....X##X......",
    "XX....X##X......",
    "......X##X......",
    ".......X##X.....",
    ".......X##X.....",
    "........X#X.....",
    "........XX......",
    "................",
];

/// Max sprite pixel scale (set from resolution at boot: 2 on >= 1600 px wide).
const CURSOR_SCALE_MAX: usize = 2;
/// Active cursor pixel scale (updated by `init_global`, read on every draw).
static CUR_SCALE: AtomicUsize = AtomicUsize::new(1);

/// Backing store for the pixels under the sprite. The saved region is the
/// *scaled* sprite (scale ≤ CURSOR_SCALE_MAX per axis), up to 4 bytes/px.
const SAVE_BYTES: usize = CURSOR_W * CURSOR_SCALE_MAX * CURSOR_H * CURSOR_SCALE_MAX * 4;
static mut SAVE: [u8; SAVE_BYTES] = [0; SAVE_BYTES];
static mut SAVE_X: usize = 0;
static mut SAVE_Y: usize = 0;
/// Snapshot of `SAVE` from before the current move (background under the
/// previous sprite position). The diff-based move reads it while writing the
/// new `SAVE`, so two buffers are required.
static mut SAVE_PREV: [u8; SAVE_BYTES] = [0; SAVE_BYTES];

/// Install the boot-time writer and draw the cursor at the screen centre.
pub fn init_global(writer: FrameBufferWriter<'static>) {
    let (width, height) = (writer.info.width, writer.info.height);
    // Keep the sprite at its native 16x24 — small and precise (user request).
    CUR_SCALE.store(1, Ordering::Relaxed);
    let (spr_w, spr_h) = (CURSOR_W, CURSOR_H);
    let x = (width / 2).min(width.saturating_sub(spr_w));
    let y = (height / 2).min(height.saturating_sub(spr_h));
    CUR_X.store(x, Ordering::Relaxed);
    CUR_Y.store(y, Ordering::Relaxed);

    // The text console shares the bootloader framebuffer: copy the raw
    // buffer pointer out (the writer keeps its own copy; FB and CONSOLE
    // mutexes serialize access, so the two views never alias in time).
    let fb_ptr = writer.framebuffer as *mut [u8];
    let info = writer.info;
    let scale = writer.scale;

    let mut slot = FB.lock();
    *slot = Some(writer);
    if let Some(w) = slot.as_mut() {
        save_region(w, x, y);
        draw_arrow(w, x, y);
    }
    // SAFETY: fb_ptr is a 'static buffer (bootloader framebuffer); the
    // CONSOLE mutex is the sole accessor for the console view.
    let fb: &'static mut [u8] = unsafe { &mut *fb_ptr };
    *CONSOLE.lock() = Some(TextConsole::new(fb, info, scale));
}

/// Current cursor position (for boot logging).
pub fn cursor_pos() -> (usize, usize) {
    (CUR_X.load(Ordering::Relaxed), CUR_Y.load(Ordering::Relaxed))
}

/// M10a: re-point the *kernel console* at a new framebuffer surface after a
/// kernel-controlled modesetting operation (see `gpu::init`).
///
/// Both views of the screen are re-created here:
///   * `FB` — the text writer and the mouse sprite owner (cleared first: the
///     sprite's saved background belongs to the old surface, and letting
///     `move_cursor` diff against a byte range that is no longer the screen
///     would paint garbage);
///   * `CONSOLE` — the scrolling text console.
///
/// The bootloader framebuffer is NOT freed: it stays mapped and simply stops
/// being written to, so a later `adopt` (M10b/c) can switch back to it.
///
/// Enters with a framebuffer the caller owns exclusively — called from the boot
/// path (IF=0, before interrupts exist) so no renderer can be mid-draw. The two
/// mutexes make that explicit rather than assumed.
pub fn adopt(framebuffer: &'static mut [u8], info: FrameBufferInfo) {
    // Clear SAVE/SAVE_PREV: their contents describe the previous surface.
    unsafe {
        let save = &mut *core::ptr::addr_of_mut!(SAVE);
        save.fill(0);
        let prev = &mut *core::ptr::addr_of_mut!(SAVE_PREV);
        prev.fill(0);
    }
    // `new` clears the surface and picks the scale from the width, exactly like
    // the boot path (1080p got scale 2, a 1024-wide mode gets 1).
    let mut writer = FrameBufferWriter::new(framebuffer, info);
    writer.x_pos = 0;
    writer.y_pos = HEADER_BAR_H + 8; // keep the console's top gap
    // The modeset surface has no header yet: draw one so the text area starts
    // under it (the same bar the boot path drew on the old surface).
    writer.draw_header("OnyxOS 0.1");
    let writable = writer.framebuffer as *mut [u8];
    let writer_scale = writer.scale;
    *FB.lock() = Some(writer);
    // Re-arm the cursor at the centre of the new mode.
    let x = (info.width / 2).min(info.width.saturating_sub(CURSOR_W));
    let y = (info.height / 2).min(info.height.saturating_sub(CURSOR_H));
    CUR_X.store(x, Ordering::Relaxed);
    CUR_Y.store(y, Ordering::Relaxed);
    // SAFETY: same discipline as `init_global`: `writable` is the 'static
    // buffer just handed to the writer, and `CONSOLE` is its only other view.
    let fb: &'static mut [u8] = unsafe { &mut *writable };
    *CONSOLE.lock() = Some(TextConsole::new(fb, info, writer_scale));
    // The whole new surface has just been cleared, headered and re-armed: the
    // first present must push ALL of it. (The console's own clear_row already
    // marked most of it, but relying on that would make correctness depend on
    // a draw path's internals — this states the invariant outright.)
    mark_all_dirty(info.width, info.height);
}

/// `(width, height, bytes_per_pixel)` of the surface the console is drawing
/// into (`(0, 0, 0)` before `init_global`). Used for `[gpu]` fallback lines.
pub fn current_geometry() -> (usize, usize, usize) {
    match FB.lock().as_ref() {
        Some(w) => (w.info.width, w.info.height, w.info.bytes_per_pixel),
        None => (0, 0, 0),
    }
}

/// Apply a mouse delta and redraw. Called from IRQ 12 — keep it allocation-
/// free and fast (it is ~500 byte writes for a full-sprite move).
pub fn move_cursor(dx: i32, dy: i32) {
    let mut slot = FB.lock();
    let w = match slot.as_mut() {
        Some(w) => w,
        None => return,
    };
    let (width, height) = (w.info.width, w.info.height);
    let old_x = CUR_X.load(Ordering::Relaxed);
    let old_y = CUR_Y.load(Ordering::Relaxed);

    // Apply the Windows-style acceleration curve (lossless 1/8-px integration).
    let (mdx, mdy) = apply_sensitivity(dx, dy);
    if mdx == 0 && mdy == 0 {
        return; // sub-pixel motion only; it stays accumulated for next packet
    }

    // Clamp so the whole sprite stays on screen.
    let cs = CUR_SCALE.load(Ordering::Relaxed).max(1);
    let max_x = width.saturating_sub(CURSOR_W * cs) as i64;
    let max_y = height.saturating_sub(CURSOR_H * cs) as i64;
    let nx = ((old_x as i64 + mdx as i64).clamp(0, max_x)) as usize;
    let ny = ((old_y as i64 + mdy as i64).clamp(0, max_y)) as usize;
    if nx == old_x && ny == old_y {
        return; // pinned at an edge with movement pushing into it
    }

    // Snapshot the current save buffer, then move the sprite with a diff:
    //   1. restore every old-sprite pixel the NEW sprite will not paint
    //      opaquely (mask rule — a naive rectangle diff leaves ghost trails
    //      inside the overlap where old-opaque meets new-transparent)
    //   2. capture the background under the new position (overlap pixels come
    //      from the snapshot, because the framebuffer there still shows the
    //      old sprite - reading them would corrupt the next restore)
    //   3. paint the sprite at the new position
    // At every instant the sprite is visible somewhere on screen, so a
    // display scanout can never catch a frame with the cursor "popped out".
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(SAVE) as *const u8,
            core::ptr::addr_of_mut!(SAVE_PREV) as *mut u8,
            SAVE_BYTES,
        );
        restore_vacated(w, old_x, old_y, nx, ny);
        capture_save(w, nx, ny, old_x, old_y);
    }
    draw_arrow(w, nx, ny);
    CUR_X.store(nx, Ordering::Relaxed);
    CUR_Y.store(ny, Ordering::Relaxed);
}

/// Copy the pixels under (x, y) into the save buffer (raw bytes, so this is
/// format-agnostic — Rgb/Bgr/U8 all round-trip losslessly).
fn save_region(w: &mut FrameBufferWriter, x: usize, y: usize) {
    let bpp = w.info.bytes_per_pixel as usize;
    let stride = w.info.stride as usize;
    let cs = CUR_SCALE.load(Ordering::Relaxed).max(1);
    let (spr_w, spr_h) = (CURSOR_W * cs, CURSOR_H * cs);
    unsafe {
        *core::ptr::addr_of_mut!(SAVE_X) = x;
        *core::ptr::addr_of_mut!(SAVE_Y) = y;
        let save = &mut *core::ptr::addr_of_mut!(SAVE);
        for row in 0..spr_h {
            let fy = y + row;
            if fy >= w.info.height {
                break;
            }
            for col in 0..spr_w {
                let fx = x + col;
                if fx >= w.info.width {
                    break;
                }
                let i = (fy * stride + fx) * bpp;
                if i + bpp > w.framebuffer.len() {
                    break;
                }
                let s = (row * spr_w + col) * 4;
                for k in 0..bpp.min(4) {
                    save[s + k] = w.framebuffer[i + k];
                }
            }
        }
    }
}

/// Restore the background pixels the sprite vacates: every pixel of the old
/// sprite rect that the new rect does NOT cover is written back from the
/// snapshot buffer. Overlap pixels are left untouched (they still show the
/// sprite until it is repainted at the new position), so the cursor never
/// disappears mid-move.
unsafe fn restore_vacated(
    w: &mut FrameBufferWriter,
    ox: usize,
    oy: usize,
    nx: usize,
    ny: usize,
) {
    let bpp = w.info.bytes_per_pixel as usize;
    let stride = w.info.stride as usize;
    let cs = CUR_SCALE.load(Ordering::Relaxed).max(1);
    let (sw, sh) = (CURSOR_W * cs, CURSOR_H * cs);
    let prev = &*core::ptr::addr_of!(SAVE_PREV);
    for row in 0..sh {
        let fy = oy + row;
        if fy >= w.info.height {
            break;
        }
        for col in 0..sw {
            let fx = ox + col;
            if fx >= w.info.width {
                break;
            }
            // Restore unless the NEW sprite paints this pixel opaquely.
            // Rectangle overlap alone is WRONG: inside the overlap, an old
            // opaque pixel can fall on a TRANSPARENT pixel of the new sprite
            // and would never be cleaned — leaving permanent ghost trails.
            let (ncol, nrow) = (fx as i64 - nx as i64, fy as i64 - ny as i64);
            if ncol >= 0
                && nrow >= 0
                && (ncol as usize) < CURSOR_W
                && (nrow as usize) < CURSOR_H
                && CURSOR[nrow as usize].as_bytes()[ncol as usize] != b'.'
            {
                continue; // the new sprite opaquely covers this pixel
            }
            let i = (fy * stride + fx) * bpp;
            if i + bpp > w.framebuffer.len() {
                break;
            }
            let s = (row * sw + col) * 4;
            for k in 0..bpp.min(4) {
                w.framebuffer[i + k] = prev[s + k];
            }
        }
    }
}

/// Capture the background under the new sprite position into `SAVE`.
/// Overlap pixels (currently showing the old sprite) are sourced from the
/// snapshot buffer, which holds the true background there.
unsafe fn capture_save(
    w: &mut FrameBufferWriter,
    nx: usize,
    ny: usize,
    ox: usize,
    oy: usize,
) {
    let bpp = w.info.bytes_per_pixel as usize;
    let stride = w.info.stride as usize;
    let cs = CUR_SCALE.load(Ordering::Relaxed).max(1);
    let (sw, sh) = (CURSOR_W * cs, CURSOR_H * cs);
    let prev = &*core::ptr::addr_of!(SAVE_PREV);
    let save = &mut *core::ptr::addr_of_mut!(SAVE);
    *core::ptr::addr_of_mut!(SAVE_X) = nx;
    *core::ptr::addr_of_mut!(SAVE_Y) = ny;
    for row in 0..sh {
        let fy = ny + row;
        if fy >= w.info.height {
            break;
        }
        for col in 0..sw {
            let fx = nx + col;
            if fx >= w.info.width {
                break;
            }
            let d = (row * sw + col) * 4;
            if fx >= ox && fx < ox + sw && fy >= oy && fy < oy + sh {
                let s = ((fy - oy) * sw + (fx - ox)) * 4;
                for k in 0..bpp.min(4) {
                    save[d + k] = prev[s + k];
                }
            } else {
                let i = (fy * stride + fx) * bpp;
                if i + bpp > w.framebuffer.len() {
                    break;
                }
                for k in 0..bpp.min(4) {
                    save[d + k] = w.framebuffer[i + k];
                }
            }
        }
    }
}

/// Stamp the arrow sprite at (x, y). Every sprite pixel becomes a
/// `cs × cs` block so the cursor's apparent size tracks the resolution
/// (2× on a 1920-wide panel — same physical size as 1× on 960-wide).
fn draw_arrow(w: &mut FrameBufferWriter, x: usize, y: usize) {
    let cs = CUR_SCALE.load(Ordering::Relaxed).max(1);
    for (row, line) in CURSOR.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let rgb = match ch {
                'X' => 0x000000,
                '#' => 0xFFFFFF,
                _ => continue,
            };
            for sy in 0..cs {
                for sx in 0..cs {
                    w.set_pixel(x + col * cs + sx, y + row * cs + sy, rgb);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Header-bar clock (RTC quick-win)
// ---------------------------------------------------------------------------

/// Header bar height — must match `draw_header` (bar is 48 px, title at y=14).
const HEADER_BAR_H: usize = 48;
const HEADER_BLUE: u32 = 0x0B5ED7;

/// Redraw the wall clock in the top-right corner of the header bar
/// (e.g. `draw_clock("14:07:32")`). Called once per second by the clock
/// task; erases the previous rendering first.
///
/// Runs with interrupts disabled inside the lock: the mouse IRQ handler
/// (`move_cursor`) takes the same `FB` mutex, and a spinning IRQ on a lock
/// held by the task it just interrupted can never be released. The redraw
/// touches ~2k pixels — microseconds with IF=0, so input latency is intact.
pub fn draw_clock(text: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut slot = FB.lock();
        let Some(w) = slot.as_mut() else {
            return;
        };
        let s = w.scale;
        // Right-aligned in the bar: text width + margins.
        let text_w = text.chars().count() * 9 * s;
        let x = w.info.width.saturating_sub(text_w + 16);
        let y: usize = 14;
        // Erase the previous rendering (glyph box + margin).
        for ey in y.saturating_sub(6)..(y + 8 * s + 6).min(HEADER_BAR_H) {
            for ex in x.saturating_sub(8)..(x + text_w + 8).min(w.info.width) {
                w.set_pixel(ex, ey, HEADER_BLUE);
            }
        }
        let (saved_x, saved_y) = (w.x_pos, w.y_pos);
        w.x_pos = x;
        w.y_pos = y;
        for c in text.chars() {
            if (c as usize) < MAX_ASCII {
                w.write_char(c);
            }
        }
        w.x_pos = saved_x;
        w.y_pos = saved_y;
    });
}

// ---------------------------------------------------------------------------
// Scrolling text console (live terminal on the framebuffer)
// ---------------------------------------------------------------------------
// Typed keys + shell output appear on screen. It draws into the SAME buffer
// as the writer and the mouse cursor sprite; only the text cursor/scroll
// state is separate. Glyph metrics match FrameBufferWriter (glyph 8x8 at
// scale `s`, advance 9s, row height 10s) so both render identically.

/// Live scrolling text console over the framebuffer region below the header.
pub struct TextConsole {
    framebuffer: &'static mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
    pixel_format: PixelFormat,
    scale: usize,
    x: usize,
    y: usize,
    top: usize,
    bottom: usize,
}

impl TextConsole {
    pub fn new(fb: &'static mut [u8], info: FrameBufferInfo, scale: usize) -> Self {
        let top = HEADER_BAR_H + 8; // just below the 48px header bar
        let mut c = Self {
            framebuffer: fb,
            width: info.width,
            height: info.height,
            stride: info.stride,
            bytes_per_pixel: info.bytes_per_pixel as usize,
            pixel_format: info.pixel_format,
            scale: scale.max(1),
            x: 0,
            y: top,
            top,
            bottom: info.height,
        };
        // Wipe the console region: the terminal starts clean instead of
        // blending glyphs with the static boot text drawn at the same rows.
        for y in top..c.bottom {
            c.clear_row(y);
        }
        c
    }

    fn glyph_w(&self) -> usize {
        9 * self.scale
    }
    fn row_h(&self) -> usize {
        10 * self.scale
    }

    fn set_pixel(&mut self, x: usize, y: usize, rgb: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.stride + x) * self.bytes_per_pixel;
        if i + 3 > self.framebuffer.len() {
            return;
        }
        // Every console pixel write flows through here, so this is the ONE
        // place that has to know about damage. Tracking it at the single
        // chokepoint is what keeps the invariant true: if a future draw path
        // forgets to mark itself dirty, it shows up here as a missing rect
        // rather than as a subtly wrong frame on screen.
        mark_dirty(x, y);
        let r = ((rgb >> 16) & 0xff) as u8;
        let g = ((rgb >> 8) & 0xff) as u8;
        let b = (rgb & 0xff) as u8;
        match self.pixel_format {
            PixelFormat::Rgb => {
                self.framebuffer[i] = r;
                self.framebuffer[i + 1] = g;
                self.framebuffer[i + 2] = b;
            }
            PixelFormat::Bgr => {
                self.framebuffer[i] = b;
                self.framebuffer[i + 1] = g;
                self.framebuffer[i + 2] = r;
            }
            PixelFormat::U8 => {
                self.framebuffer[i] = ((r as u32 + g as u32 + b as u32) / 3) as u8;
            }
            _ => {}
        }
    }

    fn clear_row(&mut self, y: usize) {
        if y >= self.bottom {
            return;
        }
        // A cleared row is a full-width dirty band; mark it as a whole row so
        // the flusher still covers it (clear_row writes bytes directly, not
        // through set_pixel, so it would otherwise leave no trace at all).
        if y < self.height {
            mark_dirty_rect(0, y, self.width.saturating_sub(1), y);
        }
        let bpp = self.bytes_per_pixel.max(1);
        let i0 = y * self.stride * bpp;
        for x in 0..self.width {
            let i = i0 + x * bpp;
            if i + 3 > self.framebuffer.len() {
                break;
            }
            // Background slate (matches clear_bg) in this pixel format.
            let rgb = match self.pixel_format {
                PixelFormat::Rgb => (0x0Eu8, 0x14u8, 0x20u8),
                PixelFormat::Bgr => (0x20u8, 0x14u8, 0x0Eu8),
                _ => (((0x0Eu32 + 0x14 + 0x20) / 3) as u8, 0, 0),
            };
            self.framebuffer[i] = rgb.0;
            if bpp >= 2 {
                self.framebuffer[i + 1] = rgb.1;
            }
            if bpp >= 3 {
                self.framebuffer[i + 2] = rgb.2;
            }
        }
    }

    /// Shift the console region up by `px` rows: memmove row r+px -> row r,
    /// then clear the vacated bottom rows.
    fn scroll_up(&mut self, px: usize) {
        if px == 0 {
            return;
        }
        // The memmove below rewrites whole rows without going through
        // set_pixel, so the damaged band is the ENTIRE console region, not just
        // the vacated rows. Marking only the bottom would leave stale text in
        // every row that moved - the classic off-by-px scroll bug.
        mark_dirty_rect(0, self.top, self.width.saturating_sub(1), self.bottom.saturating_sub(1));
        let bpp = self.bytes_per_pixel.max(1);
        let row_bytes = self.stride * bpp;
        if px >= self.bottom - self.top {
            for y in self.top..self.bottom {
                self.clear_row(y);
            }
            return;
        }
        unsafe {
            for y in (self.top + px)..self.bottom {
                let src = (y * self.stride) * bpp;
                let dst = ((y - px) * self.stride) * bpp;
                core::ptr::copy_nonoverlapping(
                    self.framebuffer.as_ptr().add(src),
                    self.framebuffer.as_mut_ptr().add(dst),
                    row_bytes,
                );
            }
        }
        for y in (self.bottom - px)..self.bottom {
            self.clear_row(y);
        }
    }

    fn clear_cell_at(&mut self, x: usize, y: usize) {
        let (gw, rh) = (self.glyph_w(), self.row_h());
        for yy in y..(y + rh).min(self.bottom) {
            for xx in x..(x + gw).min(self.width) {
                self.set_pixel(xx, yy, 0x0E1420);
            }
        }
    }

    fn write_char(&mut self, c: char) {
        if (c as usize) >= MAX_ASCII {
            return;
        }
        let glyph = font8x8::legacy::BASIC_LEGACY[c as usize];
        let s = self.scale;
        // Wrap before drawing if the glyph would overflow the right edge.
        if self.x + 8 * s > self.width {
            self.newline();
        }
        for (row, &byte) in glyph.iter().enumerate() {
            for col in 0..8u8 {
                if (byte >> col) & 1 == 1 {
                    for sy in 0..s {
                        for sx in 0..s {
                            self.set_pixel(
                                self.x + col as usize * s + sx,
                                self.y + row * s + sy,
                                0xFFFFFF,
                            );
                        }
                    }
                }
            }
        }
        self.x += 9 * s;
    }

    fn newline(&mut self) {
        self.x = 0;
        self.y += self.row_h();
        if self.y >= self.bottom {
            self.scroll_up(self.row_h());
            self.y = self.bottom - self.row_h();
        }
    }

    fn backspace(&mut self) {
        if self.x >= self.glyph_w() {
            self.x -= self.glyph_w();
            self.clear_cell_at(self.x, self.y);
        }
    }

    fn write_byte(&mut self, b: u8) {
        match b {
            0x0D => self.x = 0,
            0x0A => self.newline(),
            0x08 | 0x7F => self.backspace(),
            b if b >= 0x20 && b < 0x7F => self.write_char(b as char),
            _ => {}
        }
    }
}

/// Mirror `data` onto the framebuffer text console (no-op if not installed).
/// Afterwards the mouse cursor's save-region is re-captured when the cursor
/// sits inside the console region — otherwise the sprite's restore would
/// repaint PRE-console pixels (stale "previous text") as the cursor moves.
///
/// Runs the WHOLE mirror (both lock windows) with interrupts disabled — same
/// discipline as `draw_clock` and the serial macro: the CONSOLE and FB locks
/// must never be held across a preemption. Re-acquirers include ring-3
/// `SYS_WRITE` echoes and `take_line`'s typed-key echo, which run from IF=0
/// syscall context: a task preempted (IF=1) mid-draw while holding CONSOLE
/// would let the next task spin on it forever at IF=0 — the timer dies with
/// it and the preempted holder can never release (the B5 boot-snapshot
/// freeze, ~1-in-10 runs). The mouse IRQ's `move_cursor` takes FB as well,
/// so `refresh_cursor_save` must also stay inside the IF=0 window. The
/// window is one line of glyphs (~sub-millisecond); the LAPIC periodic timer
/// coalesces the ticks landing in it, matching the observed apic_ms drift.
pub fn console_bytes(data: &[u8]) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        {
            let mut g = CONSOLE.lock();
            let Some(c) = g.as_mut() else { return };
            for &b in data {
                c.write_byte(b);
            }
            CONSOLE_BYTES.fetch_add(data.len(), Ordering::Relaxed);
        }
        refresh_cursor_save();
    });
}

/// Re-capture the cursor's save region + redraw the sprite, so the
/// save/restore pair always reflects the latest console pixels underneath.
fn refresh_cursor_save() {
    let cx = CUR_X.load(Ordering::Relaxed);
    let cy = CUR_Y.load(Ordering::Relaxed);
    if cy < HEADER_BAR_H + 8 {
        return; // cursor outside the console region: nothing to refresh
    }
    let mut slot = FB.lock();
    if let Some(w) = slot.as_mut() {
        save_region(w, cx, cy);
        draw_arrow(w, cx, cy);
    }
}

/// Total bytes mirrored to the console (serial-observable liveness proof).
pub fn console_bytes_total() -> usize {
    CONSOLE_BYTES.load(Ordering::Relaxed)
}
