//! P6a stage 1 — **surfaces**: first-class offscreen pixel buffers.
//!
//! A surface is the drawing substrate everything else in the desktop is built
//! on. The design deliberately reuses machinery already proven in this kernel
//! rather than inventing a parallel concept:
//!
//!   * Backing memory comes from `memory::alloc_contiguous` — the same
//!     physical allocator the virtio-gpu GEM-lite resource uses — so a surface
//!     is a real DMA-capable buffer, not a `Vec` only the CPU can reach. That is
//!     what lets a surface become a scanout later with no second notion of
//!     "a thing you can draw on".
//!   * It is reachable through `memory::phys_offset`, the same physical window
//!     the GPU driver writes through.
//!
//! ## Why not just use `Vec<u32>`
//!
//! It would be simpler, and for stage 1 it would work. It is rejected because
//! the desktop exists to be blitted to the device, and the moment a surface is
//! handed to virtio it must be physically contiguous and device-addressable. A
//! heap `Vec` is neither. Choosing the right substrate now is cheaper than
//! migrating every window later.
//!
//! ## Ownership and safety
//!
//! A `SurfaceId` is a plain `u32` index into a fixed-size table, NOT a pointer
//! and NOT a reference count. Handles are validated on every entry point, so a
//! stale or forged id returns `Err` rather than corrupting memory. The table is
//! `static mut` guarded by one mutex, matching the established `FB`/`CONSOLE`
//! pattern already used in `framebuffer.rs`.

use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

/// Pixel format. Only the two the present path already speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// 32-bit host order (matches the console's `0xRRGGBB` writes).
    Rgb8888,
    /// virtio-gpu `B8G8R8X8_UNORM`: byte order B, G, R, X.
    B8G8R8X8,
}

impl Format {
    #[inline]
    pub fn bytes_per_pixel(self) -> usize {
        4
    }
}

/// A handle to a surface. Validated against the table on every use; 0 is never
/// a valid id, so 0 doubles as "none".
pub type SurfaceId = u32;

pub const INVALID: SurfaceId = 0;

/// Maximum simultaneous surfaces. A desktop needs very few (screen,
/// compositor scratch, maybe one window buffer); a small fixed table keeps
/// allocation trivial and makes exhaustion reportable rather than an OOM.
pub const MAX_SURFACES: usize = 8;

/// A live surface: its physical backing plus the geometry needed to interpret
/// the bytes.
pub struct Surface {
    pub id: SurfaceId,
    pub width: usize,
    pub height: usize,
    pub format: Format,
    /// Physical base address of the contiguous backing.
    pub phys: u64,
    pub len: usize,
    /// Union of rectangles touched since the last `take_damage`.
    pub damage: Option<(usize, usize, usize, usize)>,
}

impl Surface {
    /// Virtual address of the backing, through the bootloader's physical window.
    ///
    /// # Safety
    /// The returned pointer is valid for `len` bytes and refers to memory this
    /// surface owns exclusively. Callers must not alias it across a lock.
    pub unsafe fn va(&self) -> Option<*mut u8> {
        crate::memory::phys_offset().map(|off| (off + self.phys) as *mut u8)
    }
}

/// The table, indexed by SURFACE ID (not by slot position).
///
/// The id is allocated by `NEXT_ID` and IS the index. An earlier version
/// returned a monotonically increasing id but then looked surfaces up by that
/// id as if it were the slot index - which works only while the two happen to
/// agree, and silently fails from the first id upward. The self-test caught it
/// immediately (`geometry` reported "not live" for a surface `create` had just
/// returned), which is exactly what the self-test exists for.
///
/// Ids are never reused, so `TABLE[id]` is unambiguous even after `destroy`:
/// a retired slot stays `None` forever and the id is simply never handed out
/// again. `MAX_SURFACES` therefore bounds the number of surfaces ever created,
/// not the number live - acceptable at this scale, and stated rather than
/// hidden.
static mut TABLE: [Option<Surface>; MAX_SURFACES] = [const { None }; MAX_SURFACES];
static LOCK: Mutex<()> = Mutex::new(());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Diagnostics, reported by [`stats`].
static LIVE: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

