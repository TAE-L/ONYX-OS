//! P6c — the **sign-in screen**: the first thing the user sees at boot.
//!
//! # Why this draws into the REAL framebuffer, not a surface
//!
//! The obvious move was to render with `compositor` + `surface`, which already
//! do fonts, damage and back-to-front compositing. That does not work yet:
//!
//! * Surface damage is PER-SURFACE (`Surface::damage`). The present flusher
//!   reads `framebuffer::take_dirty_rect()` — a separate accumulator. A
//!   composited frame therefore sits in a surface and never reaches scanout.
//! * Wiring them together needs the blit loop, which is the next stage.
//!
//! So this follows the precedent of `draw_clock` / `draw_probe_marker` /
//! `draw_anim_frame`: take the FB lock, draw, then `mark_dirty_rect`. That is
//! the one path already proven to reach the screen. When the compositor is
//! bridged to scanout, this file's drawing becomes a compositor layer and only
//! the plumbing below moves.
//!
//! # Locking
//!
//! One FB acquisition per public entry point, IF=0, no nested locks — the same
//! shape as every other console draw. Damage is marked AFTER FB is released, so
//! DIRTY is never taken while FB is held.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use crate::framebuffer::FrameBufferWriter;

// ---------------------------------------------------------------------------
// Palette — a Windows 11 sign-in look, not an invented one
// ---------------------------------------------------------------------------

/// The Win11 "bloom"-ish blue that dominates the default wallpaper.
pub const BG_TOP: u32 = 0x0C2E74;
pub const BG_BOT: u32 = 0x05122F;
/// Power-menu panel fill.
pub const CARD: u32 = 0x16294F;
/// Panel border.
pub const CARD_EDGE: u32 = 0x35558F;
/// Primary text.
pub const TEXT: u32 = 0xF4F7FF;
/// De-emphasised text.
pub const TEXT_DIM: u32 = 0xA9BADD;
/// Accent for the selected power item and the avatar ring.
pub const ACCENT: u32 = 0x7CBAFF;
/// Avatar disc fill.
pub const AVATAR_BG: u32 = 0x2F63BE;
/// Avatar initial.
pub const AVATAR_FG: u32 = 0xFFFFFF;

/// The signed-in user's display name. No passwords yet, so this is a fixed label
/// rather than anything read from a credential store.
pub const USER_NAME: &str = "Admin";

/// Power menu geometry (bottom-right), in pixels at a 655 px tall screen.
///
/// These are BASE values; [`ui_scale`] multiplies them for a larger mode. They
/// were authored against a ~655 px tall reference, so on a 1080p panel they must
/// grow or the menu shrinks into a corner while the sign-in block beside it
/// scales — which is exactly the mismatch that made the menu labels overflow
/// their own rows.
const POWER_BTN: usize = 48;
const POWER_MENU_W: usize = 196;
const POWER_ITEM_H: usize = 36;
const POWER_PAD: usize = 10;
/// Inset from the screen edge to the power button.
const POWER_MARGIN: usize = 30;

/// UI scale factor for the power widget: 1 at the ~655 px tall reference these
/// constants were authored for, 2 at 1080p. Capped at 2 so a 4K panel gets a
/// proportionate menu rather than an enormous one.
fn ui_scale(h: usize) -> usize {
    ((h * 3) / 2 / 655).clamp(1, 2)
}

/// Avatar disc diameter, and the ring drawn just outside it.
const AVATAR_D: usize = 132;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub const PHASE_SIGNIN: u8 = 0;
pub const PHASE_DESKTOP: u8 = 1;

static PHASE: AtomicU8 = AtomicU8::new(PHASE_SIGNIN);
/// Power menu open?
static MENU_OPEN: AtomicBool = AtomicBool::new(false);
/// Highlighted power item (index into `POWER_ITEMS`).
static SEL: AtomicU8 = AtomicU8::new(0);
/// Set once the user has signed in, so the boot log can prove the action ran.
static SIGNED_IN: AtomicBool = AtomicBool::new(false);

/// The power menu, in display order. Indices are the values `SEL` holds.
pub const POWER_ITEMS: [&str; 3] = ["Sleep", "Restart", "Shut down"];

pub fn phase() -> u8 {
    PHASE.load(Ordering::Acquire)
}
pub fn signed_in() -> bool {
    SIGNED_IN.load(Ordering::Acquire)
}
pub fn menu_open() -> bool {
    MENU_OPEN.load(Ordering::Acquire)
}
pub fn selected() -> usize {
    SEL.load(Ordering::Acquire) as usize
}

// ---------------------------------------------------------------------------
// Geometry — shared by draw and hit-test so they cannot disagree
// ---------------------------------------------------------------------------

/// Where the power button and its menu live, in screen pixels.
///
/// Computed from the CURRENT screen size on every call, and BOTH `draw` and the
/// hit-tests call this. Layout and hit-testing being written twice and drifting
/// apart is the classic UI bug here; sharing one function makes it impossible.
pub struct PowerGeom {
    pub btn_x: usize,
    pub btn_y: usize,
    pub menu_x: usize,
    pub menu_y: usize,
    pub menu_h: usize,
    /// SCALED row height of one menu item. Carried here (rather than read from
    /// `POWER_ITEM_H`) so the hit-test divides by the same value the panel was
    /// drawn with — the two must not be able to disagree.
    pub item_h: usize,
    /// SCALED inner padding above the first row and below the last.
    pub pad: usize,
    /// SCALED width of the power button.
    pub btn: usize,
    /// SCALED panel width.
    pub menu_w: usize,
}

pub fn power_geom(w: usize, h: usize) -> PowerGeom {
    let s = ui_scale(h);
    let btn = POWER_BTN * s;
    let item_h = POWER_ITEM_H * s;
    let pad = POWER_PAD * s;
    let menu_w = POWER_MENU_W * s;
    let margin = POWER_MARGIN * s;
    let menu_h = POWER_ITEMS.len() * item_h + pad * 2;
    PowerGeom {
        btn_x: w.saturating_sub(margin + btn),
        btn_y: h.saturating_sub(margin + btn),
        // Menu sits directly ABOVE the button, right-aligned to it.
        menu_x: w.saturating_sub(margin + menu_w),
        menu_y: h.saturating_sub(margin + menu_h),
        menu_h,
        item_h,
        pad,
        btn,
        menu_w,
    }
}

