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