/// Grow `damage` to include `(x, y, x + w, y + h)` (exclusive max), clamped to
/// the surface. Accumulates without the caller having to track it.
fn union_damage(s: &mut Surface, x: usize, y: usize, w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    let x0 = x.min(s.width);
    let y0 = y.min(s.height);
    let x1 = (x + w).min(s.width);
    let y1 = (y + h).min(s.height);
    if x0 >= x1 || y0 >= y1 {
        return; // entirely clipped away
    }
    s.damage = Some(match s.damage {
        None => (x0, y0, x1, y1),
        Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
    });
}

/// Create a surface. `w`/`h` are in pixels; the backing is zeroed.
pub fn create(w: usize, h: usize, format: Format) -> Result<SurfaceId, &'static str> {
    if w == 0 || h == 0 {
        FAILED.fetch_add(1, Ordering::Relaxed);
        return Err("surface: zero-sized");
    }
    // Refuse an absurd request rather than attempting the allocation.
    let bytes = match w.checked_mul(h).and_then(|p| p.checked_mul(4)) {
        Some(b) => b,
        None => {
            FAILED.fetch_add(1, Ordering::Relaxed);
            return Err("surface: size overflow");
        }
    };
    if bytes > 64 * 1024 * 1024 {
        FAILED.fetch_add(1, Ordering::Relaxed);
        return Err("surface: too large (>64 MiB)");
    }
    let _g = LOCK.lock();
    unsafe {
        // The id IS the table index (see the TABLE doc comment), so allocate it
        // first and use it directly. Bounded by MAX_SURFACES; ids are never
        // reused, so exhaustion is permanent for this boot and is reported as
        // such rather than silently aliasing a live surface.
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if id == 0 || id as usize >= MAX_SURFACES {
            FAILED.fetch_add(1, Ordering::Relaxed);
            return Err("surface: table full");
        }
        let frames = ((bytes as u64) + 4095) / 4096;
        let phys = match crate::memory::alloc_contiguous(frames) {
            Some(p) => p,
            None => {
                FAILED.fetch_add(1, Ordering::Relaxed);
                return Err("surface: no contiguous physical frames");
            }
        };
        // Zero the backing: an uninitialised surface would leak whatever was in
        // those frames and would make any later visual bug ambiguous.
        if let Some(va) = crate::memory::phys_offset().map(|off| off + phys) {
            core::ptr::write_bytes(va as *mut u8, 0, bytes);
        }
        TABLE[id as usize] = Some(Surface {
            id,
            width: w,
            height: h,
            format,
            phys,
            len: bytes,
            damage: None,
        });
        LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }
}

/// Destroy a surface. Its frames are NOT returned to the allocator (the bump
/// allocator has no free path), so the slot is retired for reuse.
pub fn destroy(id: SurfaceId) -> Result<(), &'static str> {
    let _g = LOCK.lock();
    unsafe {
        let slot = TABLE.get_mut(id as usize).ok_or("surface: id out of range")?;
        if slot.is_none() {
            return Err("surface: not live");
        }
        *slot = None;
        LIVE.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Geometry of a live surface, without exposing its pointer.
pub fn geometry(id: SurfaceId) -> Result<(usize, usize, Format), &'static str> {
    let _g = LOCK.lock();
    unsafe {
        TABLE
            .get(id as usize)
            .and_then(|s| s.as_ref())
            .map(|s| (s.width, s.height, s.format))
            .ok_or("surface: not live")
    }
}

/// Read one pixel as `0xRRGGBB`, translating from the surface's format.
pub fn pixel(id: SurfaceId, x: usize, y: usize) -> Result<u32, &'static str> {
    let _g = LOCK.lock();
    unsafe {
        let s = TABLE
            .get(id as usize)
            .and_then(|s| s.as_ref())
            .ok_or("surface: not live")?;
        if x >= s.width || y >= s.height {
            return Err("surface: pixel out of bounds");
        }
        let va = s.va().ok_or("surface: no physical window")?;
        let raw = core::ptr::read_volatile((va as *const u32).add(y * s.width + x));
        Ok(match s.format {
            Format::Rgb8888 => raw & 0x00FF_FFFF,
            Format::B8G8R8X8 => (raw & 0xFF) << 16 | (raw & 0xFF00) | (raw >> 16),
        })
    }
}