/// Is `(x, y)` inside the power button?
pub fn hit_power(w: usize, h: usize, x: usize, y: usize) -> bool {
    let g = power_geom(w, h);
    x >= g.btn_x && x < g.btn_x + g.btn && y >= g.btn_y && y < g.btn_y + g.btn
}

/// If the click is inside the OPEN power menu, the item index it hit.
pub fn hit_power_item(w: usize, h: usize, x: usize, y: usize) -> Option<usize> {
    if !MENU_OPEN.load(Ordering::Acquire) {
        return None;
    }
    let g = power_geom(w, h);
    if x < g.menu_x || x >= g.menu_x + g.menu_w || y < g.menu_y || y >= g.menu_y + g.menu_h {
        return None;
    }
    // Subtract the top padding before dividing, so item 0's row starts exactly
    // at the panel's inner edge.
    let Some(inner) = y.checked_sub(g.menu_y + g.pad) else {
        return None;
    };
    if inner >= POWER_ITEMS.len() * g.item_h {
        return None; // bottom padding strip
    }
    Some(inner / g.item_h)
}

// ---------------------------------------------------------------------------
// Sign-in block layout — avatar, name, "Sign in" button
// ---------------------------------------------------------------------------

/// Where the sign-in block lives, in screen pixels.
///
/// Everything here is derived from the CURRENT screen size rather than being a
/// fixed pixel position, because this screen runs at whatever mode the GPU or
/// bootloader handed over (1080p in practice, 1024x768 in the compositor test).
/// A hardcoded 1080p layout would put the avatar off-screen at a smaller mode.
///
/// Like [`PowerGeom`], the layout is computed by ONE function that both `draw`
/// and the hit-tests call, so the drawn button and the clickable button cannot
/// drift apart.
pub struct SigninGeom {
    pub avatar_cx: i64,
    pub avatar_cy: i64,
    pub avatar_r: i64,
    /// BASELINE of the user name (not its top edge — see the font API).
    pub name_baseline: i64,
    pub btn_x: i64,
    pub btn_y: i64,
    pub btn_w: i64,
    pub btn_h: i64,
    /// Size of the user name, derived from the screen.
    pub fs_name: u16,
    /// Size of the "Sign in" label, derived from the BUTTON height.
    pub fs_btn: u16,
    /// Size of a power-menu label, derived from the MENU ROW height.
    pub fs_item: u16,
}

pub fn signin_geom(w: usize, h: usize) -> SigninGeom {
    let cx = (w / 2) as i64;
    let hh = h as i64;

    // Vertical rhythm as fractions of the screen height, so the block keeps its
    // proportions at any mode. The gaps are expressed as multiples of the text
    // size that follows them rather than as fixed pixels, which is what keeps
    // the name from crowding the avatar (or drifting away from it) when the font
    // size changes with the mode.
    let avatar_r = hh * 11 / 100;
    let avatar_cy = hh * 30 / 100;
    let fs_name = ((hh * 3) / 100).max(12) as u16;
    // Baseline sits one name-line below the bottom of the disc.
    let name_baseline = avatar_cy + avatar_r + fs_name as i64 * 5 / 4;

    let btn_h = hh * 6 / 100;
    let btn_w = btn_h * 7 / 2;
    // One name-line of breathing room between the name and the button.
    let btn_y = name_baseline + fs_name as i64 * 3 / 2;

    SigninGeom {
        avatar_cx: cx,
        avatar_cy,
        avatar_r,
        name_baseline,
        btn_x: cx - btn_w / 2,
        btn_y,
        btn_w,
        btn_h,
        fs_name,
        // Each label is sized from the box it has to fit inside, NOT from the
        // screen. Deriving both from one screen-level number is what let the
        // menu labels outgrow their 36 px rows.
        fs_btn: ((btn_h * 40) / 100).max(11) as u16,
        fs_item: ((POWER_ITEM_H * ui_scale(h) * 42) / 100).max(11) as u16,
    }
}

/// What a click at `(x, y)` means, resolved WITHOUT performing it.
///
/// Splitting classification from action is what makes this screen testable: the
/// self-test can assert that a click on the "Restart" row resolves to
/// `MenuItem(1)` without that actually rebooting the machine mid-test. It is
/// also why [`Click::MenuItem`] is only produced while the menu is OPEN — a
/// click on those pixels with the menu closed is just a click on the backdrop.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Click {
    /// The power button in the corner.
    PowerButton,
    /// A row of the open power menu, by index into [`POWER_ITEMS`].
    MenuItem(usize),
    /// The big "Sign in" button.
    SignIn,
    /// Anywhere else — used to dismiss the menu.
    Outside,
}

/// Classify a click. Pure: reads geometry and the menu-open flag, changes
/// nothing and triggers nothing.
pub fn classify_click(w: usize, h: usize, x: usize, y: usize) -> Click {
    if let Some(idx) = hit_power_item(w, h, x, y) {
        return Click::MenuItem(idx);
    }
    if hit_power(w, h, x, y) {
        return Click::PowerButton;
    }
    let g = signin_geom(w, h);
    if x >= g.btn_x.max(0) as usize
        && (x as i64) < g.btn_x + g.btn_w
        && y >= g.btn_y.max(0) as usize
        && (y as i64) < g.btn_y + g.btn_h
    {
        return Click::SignIn;
    }
    Click::Outside
}

/// Is `(x, y)` inside the "Sign in" button?
pub fn hit_signin(w: usize, h: usize, x: usize, y: usize) -> bool {
    classify_click(w, h, x, y) == Click::SignIn
}

// ---------------------------------------------------------------------------
// Drawing primitives
// ---------------------------------------------------------------------------