/// Write one pixel as `0xRRGGBB`, translating into the surface's format, and
/// union that single pixel into the damage rect.
///
/// Stage 1 exposes this deliberately: a surface you cannot write to is not a
/// drawing substrate, and stage 2's `fill`/`blit` are built on exactly this
/// write. Damage is accumulated HERE, at the single chokepoint, so no future
/// draw path can forget to mark itself - the same discipline `set_pixel` uses
/// in `framebuffer.rs`.
pub fn put_pixel(id: SurfaceId, x: usize, y: usize, rgb: u32) -> Result<(), &'static str> {
    let _g = LOCK.lock();
    unsafe {
        let s = TABLE
            .get_mut(id as usize)
            .and_then(|s| s.as_mut())
            .ok_or("surface: not live")?;
        if x >= s.width || y >= s.height {
            return Err("surface: pixel out of bounds");
        }
        let va = s.va().ok_or("surface: no physical window")?;
        let raw = match s.format {
            Format::Rgb8888 => rgb & 0x00FF_FFFF,
            Format::B8G8R8X8 => (rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16),
        };
        core::ptr::write_volatile((va as *mut u32).add(y * s.width + x), raw);
        union_damage(s, x, y, 1, 1);
        Ok(())
    }
}

/// Fill a rectangle with a solid `0xRRGGBB` colour, clipped to the surface, and
/// union the written rect into the surface's damage.
///
/// Clipping is silent and non-fatal: a rectangle that hangs off the edge draws
/// the visible part, exactly as a GPU blit would. Rejecting the whole call would
/// make every caller do its own bounds arithmetic, and every caller would get it
/// wrong once. Returns the rect actually written so tests (and a future
/// compositor doing dirty-rect maths) can check it.
///
/// A zero-area rect writes nothing and records no damage - so a
/// `blit` of a fully-clipped window cannot manufacture a phantom present.
pub fn fill(id: SurfaceId, x: usize, y: usize, w: usize, h: usize, rgb: u32) -> Result<(usize, usize, usize, usize), &'static str> {
    if w == 0 || h == 0 {
        return Err("surface: zero-area fill");
    }
    let _g = LOCK.lock();
    unsafe {
        let s = TABLE
            .get_mut(id as usize)
            .and_then(|s| s.as_mut())
            .ok_or("surface: not live")?;
        let x0 = x.min(s.width);
        let y0 = y.min(s.height);
        let x1 = (x + w).min(s.width);
        let y1 = (y + h).min(s.height);
        if x0 >= x1 || y0 >= y1 {
            return Err("surface: fill entirely outside the surface");
        }
        let raw = match s.format {
            Format::Rgb8888 => rgb & 0x00FF_FFFF,
            Format::B8G8R8X8 => (rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16),
        };
        let va = s.va().ok_or("surface: no physical window")?;
        // Row-at-a-time rather than per-pixel: the inner loop becomes a `u32`
        // store, which is what makes a full-screen clear cheap enough to do
        // every frame. A per-pixel bounds check here would be the single
        // hottest loop in the whole compositor.
        for row in y0..y1 {
            let p = (va as *mut u32).add(row * s.width + x0);
            for i in 0..(x1 - x0) {
                core::ptr::write_volatile(p.add(i), raw);
            }
        }
        union_damage(s, x0, y0, x1 - x0, y1 - y0);
        Ok((x0, y0, x1 - x0, y1 - y0))
    }
}