/// Blend `rgb` over the pixel at `(x, y)` at alpha `a` (0..=255).
///
/// P6c: made public so the desktop's Text Lab can draw its baseline rule the
/// same way glyphs are composited, rather than writing an opaque pixel and
/// punching a visible notch through the window body.
#[inline]
pub fn blend_px(w: &mut FrameBufferWriter, x: usize, y: usize, rgb: u32, a: u8) {
    if a == 0 {
        return;
    }
    if a >= 255 {
        w.set_pixel(x, y, rgb);
        return;
    }
    let Some(dst) = w.get_pixel(x, y) else {
        return;
    };
    let inv = 255 - a as u32;
    let ch = |s: u32| -> u32 { ((rgb >> s) & 0xff) * a as u32 + ((dst >> s) & 0xff) * inv };
    let nr = ch(16) / 255;
    let ng = ch(8) / 255;
    let nb = ch(0) / 255;
    w.set_pixel(x, y, (nr << 16) | (ng << 8) | nb);
}

/// The backdrop colour of scanline `y` on an `h`-tall screen.
///
/// Extracted from [`draw_backdrop`] and shared with it, because the power
/// glyphs need to PUNCH a gap out of their own ring — repainting those pixels
/// with a flat colour instead of the gradient would leave a visible notch. One
/// function, so the erased pixels are by construction identical to the ones the
/// backdrop would have put there.
fn bg_row(y: usize, h: usize) -> u32 {
    let t: u32 = if h <= 1 { 0 } else { (y as u32 * 255) / (h as u32 - 1) };
    let inv = 255 - t;
    let mix = |s: u32| -> u32 { (((BG_TOP >> s) & 0xff) * inv + ((BG_BOT >> s) & 0xff) * t) / 255 };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// Vertical gradient backdrop.
///
/// Windows' sign-in wallpaper is a top-lit bloom; a vertical ramp is the closest
/// cheap match and costs exactly one pass. Drawn as whole scanlines via
/// `fill_row`, so the inner loop stays a straight byte write rather than a
/// per-pixel format switch.
fn draw_backdrop(w: &mut FrameBufferWriter, width: usize, height: usize) {
    for y in 0..height {
        w.fill_row(y, bg_row(y, height), width);
    }
}

/// Integer half-width of a circle scanline: `floor(sqrt(r^2 - dy^2))`.
///
/// Exact integer sqrt with no floating point (the kernel has no FPU contract
/// here). A plain bounded walk, bounded by `r`, so O(r) per row and O(r^2) per
/// circle - fine for the handful of circles this screen draws, and exact where
/// a float would round.
#[inline]
fn circle_hw(dy: i64, r2: i64) -> i64 {
    let inner = r2 - dy * dy;
    if inner <= 0 {
        return 0;
    }
    let mut hw = 0i64;
    while (hw + 1) * (hw + 1) <= inner {
        hw += 1;
    }
    hw
}

/// Filled circle, drawn as horizontal spans.
fn fill_circle(w: &mut FrameBufferWriter, cx: i64, cy: i64, r: i64, rgb: u32) {
    if r <= 0 {
        return;
    }
    let r2 = r * r;
    for dy in -r..=r {
        let y = cy + dy;
        if y < 0 {
            continue;
        }
        let hw = circle_hw(dy, r2);
        for x in (cx - hw)..=(cx + hw) {
            if x >= 0 {
                w.set_pixel(x as usize, y as usize, rgb);
            }
        }
    }
}

/// Circle OUTLINE of thickness `t` - the avatar's accent ring and the power
/// glyph's ring.
///
/// Paints only the annulus directly rather than filling the disc and punching a
/// hole: punching would have to repaint the interior, and the interior here is
/// the BACKDROP GRADIENT, which a solid fill would flatten into a visible disc.
fn stroke_circle(w: &mut FrameBufferWriter, cx: i64, cy: i64, r: i64, t: i64, rgb: u32) {
    if r <= 0 || t <= 0 {
        return;
    }
    let outer2 = r * r;
    let inner_r = (r - t).max(0);
    let inner2 = inner_r * inner_r;
    for dy in -r..=r {
        let y = cy + dy;
        if y < 0 {
            continue;
        }
        let hw = circle_hw(dy, outer2);
        for x in (cx - hw)..=(cx + hw) {
            if x < 0 {
                continue;
            }
            let dx = x - cx;
            if dx * dx + dy * dy >= inner2 {
                w.set_pixel(x as usize, y as usize, rgb);
            }
        }
    }
}

/// Filled circle at fractional alpha — the avatar disc, which sits over the
/// gradient and must not flatten it into a flat blue dot.
fn fill_circle_a(w: &mut FrameBufferWriter, cx: i64, cy: i64, r: i64, rgb: u32, a: u8) {
    if r <= 0 {
        return;
    }
    let r2 = r * r;
    for dy in -r..=r {
        let y = cy + dy;
        if y < 0 {
            continue;
        }
        let hw = circle_hw(dy, r2);
        for x in (cx - hw)..=(cx + hw) {
            if x >= 0 {
                blend_px(w, x as usize, y as usize, rgb, a);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Text — Lato via the shared glyph cache
// ---------------------------------------------------------------------------

/// Advance width of `text` at `size`, in pixels.
///
/// Wraps [`crate::font::measure`] so there is exactly ONE answer to "how wide
/// is this label" in this file — the same one `draw_text` advances by. Centring
/// with one measure and drawing with another lands the label a glyph off-centre.
///
/// P6c: made public so the desktop's Text Lab can centre a label it does not
/// itself draw, without growing a second definition of "how wide is this string"
/// that could drift from the one the glyphs are actually advanced by.
pub fn measure(text: &str, size: u16) -> i32 {
    match crate::font::Face::load() {
        Ok(f) => crate::font::measure(&f, text, size),
        Err(_) => 0,
    }
}

/// Extra tracking (pixels between glyphs) and a baseline shift, applied to a
/// whole run.
///
/// Kept as a type rather than two loose `i32` arguments because these are the
/// knobs a text customizer needs, and every caller that wants one has to pass
/// both. The Text Lab prototype that first needed them has been removed; the
/// default is the font's own metrics, so a plain call is unaffected.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TextStyle {
    /// Pixels added to the pen advance AFTER each glyph. Negative tightens.
    pub tracking: i32,
    /// Pixels added DOWN to every baseline in the run.
    pub baseline_shift: i32,
}

/// Draw `text` with its BASELINE at `baseline`, antialiased, blending against
/// whatever is already on screen.
///
/// Returns the advance width consumed, so a caller can lay out a row of mixed
/// runs without re-measuring. Glyphs come from `surface::cached_glyph`, which
/// is cache-first by design — see that function's note on the allocation storm
/// a per-character re-rasterize used to cause.
fn draw_text(
    w: &mut FrameBufferWriter,
    x: i64,
    baseline: i64,
    text: &str,
    size: u16,
    rgb: u32,
) -> i32 {
    draw_text_styled(w, x, baseline, text, size, rgb, TextStyle::default())
}

/// [`draw_text`] with an explicit [`TextStyle`].
///
/// `baseline` is the DEFAULT baseline; `style.baseline_shift` moves it, so a
/// caller can measure and centre once and still get an adjustable run.
fn draw_text_styled(
    w: &mut FrameBufferWriter,
    x: i64,
    baseline: i64,
    text: &str,
    size: u16,
    rgb: u32,
    style: TextStyle,
) -> i32 {
    let baseline = baseline + style.baseline_shift as i64;
    let mut pen = x;
    for ch in text.chars() {
        let Some(g) = crate::surface::cached_glyph(ch, size) else {
            continue;
        };
        if g.w > 0 && g.h > 0 {
            // `bearing_y` is the distance UP from the baseline to the mask top.
            // The glyph's metrics are `i32`; widening here keeps every position
            // computation below in one type instead of casting at each use.
            let gx = pen + g.bearing_x as i64;
            let gy = baseline - g.bearing_y as i64;
            for row in 0..g.h {
                for col in 0..g.w {
                    let a = g.alpha[row * g.w + col];
                    if a == 0 {
                        continue;
                    }
                    let px = gx + col as i64;
                    let py = gy + row as i64;
                    if px >= 0 && py >= 0 {
                        blend_px(w, px as usize, py as usize, rgb, a);
                    }
                }
            }
        }
        pen += g.advance as i64 + style.tracking as i64;
    }
    // Exclude the trailing tracking: a run's width is what it INKED, not the gap
    // after the final glyph, so centring with this stays stable.
    (pen - x) as i32 - style.tracking
}

/// Draw `text` horizontally centred on `cx`.
///
/// P6c: made public so the desktop can label itself without growing a second
/// centring implementation that could drift from this one by a glyph.
pub fn draw_text_centered(
    w: &mut FrameBufferWriter,
    cx: i64,
    baseline: i64,
    text: &str,
    size: u16,
    rgb: u32,
) {
    let width = measure(text, size) as i64;
    draw_text(w, cx - (width / 2), baseline, text, size, rgb);
}

// ---------------------------------------------------------------------------
// Power glyphs — a ring with a gap, and the stem through it
// ---------------------------------------------------------------------------

/// Stroke the arc of a circle between two angles (degrees, 0 = 3 o'clock,
/// increasing CLOCKWISE because screen Y grows downward).
///
/// The power symbol is a broken ring, so a full [`stroke_circle`] cannot draw
/// it: the gap has to be genuinely absent, not painted over afterwards with a
/// flat colour that would not match the gradient behind it.
///
/// No trigonometry and no floating point — the kernel has no FPU contract here.
/// This is an exact integer walk over a 64-entry unit-circle table.
fn stroke_arc(
    w: &mut FrameBufferWriter,
    cx: i64,
    cy: i64,
    r: i64,
    thickness: i64,
    rgb: u32,
    a0: i32,
    a1: i32,
    steps: i32,
) {
    if r <= 0 || steps <= 0 {
        return;
    }
    // `sin(angle)` for 64 steps around the circle, scaled by 256. Index 0 is
    // 3 o'clock and the index increases CLOCKWISE (screen Y grows downward).
    // Generated from sin(k * 5.625 deg) * 256 and rounded; 1/256 of a radius is
    // well under a pixel at any radius this screen draws.
    const SIN: [i32; 64] = [
        0, 25, 50, 74, 98, 121, 142, 162, 181, 198, 213, 226, 237, 245, 251, 255, //
        256, 255, 251, 245, 237, 226, 213, 198, 181, 162, 142, 121, 98, 74, 50, 25, //
        0, -25, -50, -74, -98, -121, -142, -162, -181, -198, -213, -226, -237, -245, -251,
        -255, //
        -256, -255, -251, -245, -237, -226, -213, -198, -181, -162, -142, -121, -98, -74,
        -50, -25, //
    ];
    let idx = |deg: i32| -> usize { ((((deg % 360) + 360) % 360) * 64 / 360) as usize & 63 };
    for s in 0..=steps {
        let deg = a0 + (a1 - a0) * s / steps;
        let i = idx(deg);
        // cos(angle) == sin(angle + 90 deg), i.e. the table shifted by 16 of its
        // 64 entries. Writing `16 - i` rather than `-i` is what puts the point at
        // (cx + r, cy) for angle 0 instead of (cx, cy + r).
        let sin = SIN[i];
        // `rem_euclid`, not `&`: the index is `usize`, so `16 - i` would
        // UNDERFLOW (and panic in a debug build) for every `i > 16` — which is
        // most of the circle. The subtraction is done in `i32` and wrapped, so
        // angle 270° correctly reads cos = 0 instead of exploding.
        let cos = SIN[(16i32 - i as i32).rem_euclid(64) as usize];
        let px = cx + (r * cos as i64) / 256;
        let py = cy + (r * sin as i64) / 256;
        if thickness > 1 {
            fill_circle(w, px, py, thickness / 2, rgb);
        } else if px >= 0 && py >= 0 {
            w.set_pixel(px as usize, py as usize, rgb);
        }
    }
}

/// The power symbol: a ring open at the top with a stem rising through the gap.
///
/// The gap is centred on 12 o'clock, which in this clockwise convention is 270°.
fn draw_glyph_power(w: &mut FrameBufferWriter, cx: i64, cy: i64, r: i64, rgb: u32) {
    // Sweep 315deg -> 585deg (=225deg): the gap spans 225deg..315deg, i.e. the top.
    stroke_arc(w, cx, cy, r, (r / 4).max(2), rgb, 315, 585, 28);
    // Stem: a vertical bar rising from the ring's top edge to just inside it.
    let stem_h = (r * 3) / 4;
    let stem_w = ((r / 4).max(2) / 2).max(1);
    for dy in 0..stem_h {
        for dx in 0..stem_w {
            let px = cx - stem_w / 2 + dx;
            let py = cy - r - r / 8 + dy;
            if px >= 0 && py >= 0 {
                w.set_pixel(px as usize, py as usize, rgb);
            }
        }
    }
}

/// The restart symbol: a ring with a gap at the top-right and an arrowhead
/// turning back into it — the shape that actually distinguishes "restart" from
/// "shut down" once the labels are only 12 px tall.
fn draw_glyph_restart(w: &mut FrameBufferWriter, cx: i64, cy: i64, r: i64, rgb: u32) {
    // Gap at the top-right (between 270deg and 340deg), so the open end faces up.
    stroke_arc(w, cx, cy, r, (r / 4).max(2), rgb, 340, 630, 30);
    // Arrowhead at the END of the sweep (630deg == 270deg, the top): a filled
    // triangle tapering downwards, i.e. pointing back along the ring.
    let s = (r / 2).max(3);
    let ax = cx;
    let ay = cy - r - r / 8;
    for row in 0..s {
        let half = (s - row) / 2;
        for dx in -half..=half {
            let px = ax + dx;
            let py = ay + row;
            if px >= 0 && py >= 0 {
                w.set_pixel(px as usize, py as usize, rgb);
            }
        }
    }
}

/// Glyph for power menu row `idx`, centred at `(cx, cy)` with radius `r`.
fn draw_power_item_glyph(
    w: &mut FrameBufferWriter,
    idx: usize,
    cx: i64,
    cy: i64,
    r: i64,
    rgb: u32,
) {
    match POWER_ITEMS[idx] {
        "Restart" => draw_glyph_restart(w, cx, cy, r, rgb),
        // "Shut down" and "Sleep" share the power symbol; at this size a crescent
        // moon is mush, and the label beside it carries the meaning. Keeping
        // them on one arm means a new item can never render blank.
        _ => draw_glyph_power(w, cx, cy, r, rgb),
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// The round profile disc with the person glyph inside it.
///
/// The head is a filled circle and the shoulders a disc centred below it,
/// clipped to the avatar's own chord at each row. Clipping per row (rather than
/// intersecting shapes up front) is what keeps the silhouette inside the disc
/// at every size without any trigonometry.
fn draw_avatar(w: &mut FrameBufferWriter, g: &SigninGeom) {
    let (cx, cy, r) = (g.avatar_cx, g.avatar_cy, g.avatar_r);
    fill_circle_a(w, cx, cy, r, AVATAR_BG, 0xE0);
    // Accent ring just outside the disc — the "this is you" affordance.
    stroke_circle(w, cx, cy, r + 2, 2, ACCENT);

    // Head.
    fill_circle(w, cx, cy - r / 4, r / 2, AVATAR_FG);

    // Shoulders: a wide disc whose top half is hidden behind the head, clipped
    // to the avatar so it ends flush with the disc edge.
    let body_r = (r * 3) / 4;
    let body_cy = cy + (r * 5) / 8;
    let r2 = body_r * body_r;
    for dy in -body_r..=body_r {
        let y = body_cy + dy;
        let lim = circle_hw(y - cy, r * r);
        if lim <= 0 {
            continue;
        }
        let hw = circle_hw(dy, r2);
        for x in (cx - hw)..=(cx + hw) {
            // Keep only the part inside the avatar's chord at this row.
            if x - cx <= lim && x - cx >= -lim && x >= 0 && y >= 0 {
                w.set_pixel(x as usize, y as usize, AVATAR_FG);
            }
        }
    }
}

/// The "Sign in" button: a filled rectangle with a 2 px accent border.
fn draw_signin_button(w: &mut FrameBufferWriter, g: &SigninGeom) {
    let (x0, y0) = (g.btn_x, g.btn_y);
    let (bw, bh) = (g.btn_w, g.btn_h);
    for dy in 0..bh {
        let y = y0 + dy;
        if y < 0 {
            continue;
        }
        for dx in 0..bw {
            let x = x0 + dx;
            if x < 0 {
                continue;
            }
            let edge = dx < 2 || dx >= bw - 2 || dy < 2 || dy >= bh - 2;
            w.set_pixel(x as usize, y as usize, if edge { ACCENT } else { CARD });
        }
    }
    // Centre the label on the button's middle. `baseline` is the text's BOTTOM,
    // so the optical centre of a line of height `fs` is half a line above the
    // middle — without that offset the label sits visibly low in the button.
    draw_text_centered(
        w,
        x0 + bw / 2,
        y0 + bh / 2 + g.fs_btn as i64 / 3,
        "Sign in",
        g.fs_btn,
        TEXT,
    );
}

/// The corner power button: the power symbol, with a translucent disc behind it
/// so the glyph stays legible over any part of the gradient.
fn draw_power_button(w: &mut FrameBufferWriter, pw: usize, ph: usize) {
    let g = power_geom(pw, ph);
    let cx = (g.btn_x + g.btn / 2) as i64;
    let cy = (g.btn_y + g.btn / 2) as i64;
    fill_circle_a(w, cx, cy, (g.btn / 2) as i64, CARD, 0xCC);
    draw_glyph_power(w, cx, cy, (g.btn as i64 * 2) / 5, ACCENT);
}

/// The power menu panel, drawn only while `MENU_OPEN`.
fn draw_power_menu(w: &mut FrameBufferWriter, pw: usize, ph: usize, fs: u16) {
    let g = power_geom(pw, ph);
    let sel = selected();
    // Text column and glyph radius both scale with the row height, so the label
    // always has the same proportion of its row regardless of the UI scale.
    let glyph_x = g.menu_x as i64 + g.item_h as i64 * 3 / 4;
    let text_x = g.menu_x as i64 + g.item_h as i64 * 3 / 2;
    let glyph_r = (g.item_h as i64 * 3) / 10;

    // Panel. Drawn before the rows so a row highlight can never be painted over
    // by the panel fill.
    for dy in 0..g.menu_h {
        let y = g.menu_y + dy;
        if y >= ph {
            break;
        }
        for dx in 0..g.menu_w {
            let x = g.menu_x + dx;
            if x >= pw {
                break;
            }
            let edge = dx == 0 || dx == g.menu_w - 1 || dy == 0 || dy == g.menu_h - 1;
            w.set_pixel(x, y, if edge { CARD_EDGE } else { CARD });
        }
    }

    // Rows: highlight, glyph, then label.
    for (i, label) in POWER_ITEMS.iter().enumerate() {
        let row_y = g.menu_y + g.pad + i * g.item_h;
        let cy = row_y + g.item_h / 2;
        let fg = if i == sel { ACCENT } else { TEXT };
        if i == sel {
            // Inset from the panel border so the panel's own edge stays visible.
            for dy in 1..(g.item_h - 1) {
                let y = row_y + dy;
                if y >= ph {
                    break;
                }
                for dx in 3..(g.menu_w - 3) {
                    let x = g.menu_x + dx;
                    if x >= pw {
                        break;
                    }
                    w.set_pixel(x, y, CARD_EDGE);
                }
            }
        }
        draw_power_item_glyph(w, i, glyph_x, cy as i64, glyph_r, fg);
        // Baseline a third of the line above the row's middle, for the same
        // optical-centring reason as the Sign in button.
        draw_text(w, text_x, cy as i64 + fs as i64 / 3, label, fs, fg);
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Draw the whole sign-in screen: backdrop, avatar, name, "Sign in" button, the
/// power button, and the power menu when it is open.
///
/// ONE FB acquisition for the entire frame and ONE damage mark after it. Drawing
/// the screen in separate locked chunks would let the mouse IRQ interleave a
/// cursor move between the backdrop and the avatar, which presents as a torn
/// first frame — and would mark the whole surface dirty several times per repaint.
pub fn draw() {
    let _ = crate::framebuffer::with_writer(|w, width, height| {
        draw_backdrop(w, width, height);
        // Once signed in the desktop owns the screen; painting over it would
        // fight the compositor for the same pixels forever.
        if phase() == PHASE_DESKTOP {
            return;
        }
        let g = signin_geom(width, height);
        draw_avatar(w, &g);
        draw_text_centered(w, g.avatar_cx, g.name_baseline, USER_NAME, g.fs_name, TEXT);
        draw_signin_button(w, &g);
        draw_power_button(w, width, height);
        if menu_open() {
            draw_power_menu(w, width, height, g.fs_item);
        }
    });
}

/// Repaint ONLY the power widget: the menu panel and the corner button.
///
/// P6c: this is what a click must use. Toggling the menu changes a rectangle
/// roughly 400x150 in the corner, and repainting the whole 1920x1080 surface for
/// it was what produced the top-to-bottom wipe — the dispi backend scans out the
/// very memory being written, so a full repaint is visible as it happens.
///
/// The region is the UNION of the menu panel and the button, taken whether or not
/// the menu is currently open, so opening it paints the panel and closing it
/// restores the backdrop over exactly the same pixels (otherwise closing would
/// leave the panel's ghost behind).
pub fn draw_power() {
    let (w, h, _) = crate::framebuffer::current_geometry();
    if w == 0 || h == 0 {
        return;
    }
    let pg = power_geom(w, h);
    let x0 = pg.menu_x.min(pg.btn_x);
    let y0 = pg.menu_y.min(pg.btn_y);
    let x1 = (pg.menu_x + pg.menu_w).max(pg.btn_x + pg.btn);
    let y1 = (pg.menu_y + pg.menu_h).max(pg.btn_y + pg.btn);
    let fs = signin_geom(w, h).fs_item;
    let _ = crate::framebuffer::with_writer_region(
        |w, pw, ph| {
            // Repaint the backdrop for this rect only — the same gradient the
            // full-screen pass would have produced, so nothing seams.
            let ry1 = y1.min(ph.saturating_sub(1));
            let rx1 = x1.min(pw.saturating_sub(1));
            for y in y0..=ry1 {
                let c = bg_row(y, ph);
                for x in x0..=rx1 {
                    w.set_pixel(x, y, c);
                }
            }
            draw_power_button(w, pw, ph);
            if menu_open() {
                draw_power_menu(w, pw, ph, fs);
            }
        },
        Some((x0, y0, x1, y1)),
    );
}

/// Apply a click: toggle the menu, run a power action, or sign in.
///
/// This MUTATES state and can reboot the machine, so it is kept separate from
/// [`classify_click`]: the self-test exercises the hit-testing through the pure
/// function and therefore never lands here.
pub fn on_click(x: usize, y: usize) {
    let (w, h, _) = crate::framebuffer::current_geometry();
    if w == 0 || h == 0 {
        return;
    }
    match classify_click(w, h, x, y) {
        Click::PowerButton => {
            MENU_OPEN.store(!menu_open(), Ordering::Release);
            // Region repaint, not the full screen — see `draw_power`.
            draw_power();
        }
        Click::MenuItem(idx) => {
            SEL.store(idx as u8, Ordering::Release);
            // Close the menu and repaint BEFORE the action, so a power-off that
            // never returns still leaves a correct final frame behind.
            MENU_OPEN.store(false, Ordering::Release);
            draw_power();
            crate::serial_writeln!("[signin] power action: {}", POWER_ITEMS[idx]);
            perform(POWER_ITEMS[idx]);
        }
        Click::SignIn => {
            SIGNED_IN.store(true, Ordering::Release);
            PHASE.store(PHASE_DESKTOP, Ordering::Release);
            crate::serial_writeln!("[signin] signed in as {USER_NAME} - entering desktop");
            // The desktop replaces the whole screen, so this IS a full repaint —
            // the one place the expensive path is correct.
            crate::desktop::draw();
        }
        Click::Outside => {
            // Click-away dismisses the menu, but only redraw if one was open.
            if menu_open() {
                MENU_OPEN.store(false, Ordering::Release);
                draw_power();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Power actions
// ---------------------------------------------------------------------------

/// Run a power action by name.
///
/// `Sleep` is not implemented yet and says so rather than pretending: a menu
/// item that silently does nothing is worse than one that reports it is stubbed.
fn perform(action: &str) {
    match action {
        "Shut down" => crate::power::shutdown(),
        "Restart" => crate::power::restart(),
        "Sleep" => crate::serial_writeln!("[signin] Sleep is not implemented yet"),
        other => crate::serial_writeln!("[signin] unknown power action: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Click queue — the IRQ/task split
// ---------------------------------------------------------------------------

/// A click waiting to be processed: `(x, y)` plus a "one is pending" flag.
///
/// A single slot, not a ring. The producer is the mouse IRQ and the consumer is
/// a task, so at most a couple of clicks can be in flight, and the ONLY thing a
/// user can do to the sign-in screen is click. A full ring here would add
/// ordering and overflow rules for a case that cannot arise; what the slot does
/// need is to never LOSE a click silently, so `queue_click` overwrites the
/// pending coordinate (the newest click is the one the user meant) and
/// `take_pending` clears the flag with the value it read.
static CLICK_X: AtomicUsize = AtomicUsize::new(0);
static CLICK_Y: AtomicUsize = AtomicUsize::new(0);
static CLICK_PENDING: AtomicBool = AtomicBool::new(false);

/// Record a click from IRQ context. Allocation-free and lock-free.
///
/// The mouse IRQ can fire while a task holds the framebuffer lock, so this must
/// never touch the FB — it only stores three atomics. The repaint happens later,
/// in [`run_pending`].
pub fn queue_click(x: usize, y: usize) {
    CLICK_X.store(x, Ordering::Relaxed);
    CLICK_Y.store(y, Ordering::Relaxed);
    // Release: the coordinate stores must be visible before the flag that
    // publishes them, or the consumer could read a stale coordinate.
    CLICK_PENDING.store(true, Ordering::Release);
}

/// Take the pending click, if any. Called from task context.
fn take_pending() -> Option<(usize, usize)> {
    // Acquire pairs with the `Release` in `queue_click`.
    if !CLICK_PENDING.swap(false, Ordering::Acquire) {
        return None;
    }
    Some((
        CLICK_X.load(Ordering::Relaxed),
        CLICK_Y.load(Ordering::Relaxed),
    ))
}

/// Drain queued clicks and act on them, at most one per call.
///
/// This is the ONLY place a click turns into a redraw, and it runs in task
/// context so `on_click` may safely take the framebuffer lock and repaint.
pub fn run_pending() {
    if let Some((x, y)) = take_pending() {
        // Which screen gets the click is decided by the phase, so the two never
        // have to both listen: the sign-in screen ignores desktop clicks, and
        // the Text Lab ignores sign-in clicks.
        if phase() == PHASE_DESKTOP {
            crate::desktop::on_click(x, y);
        } else {
            on_click(x, y);
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------

/// P6c self-test: geometry, hit-testing, and that `draw` actually paints.
///
/// Every check asserts on real geometry or real pixels, so deleting a
/// hit-test, breaking the layout maths, or emptying the draw body fails it —
/// the "the task started" assertion this codebase keeps rejecting.
///
/// The power ACTIONS are deliberately never executed: that would power off the
/// VM the test runs in. They are covered by [`classify_click`] resolving each
/// row to its OWN index, which is the part that can actually be wrong: a bug
/// returning row 0 for every y would make "Shut down" reboot the machine, and
/// the self-test would otherwise pass while the button did the wrong thing.
pub fn signin_test() {
    // STAGGERED start, cumulative with the other P6 self-tests (surface ->
    // blit -> font -> compositor -> signin). Without this the screen's work
    // competes with them for the single CPU and delays every later test; with
    // it, each test runs in the gap after the previous one has finished.
    crate::scheduler::sleep_kernel(6000);
    let (w, h, _) = crate::framebuffer::current_geometry();
    if w == 0 || h == 0 {
        crate::serial_writeln!("[signin] FAILED: no framebuffer writer installed");
        return;
    }
    crate::serial_writeln!("[signin] P6c self-test at {w}x{h}");

    let (mut pass, mut bad) = (0usize, 0usize);
    // A closure so every check reports the same way and the counters cannot drift.
    let mut check = |name: &str, ok: bool| {
        if ok {
            pass += 1;
            crate::serial_writeln!("[signin] PASS {name}");
        } else {
            bad += 1;
            crate::serial_writeln!("[signin] BAD  {name}");
        }
    };

    let g = signin_geom(w, h);
    let pg = power_geom(w, h);

    // --- geometry is on screen at the CURRENT mode ---
    check(
        "avatar on screen",
        g.avatar_cy - g.avatar_r >= 0 && g.avatar_cy + g.avatar_r < h as i64,
    );
    check(
        "sign-in button on screen",
        g.btn_x >= 0
            && g.btn_y >= 0
            && g.btn_x + g.btn_w < w as i64
            && g.btn_y + g.btn_h < h as i64,
    );
    // Bound the casted values first: `pg.menu_y as i64 + ...` parses ambiguously
    // (`<` after `as i64` is read as generic arguments).
    let menu_right = pg.menu_x as i64 + pg.menu_w as i64;
    let menu_bottom = pg.menu_y as i64 + pg.menu_h as i64;
    check(
        "power menu on screen",
        pg.menu_y as i64 >= 0 && pg.menu_x as i64 >= 0 && menu_bottom < h as i64 && menu_right < w as i64,
    );
    // The two hit regions must not overlap, or one silently wins every click.
    check(
        "power button clear of sign-in button",
        pg.btn_x as i64 > g.btn_x + g.btn_w || pg.btn_y as i64 > g.btn_y + g.btn_h,
    );

    // --- the power button ---
    let bcx = pg.btn_x + pg.btn / 2;
    let bcy = pg.btn_y + pg.btn / 2;
    check("power button hit at its centre", hit_power(w, h, bcx, bcy));
    check("power button miss at origin", !hit_power(w, h, 4, 4));
    check(
        "power button classifies as PowerButton",
        classify_click(w, h, bcx, bcy) == Click::PowerButton,
    );

    // --- the menu only takes clicks while it is OPEN ---
    let rx = pg.menu_x + pg.menu_w / 2;
    MENU_OPEN.store(false, Ordering::Release);
    check(
        "closed menu rows are not clickable",
        classify_click(w, h, rx, pg.menu_y + pg.pad + 2) == Click::Outside,
    );

    // --- every row resolves to its OWN index ---
    MENU_OPEN.store(true, Ordering::Release);
    let mut rows_ok = true;
    for (i, label) in POWER_ITEMS.iter().enumerate() {
        let ry = pg.menu_y + pg.pad + i * pg.item_h + pg.item_h / 2;
        if classify_click(w, h, rx, ry) != Click::MenuItem(i) {
            rows_ok = false;
            crate::serial_writeln!("[signin]   row {i} ({label}) mis-hit");
        }
    }
    check("every menu row hit-tests to its own index", rows_ok);
    // The panel's padding must not resolve to a row, or the first and last rows
    // have hit areas larger than they look.
    check(
        "menu top padding is not a row",
        classify_click(w, h, rx, pg.menu_y + 1) == Click::Outside,
    );
    check(
        "menu bottom padding is not a row",
        classify_click(w, h, rx, pg.menu_y + pg.menu_h - 2) == Click::Outside,
    );
    MENU_OPEN.store(false, Ordering::Release);

    // --- the Sign in button ---
    let scx = (g.btn_x + g.btn_w / 2) as usize;
    let scy = (g.btn_y + g.btn_h / 2) as usize;
    check("sign-in button hit at its centre", hit_signin(w, h, scx, scy));
    check(
        "sign-in button classifies as SignIn",
        classify_click(w, h, scx, scy) == Click::SignIn,
    );

    // --- draw() actually paints ---
    //
    // Read pixels back rather than trusting the call returned. The avatar disc
    // centre must differ from the raw gradient and a button interior must be the
    // card fill — both fail if the draw body is emptied.
    draw();
    // `with_writer` returns `Option` (no writer installed?) around the closure's
    // own `Option` (pixel off-screen?), so these flatten the two.
    let avatar_px = crate::framebuffer::with_writer(|w, _, _| {
        w.get_pixel(g.avatar_cx as usize, g.avatar_cy as usize)
    })
    .flatten();
    check(
        "avatar disc painted over the gradient",
        avatar_px
            .map(|p| p != bg_row(g.avatar_cy as usize, h))
            .unwrap_or(false),
    );
    let btn_px = crate::framebuffer::with_writer(|w, _, _| {
        // Probe the button's TOP-LEFT interior, not its centre: the "Sign in"
        // label is centred on the button, so the centre pixel is glyph ink and
        // asserting on it tested the text, not the fill. This point is inside
        // the 2 px border and clear of the label's run.
        w.get_pixel(
            (g.btn_x + 8).max(0) as usize,
            (g.btn_y + 4).max(0) as usize,
        )
    })
    .flatten();
    check("sign-in button painted", btn_px.map(|p| p == CARD).unwrap_or(false));

    // The power glyph must put accent ink on screen.
    let glyph_px = crate::framebuffer::with_writer(|w, _, _| {
        let mut found = false;
        for y in pg.btn_y..(pg.btn_y + pg.btn) {
            for x in pg.btn_x..(pg.btn_x + pg.btn) {
                if w.get_pixel(x, y) == Some(ACCENT) {
                    found = true;
                }
            }
        }
        found
    })
    .unwrap_or(false);
    check("power button glyph painted", glyph_px);

    // Opening the menu must actually change the screen. Uses the SAME region repaint
    // a click uses, so this check covers the cheap path rather than only the
    // full-screen one.
    MENU_OPEN.store(true, Ordering::Release);
    draw_power();
    let menu_px = crate::framebuffer::with_writer(|w, _, _| {
        w.get_pixel(
            pg.menu_x + pg.menu_w / 2,
            pg.menu_y + pg.pad + pg.item_h / 2,
        )
    })
    .flatten();
    check(
        "power menu panel painted",
        menu_px
            .map(|p| p != bg_row(pg.menu_y + pg.pad, h))
            .unwrap_or(false),
    );
    MENU_OPEN.store(false, Ordering::Release);
    // Back to the full screen so the boot screen is left in its normal state.
    draw();

    crate::serial_writeln!("[signin] done: {pass} passed, {bad} bad");
}

/// Draw the screen once, then service queued clicks.
///
/// The screen stays interactive because this task drains the click queue the
/// mouse IRQ fills ([`run_pending`]) and repaints on demand — the IRQ itself only
/// records coordinates, because a full-screen repaint in IRQ context would hold
/// the framebuffer lock across millions of pixel writes with interrupts off.
///
/// The poll sleeps rather than spins: a click redraws within one 20 ms tick,
/// which is imperceptible next to the present path's own sub-millisecond
/// latency, and spinning would make this an always-runnable IDLE task — the
/// exact starvation shape `ticker_task` documents.
pub fn signin_task() {
    // Wait for BOTH self-tests to finish before taking the screen: they assert
    // on pixels, and two tasks repainting the same framebuffer concurrently would
    // make those assertions read another task's output. Cumulative with the
    // staggered starts, so this lands after both of theirs.
    crate::scheduler::sleep_kernel(11000);
    crate::serial_writeln!("[signin] drawing sign-in screen");
    draw();
    loop {
        run_pending();
        crate::scheduler::sleep_kernel(20);
    }
}