/// Copy a rectangle from one surface to another, with optional clipping on both
/// sides, and union the written rect into the DESTINATION's damage only.
///
/// Damage is recorded on the destination and NOT the source: the source's own
/// pixels did not change, so marking it would schedule a pointless re-present of
/// a window that was merely read. That distinction is the difference between a
/// compositor that costs one present per frame and one that costs two.
///
/// The copy always goes through `0xRRGGBB`, converting on the way in and out,
/// so blitting between different formats is correct by construction rather than
/// by a special case. That costs two shifts per pixel; stage 3 replaces it with
/// a same-format fast path once the profiler says it matters.
pub fn blit(
    dst: SurfaceId,
    src: SurfaceId,
    dx: i32,
    dy: i32,
    sx: usize,
    sy: usize,
    w: usize,
    h: usize,
) -> Result<(usize, usize, usize, usize), &'static str> {
    if w == 0 || h == 0 {
        return Err("surface: zero-area blit");
    }
    let _g = LOCK.lock();
    unsafe {
        let Some(d) = TABLE.get_mut(dst as usize).and_then(|s| s.as_mut()) else {
            return Err("surface: destination not live");
        };
        let Some(s) = TABLE.get(src as usize).and_then(|s| s.as_ref()) else {
            return Err("surface: source not live");
        };
        // Reject anything the SOURCE cannot supply before touching the
        // destination. A partial read would otherwise read past the source's
        // backing - the one memory-safety hole this function could have.
        if sx >= s.width || sy >= s.height {
            return Err("surface: blit source rect outside the source");
        }
        let rw = w.min(s.width - sx);
        let rh = h.min(s.height - sy);

        // Clip against the destination, translating the source origin by the
        // same amount so the two stay in step.
        let dx0 = (dx.max(0) as usize).min(d.width);
        let dy0 = (dy.max(0) as usize).min(d.height);
        let dx1 = ((dx.max(0) as usize).saturating_add(rw)).min(d.width);
        let dy1 = ((dy.max(0) as usize).saturating_add(rh)).min(d.height);
        if dx0 >= dx1 || dy0 >= dy1 {
            return Err("surface: blit entirely clipped off the destination");
        }
        // Skip the clipped-off top/left margin.
        let skip_x = dx0 - (dx.max(0) as usize);
        let skip_y = dy0 - (dy.max(0) as usize);
        let span_x = dx1 - dx0;
        let span_y = dy1 - dy0;

        let sva = s.va().ok_or("surface: source has no physical window")?;
        let dva = d.va().ok_or("surface: destination has no physical window")?;
        for row in 0..span_y {
            let srow = sy + skip_y + row;
            let drow = dy0 + row;
            let sp = (sva as *const u32).add(srow * s.width + sx + skip_x);
            let dp = (dva as *mut u32).add(drow * d.width + dx0);
            for col in 0..span_x {
                let raw = core::ptr::read_volatile(sp.add(col));
                let rgb = match s.format {
                    Format::Rgb8888 => raw & 0x00FF_FFFF,
                    Format::B8G8R8X8 => (raw & 0xFF) << 16 | (raw & 0xFF00) | (raw >> 16),
                };
                let out = match d.format {
                    Format::Rgb8888 => rgb,
                    Format::B8G8R8X8 => (rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16),
                };
                core::ptr::write_volatile(dp.add(col), out);
            }
        }
        union_damage(d, dx0, dy0, span_x, span_y);
        Ok((dx0, dy0, span_x, span_y))
    }
}

/// Alpha-blend an 8-bit coverage mask onto a surface as `rgb`, using
/// source-over compositing, and union the written rect into the surface damage.
///
/// This is the bridge that makes `font::Glyph` drawable. The font module
/// deliberately produces a bare alpha mask with no colour, so the SAME mask
/// composites over a light or dark background without a second rasterization -
/// which is exactly what a desktop needs (dark title bars, light document
/// areas, one text path).
///
/// The blend is `dst = src*a + dst*(1-a)` with `a` in 0..=255, computed in u32
/// and shifted back. Doing it in u8 would truncate the low bits of the product
/// and visibly darken every antialiased edge - the "text looks slightly grey
/// and thin" symptom.
///
/// Fully transparent pixels (a == 0) are SKIPPED rather than blended: blending
/// zero would write the destination back unchanged but still cost a read-modify-
/// write and, more importantly, would mark damage for a region that did not
/// change. For a run of whitespace that is the difference between "no damage at
/// all" and "a present every time you type a space".
///
/// Returns the rect actually written, or `None` if the mask was entirely
/// transparent and therefore nothing was drawn.
pub fn blend_mask(
    id: SurfaceId,
    x: usize,
    y: usize,
    mask: &[u8],
    mw: usize,
    mh: usize,
    rgb: u32,
) -> Result<Option<(usize, usize, usize, usize)>, &'static str> {
    if mw == 0 || mh == 0 || mask.len() != mw * mh {
        return Err("surface: bad mask geometry");
    }
    // A mask with no ink at all must not create damage (see the doc comment).
    if !mask.iter().any(|&a| a != 0) {
        return Ok(None);
    }
    let _g = LOCK.lock();
    unsafe {
        let s = TABLE
            .get_mut(id as usize)
            .and_then(|s| s.as_mut())
            .ok_or("surface: not live")?;
        // Clip the mask rect to the surface BEFORE indexing either buffer.
        let x0 = x.min(s.width);
        let y0 = y.min(s.height);
        let x1 = (x + mw).min(s.width);
        let y1 = (y + mh).min(s.height);
        if x0 >= x1 || y0 >= y1 {
            return Ok(None);
        }
        let va = s.va().ok_or("surface: no physical window")?;
        let src = pack(rgb, s.format);

        let mut touched = false;
        for row in y0..y1 {
            let my = row - y0; // row within the mask
            for col in x0..x1 {
                let a = mask[my * mw + (col - x0)];
                if a == 0 {
                    continue; // fully transparent: leave the destination alone
                }
                let p = (va as *mut u32).add(row * s.width + col);
                let dst = core::ptr::read_volatile(p);
                // Source-over in u32: `(src*a + dst*(255-a) + 127) / 255`, with
                // the +127 rounding so a 50% blend does not always land low.
                let blended = (src as u32 * a as u32 + dst as u32 * (255 - a) as u32 + 127) / 255;
                core::ptr::write_volatile(p, blended);
                touched = true;
            }
        }
        if !touched {
            return Ok(None);
        }
        let w = x1 - x0;
        let h = y1 - y0;
        union_damage(s, x0, y0, w, h);
        Ok(Some((x0, y0, w, h)))
    }
}

/// Pack a `0xRRGGBB` colour into the surface's native word layout.
#[inline]
fn pack(rgb: u32, f: Format) -> u32 {
    match f {
        Format::Rgb8888 => rgb & 0x00FF_FFFF,
        Format::B8G8R8X8 => (rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16),
    }
}

/// Ascent in pixels for a given font size, so a caller can place a baseline
/// without depending on `font` directly. Returns 0 if the face cannot be parsed,
/// which the caller should treat as "do not draw text" rather than guess.
pub fn line_ascent(size: u16) -> i32 {
    match crate::font::Face::load() {
        Ok(f) => f.line_metrics(size).ascent,
        Err(_) => 0,
    }
}

/// A glyph bitmap cache keyed by `(char, size)`.
///
/// P6b: the compositor draws a title per window per frame. Without a cache each
/// of those re-parses the face AND re-rasterizes the same glyph, and
/// `rasterize` allocates a `Vec` per outline - so a frame that draws 3 titles
/// allocated ~10 MiB of short-lived vectors and ran the heap allocator out of
/// contiguous space. Under TCG the bump allocator's failure path is a spin, so
/// that presented as the compositor HANGING rather than as an allocation error.
///
/// Caching by (char, size) makes a repeated frame allocation-free in the steady
/// state, which is what a compositor actually needs: the same few strings are
/// redrawn every frame, forever.
static GLYPH_CACHE: spin::Mutex<Option<alloc::collections::BTreeMap<(u32, u16), crate::font::Glyph>>> =
    spin::Mutex::new(None);

/// Maximum cached glyphs. A UI has a small working set (a dozen labels, two or
/// three sizes); a hard cap keeps a pathological caller - text with thousands
/// of distinct characters, at many sizes - from growing the cache without
/// limit. When full, the cache is simply cleared and rebuilt, which is cheap
/// and bounded.
const GLYPH_CACHE_MAX: usize = 256;

/// A cached, rasterized glyph. The alpha mask is COPIED out of the font's
/// per-call vector so the cache owns it; the font's copy is dropped immediately.
fn cached_glyph(ch: char, size: u16) -> Option<crate::font::Glyph> {
    let key = (ch as u32, size);
    let mut guard = GLYPH_CACHE.lock();
    if guard.is_none() {
        *guard = Some(alloc::collections::BTreeMap::new());
    }
    let map = guard.as_mut()?;
    if let Some(g) = map.get(&key) {
        return Some(g.clone());
    }
    // Render outside the borrow of the existing entries.
    let face = crate::font::Face::load().ok()?;
    let gid = face.glyph_for_char(ch);
    let g = crate::font::rasterize(&face, gid, size);
    if map.len() >= GLYPH_CACHE_MAX {
        map.clear();
    }
    map.insert(key, g.clone());
    Some(g)
}

/// Draw one line of text onto a surface, compositing the glyph alpha masks.
///
/// This is the first consumer of BOTH the font and the surface modules, and it
/// deliberately lives here rather than in a separate text widget: the compositor
/// needs to label windows, the clock and the taskbar, and all of them want the
/// same primitive. `y` is the BASELINE, matching every font API, because a
/// caller that has to think about ascender/descender arithmetic will get it
/// wrong at least once.
///
/// Returns the advance width consumed, so a caller can lay out a row without
/// re-measuring. Damage is recorded by [`blend_mask`], so the present path picks
/// the text up with no extra plumbing.
pub fn draw_text(
    id: SurfaceId,
    x: usize,
    baseline_y: usize,
    text: &str,
    size: u16,
    rgb: u32,
) -> Result<i32, &'static str> {
    let face = crate::font::Face::load()?;
    let baseline = baseline_y as i32;
    let mut pen = x as i32;
    for ch in text.chars() {
        // Cache-first: see GLYPH_CACHE. Re-rasterizing per character per frame
        // exhausted the heap and hung the compositor.
        let g = match cached_glyph(ch, size) {
            Some(g) => g,
            None => {
                // Cache unavailable (no face): fall back to the advance only, so
                // the caller still lays out correctly even if nothing draws.
                pen += (face.advance(face.glyph_for_char(ch)) as i32 * size as i32)
                    / face.units_per_em() as i32;
                continue;
            }
        };
        if g.w > 0 && g.h > 0 {
            // `bearing_y` is the distance UP from the baseline to the mask top,
            // so the mask's first row sits at `baseline - bearing_y`.
            let gx = pen + g.bearing_x;
            let gy = baseline - g.bearing_y;
            if gx >= 0 && gy >= 0 {
                let _ = blend_mask(
                    id,
                    gx as usize,
                    gy as usize,
                    &g.alpha,
                    g.w,
                    g.h,
                    rgb,
                )?;
            }
        }
        pen += g.advance;
    }
    Ok(pen - x as i32)
}

/// `(width, height, Format)` of a live surface - the accessor the compositor
/// needs before it can blit anything anywhere.
pub fn info(id: SurfaceId) -> Result<(usize, usize, Format), &'static str> {
    geometry(id)
}

/// Take (and clear) the accumulated damage rect, exclusive bounds.
pub fn take_damage(id: SurfaceId) -> Option<(usize, usize, usize, usize)> {
    let _g = LOCK.lock();
    unsafe {
        let s = TABLE
            .get_mut(id as usize)
            .and_then(|slot| slot.as_mut())?;
        s.damage.take()
    }
}

/// `(live, failed_create)` counters for the P6a self-test's report line.
pub fn stats() -> (u32, u32) {
    (LIVE.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}
