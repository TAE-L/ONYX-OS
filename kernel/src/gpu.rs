//! M10a: PCI display scan + kernel-controlled modesetting.
//!
//! Two halves:
//!
//! 1. **Scan** â€” `pci::init` has already enumerated the bus; [`init`] picks the
//!    display function out of that registry (class 0x03, VGA-compatible
//!    subclass 0x00 first, then any other display subclass) and reports its
//!    BARs. That is where the framebuffer's next address comes from instead of
//!    the bootloader's VBE mode.
//! 2. **Modesetting** â€” program QEMU's VBE/dispi registers (ports 0x1CE index /
//!    0x1CF data, `VBE_DISPI_INDEX_*`) to the kernel's chosen mode, map the
//!    linear framebuffer BAR at a fixed kernel virtual address, and re-point
//!    the console at it (`framebuffer::adopt`).
//!
//! Everything runs in the boot path (before `interrupts::enable()` and before
//! the first task is scheduled), which is what makes the two hairy steps safe:
//!
//!   * the dispi ports are a two-step (index + data) access â€” same hazard class
//!     as the PCI config port (PLAN.md M9.6-B1): a preemption between the two
//!     halves would corrupt the transfer, so the whole sequence stays in one
//!     IF=0 window;
//!   * page-table edits must not interleave with any other `map_to`, which is
//!     why the mapping goes through `memory::with_global_frames` â€” the
//!     kernel-service lock (M9.8-(d)) plus the frame allocator in one guard.
//!
//! Every step that can fail logs `[gpu] ... fallback` and leaves the bootloader
//! framebuffer exactly as it was, so a machine with no display device
//! (`-vga none`) or a display we cannot program still boots and passes every
//! suite. Nothing here panics.
//!
//! Scope note: this is the *kernel* console only. The ring-3 UI path is not
//! touched â€” `framebuffer::init_global` (and with it the UI's backing buffer)
//! is still created from the bootloader framebuffer in `userspace::init`, which
//! runs before the mode change; routing that path through the dispi surface is
//! M10c's job.

use alloc::vec::Vec;
use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::structures::paging::mapper::Translate;
use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::pci;

/// The mode the kernel picks when the display device can be programmed.
/// 1920x1080x32 = ~8.3 MiB of pixels: the bootloader's own VBE mode was
/// already this resolution, so the kernel-controlled mode matches it (no
/// visible "resize flicker" at hand-over), and it sits well inside QEMU's
/// default 16 MiB of video memory (the test suite boots with vgamem_mb=128,
/// like a modern GPU's local memory).
pub const MODE_W: usize = 1920;
pub const MODE_H: usize = 1080;
pub const MODE_BPP: usize = 4;

/// Rehearse the *fallback* half of modesetting on every boot (before the
/// console is handed over): program a mode the kernel does not want (16 bpp),
/// confirm the read-back verification catches it, then restore the good mode
/// and confirm the verification accepts it again.
///
/// Why this is here: the graceful-fallback path ("modesetting not possible ->
/// keep the bootloader framebuffer") is the one branch a QEMU test cannot
/// provoke from outside â€” QEMU's std VGA always implements the dispi interface,
/// and the machine-level alternatives are not usable (`-vga none` leaves the
/// bootloader without a VBE mode, so the kernel never gets a serial line at
/// all: the boot dies before our code runs â€” see the M10a bug log in
/// PLAN.md). Rehearsing the bad write *inside* the kernel is deterministic and
/// exercises the very same verification + restore code the real failure path
/// uses. Flip to `false` for a quieter log.
const FALLBACK_REHEARSAL: bool = true;


/// Where the framebuffer BAR is mapped.
///
/// Deliberately *not* inside the bootloader's physical-memory window: that
/// window is a 1:1 view of the whole of RAM, and a framebuffer BAR can sit
/// inside RAM on a machine with more memory (`-m 3G` and up would overlap
/// 0xfd000000). 4 TiB (PML4 slot 8) is provably unused: the kernel image lives
/// in slot 0, the physical-memory window in slot 5, the heap in slot 136
/// (0x4444_4444_0000). [`map_lfb`] still re-checks with `translate_addr` and
/// refuses to map over a live mapping instead of clobbering it.
pub const FB_VADDR: u64 = 0x0000_0400_0000_0000;

// --- VBE/dispi register interface -------------------------------------------
// QEMU's std VGA, bochs-display and virtio-vga all expose the bochs VBE
// extension through this index/data port pair.
const DISPI_INDEX_PORT: u16 = 0x01CE;
const DISPI_DATA_PORT: u16 = 0x01CF;
const IDX_ID: u16 = 0x0;
const IDX_XRES: u16 = 0x1;
const IDX_YRES: u16 = 0x2;
const IDX_BPP: u16 = 0x3;
const IDX_ENABLE: u16 = 0x4;
const IDX_REVISION: u16 = 0x5;
const IDX_VIDEO_MEMORY_64K: u16 = 0xA;
const DISPI_DISABLED: u16 = 0x0;
const DISPI_ENABLED: u16 = 0x1;

/// `VBE_DISPI_ID0..ID5` â€” the interface versions QEMU can report. Anything
/// outside this range means "no dispi device behind those ports".
const DISPI_ID_MIN: u16 = 0xB0C0;
const DISPI_ID_MAX: u16 = 0xB0C5;


/// What the boot path did to the display (`None` = fallback: the bootloader
/// framebuffer is still the console target).
#[derive(Clone, Copy)]
pub struct GpuMode {
    /// Vendor:device of the display function (`0x12341111` for QEMU's VGA).
    pub ven_dev: u32,
    pub width: usize,
    pub height: usize,
    pub bpp: usize,
    /// Which BAR of the display function the framebuffer lives in (not always
    /// 0 — vmware-svga keeps its IO ports in BAR0 and the LFB in BAR1).
    pub bar_idx: usize,
    /// BAR base/size as reported *before* the mode write.
    pub bar_base: u32,
    pub bar_size: u32,
    /// Linear framebuffer address read back *after* the mode write (the
    /// authority for the mapping: a mode switch may move the VGA window).
    pub lfb_addr: u32,
    /// `VBE_DISPI_ENABLE` read back after programming it (1 = live).
    pub enable: u16,
    /// Bytes mapped at [`FB_VADDR`].
    pub mapped: usize,
    /// Canary checksum computed from the written pattern (32-bit wrapped sum).
    pub canary_expected: u32,
    /// Canary checksum read back out of the mapped framebuffer.
    pub canary_actual: u32,
}

/// The mode the kernel installed.
static MODE: Mutex<Option<GpuMode>> = Mutex::new(None);
/// Set once [`init`] finished with a working dispi mode â€” readable without
/// taking a mutex from task/interrupt context.
static MODE_PRESENT: AtomicBool = AtomicBool::new(false);
/// Steps that fell back to the bootloader framebuffer (0 = full dispi path).
static FALLBACKS: AtomicU32 = AtomicU32::new(0);
/// The dispi device id read at boot (0 = the ports did not answer).
static DISPI_ID: AtomicU32 = AtomicU32::new(0);
/// Fallback rehearsals that were really detected by the read-back check.
static REHEARSALS: AtomicU32 = AtomicU32::new(0);

/// The mode we installed, if any.
pub fn mode() -> Option<GpuMode> {
    *MODE.lock()
}

/// Is a kernel-programmed dispi mode live? (cheap, lock-free)
pub fn mode_present() -> bool {
    MODE_PRESENT.load(Ordering::Acquire)
}

/// How many steps fell back to the bootloader framebuffer.
pub fn fallbacks() -> u32 {
    FALLBACKS.load(Ordering::Relaxed)
}

/// The dispi device id read at boot (`0` = no bochs VBE interface).
pub fn dispi_id() -> u32 {
    DISPI_ID.load(Ordering::Relaxed)
}

/// How many fallback rehearsals the read-back check really caught.
pub fn rehearsals() -> u32 {
    REHEARSALS.load(Ordering::Relaxed)
}

/// One `[gpu]` line to serial AND the framebuffer console (the M9.6-B1
/// convention: serial is what the test suites grep, the console is what a human
/// looking at the screen sees).
fn log(line: &str) {
    crate::serial_writeln!("{}", line);
    crate::framebuffer::console_bytes(line.as_bytes());
    crate::framebuffer::console_bytes(b"\n");
}

/// Count one fallback and say why.
fn fallback(why: &str) {
    FALLBACKS.fetch_add(1, Ordering::Relaxed);
    log(why);
}

/// Read one dispi register.
///
/// # Safety
/// Ports 0x1CE/0x1CF must only be touched with interrupts disabled: the pair is
/// a two-step index/data transfer (the boot path is IF=0 by construction; task
/// context must wrap this in `without_interrupts`).
unsafe fn dispi_read(index: u16) -> u16 {
    let mut idx = Port::<u16>::new(DISPI_INDEX_PORT);
    let mut data = Port::<u16>::new(DISPI_DATA_PORT);
    idx.write(index);
    data.read()
}

/// Write one dispi register (index write + data write, one atomic pair).
///
/// # Safety
/// Same two-step rule as [`dispi_read`].
unsafe fn dispi_write(index: u16, value: u16) {
    let mut idx = Port::<u16>::new(DISPI_INDEX_PORT);
    let mut data = Port::<u16>::new(DISPI_DATA_PORT);
    idx.write(index);
    data.write(value);
}

/// Program a dispi mode (disable â†’ geometry â†’ enable, the canonical bochs-VBE
/// order: the resolution registers are only latched by the ENABLE write, and
/// ENABLE=0 first makes the change apply even on top of an active mode).
///
/// # Safety
/// Two-step port access: interrupts must be disabled by the caller (the boot
/// path is IF=0 by construction).
unsafe fn set_mode_raw(xres: u16, yres: u16, bpp: u16) {
    dispi_write(IDX_ENABLE, DISPI_DISABLED);
    dispi_write(IDX_XRES, xres);
    dispi_write(IDX_YRES, yres);
    dispi_write(IDX_BPP, bpp);
    dispi_write(IDX_ENABLE, DISPI_ENABLED);
}

/// Program a mode and read all four registers back: `(xres, yres, bpp,
/// enable)`. The read-back is the honest answer to "did the hardware take it?"
/// â€” QEMU silently *rejects* unbootable geometry (e.g. a surface larger than
/// the device's VRAM) by leaving the registers at their old values.
fn set_mode(xres: u16, yres: u16, bpp: u16) -> (u16, u16, u16, u16) {
    unsafe {
        set_mode_raw(xres, yres, bpp);
        (
            dispi_read(IDX_XRES),
            dispi_read(IDX_YRES),
            dispi_read(IDX_BPP),
            dispi_read(IDX_ENABLE),
        )
    }
}

/// Does a read-back `rb` prove the device accepted the mode `want`?
fn mode_matches(rb: (u16, u16, u16, u16), want: (u16, u16, u16)) -> bool {
    rb.0 == want.0 && rb.1 == want.1 && rb.2 == want.2 && rb.3 & DISPI_ENABLED != 0
}

/// The four mode registers as they are right now: `(xres, yres, bpp, enable)`.
fn read_mode_raw() -> (u16, u16, u16, u16) {
    unsafe {
        (
            dispi_read(IDX_XRES),
            dispi_read(IDX_YRES),
            dispi_read(IDX_BPP),
            dispi_read(IDX_ENABLE),
        )
    }
}

/// Put the display back the way the boot found it. Called from every failure
/// *after* a modeset attempt: a fallback must not leave the hardware in a
/// half-programmed mode that nobody owns (the console then draws into the
/// bootloader framebuffer while the device scans out some other geometry).
fn restore_mode(saved: (u16, u16, u16, u16)) -> (u16, u16, u16, u16) {
    if saved.3 & DISPI_ENABLED != 0 {
        // The firmware mode was live: program it and latch it again.
        set_mode(saved.0, saved.1, saved.2)
    } else {
        // Nothing was enabled: leave it disabled, only the registers changed.
        unsafe {
            dispi_write(IDX_XRES, saved.0);
            dispi_write(IDX_YRES, saved.1);
            dispi_write(IDX_BPP, saved.2);
            dispi_write(IDX_ENABLE, DISPI_DISABLED);
        }
        read_mode_raw()
    }
}

/// [`restore_mode`] + one log line (the `tag` is `fallback` for a real fallback
/// and `fallback-rehearsal` for the rehearsal, so a passing boot is never
/// mistaken for a boot that actually fell back).
fn restore_and_log(saved: (u16, u16, u16, u16), tag: &str) {
    if saved.0 == 0 && saved.1 == 0 && saved.2 == 0 {
        return; // nothing meaningful was programmed before us
    }
    let rb = restore_mode(saved);
    log(&alloc::format!(
        "[gpu] {tag}: pre-boot dispi mode {}x{}x{} restored (read back {}x{}x{} \
         enable={})",
        saved.0,
        saved.1,
        saved.2,
        rb.0,
        rb.1,
        rb.2,
        rb.3
    ));
}

/// The one way out of a failed modeset: undo the mode change, then report the
/// fallback (reason + counter) with the shared wording.
fn fallback_restore(saved: (u16, u16, u16, u16), why: &str) {
    restore_and_log(saved, "fallback");
    fallback(why);
}

/// The single decision point for "this read-back is not the mode we asked for":
/// reports the fallback (same wording, same counter) and answers "unusable".
///
/// `want` is the mode the kernel needs, `attempted` is what was actually asked
/// of the device. They are equal on the live path; the rehearsal asks for a
/// deliberately bogus mode, and naming `want` there printed the *good* mode as
/// "not accepted" — a log line that lied about what had just happened.
///
/// Shared by the real path and the fallback rehearsal, so the rehearsal exercises
/// the *actual* decision code â€” not a copy of it.
fn unusable_mode(
    rb: (u16, u16, u16, u16),
    want: (u16, u16, u16),
    attempted: (u16, u16, u16),
    tag: &str,
) -> bool {
    if mode_matches(rb, want) {
        return false;
    }
    let what = "the bootloader framebuffer";
    if tag.is_empty() {
        FALLBACKS.fetch_add(1, Ordering::Relaxed);
        log(&alloc::format!(
            "[gpu] modeset: {}x{}x{} was not accepted (read back xres={} yres={} \
             bpp={} enable={}) - fallback to {what}",
            attempted.0,
            attempted.1,
            attempted.2,
            rb.0,
            rb.1,
            rb.2,
            rb.3
        ));
    } else {
        REHEARSALS.fetch_add(1, Ordering::Relaxed);
        log(&alloc::format!(
            "[gpu] {tag}: deliberately bogus {}x{}x{} read back as xres={} yres={} \
             bpp={} enable={} - the same check the live boot uses would fall back \
             to {what}",
            attempted.0,
            attempted.1,
            attempted.2,
            rb.0,
            rb.1,
            rb.2,
            rb.3
        ));
    }
    true
}

/// Boot-path entry point: scan the PCI registry for a display device, program
/// its mode, map the framebuffer and re-point the console. Never fails hard â€”
/// every error path logs a `[gpu] ... fallback` line and returns.
///
/// MUST be called with interrupts disabled and before the first task is
/// scheduled (`main` calls it right after `pci::init`), and AFTER
/// `memory::init_global_frames` (the mapping needs the runtime frame
/// allocator).
pub fn init() {
    // ---- 1. scan: which display device does this machine have? -------------
    let Some(dev) = pci::find_display() else {
        fallback(
            "[gpu] scan: no display-class (0x03) device in the PCI registry \
             - fallback to the bootloader framebuffer",
        );
        return;
    };
    let mut line = alloc::format!(
        "[gpu] scan: display device {} {} class={:02x}{:02x}({}) ",
        dev.slot(),
        dev.identify(),
        dev.class,
        dev.subclass,
        dev.class_name()
    );
    for i in 0..6 {
        let (is_io, base, size) = dev.bars[i];
        if size == 0 {
            continue;
        }
        line.push_str(&alloc::format!(
            "bar{i}={}:{base:#x}/{} ",
            if is_io { "io" } else { "mem" },
            if size == pci::UNSIZED {
                alloc::string::String::from("?")
            } else {
                pci::size_str(size)
            }
        ));
    }
    log(&line);

    // ---- 2. dispi probe: is the bochs VBE interface there? ----------------
    // M10b first: if this is a modern virtio-gpu (virtio-vga shell), probe its
    // capability list now — the probe result decides which backend M10b builds
    // the command path on. The dispi path below stays the live console this
    // increment either way; the virtio backend takes over once its command
    // path exists.
    if crate::virtio::is_virtio_gpu(&dev) {
        match crate::virtio::init(&dev) {
            Some(info) => {
                crate::serial_writeln!(
                    "[vgpu] backend note: virtio-gpu transport probed: {} - {} queue(s), {} scanout(s), features {:#010x}; M10b command path pending - dispi console stays live for now",
                    info.transport(),
                    info.num_queues,
                    info.num_scanouts,
                    info.device_features
                );
            }
            None => {
                crate::serial_writeln!(
                    "[vgpu] backend note: virtio probe failed - dispi console stays \
                     the only path (graceful)"
                );
            }
        }
    }
    let (id, revision, vram_64k) = unsafe {
        (
            dispi_read(IDX_ID),
            dispi_read(IDX_REVISION),
            dispi_read(IDX_VIDEO_MEMORY_64K),
        )
    };
    DISPI_ID.store(u32::from(id), Ordering::Relaxed);
    if !(DISPI_ID_MIN..=DISPI_ID_MAX).contains(&id) {
        let (w, h, bpp) = crate::framebuffer::current_geometry();
        fallback(&alloc::format!(
            "[gpu] dispi: no VBE interface behind 0x1CE/0x1CF (id={id:#06x}, \
             want {DISPI_ID_MIN:#06x}..={DISPI_ID_MAX:#06x}) - fallback: keep \
             the bootloader framebuffer ({w}x{h}x{bpp})"
        ));
        return;
    }
    // What the firmware (SeaBIOS's VBE mode, requested by the bootloader) had
    // programmed before we touch anything. Kept so every failure *after* a
    // modeset can put the display back exactly as we found it: a fallback that
    // leaves the hardware in a half-set mode nobody owns would be worse than
    // never having tried.
    let saved = read_mode_raw();
    log(&alloc::format!(
        "[gpu] dispi: id={id:#06x} revision={revision} vram={} KiB, mode as found \
         {}x{}x{} enable={} - bochs VBE interface live",
        u32::from(vram_64k) * 64,
        saved.0,
        saved.1,
        saved.2,
        saved.3
    ));

    // ---- 3. modeset: disable, set xres/yres/bpp, enable -------------------
    // The canonical bochs-VBE order: the resolution registers are only latched
    // by the ENABLE write, and ENABLE=0 first makes the change apply even if a
    // firmware/VBE mode was already active.
    let want = (MODE_W as u16, MODE_H as u16, (MODE_BPP * 8) as u16);
    let rb = set_mode(want.0, want.1, want.2);
    // One decision function for the real path and the rehearsal below: the
    // read-back is the honest answer to "did the hardware take it?" (QEMU
    // silently *rejects* unbootable geometry — e.g. a surface larger than the
    // device's VRAM — by leaving the registers at their old values).
    if unusable_mode(rb, want, want, "") {
        // The device did not take the mode: put back what we found and stop.
        restore_and_log(saved, "fallback");
        return;
    }
    log(&alloc::format!(
        "[gpu] modeset: {MODE_W}x{MODE_H}x{} programmed and read back (enable={})",
        MODE_BPP * 8,
        rb.3
    ));

    // ---- 3b. fallback rehearsal -------------------------------------------
    if FALLBACK_REHEARSAL {
        let bad = set_mode(want.0, want.1, 16);
        let bad_wanted = (want.0, want.1, 16u16);
        let programmed = mode_matches(bad, bad_wanted);
        // Ask the *live* decision function about this read-back: it must report
        // the mode as unusable (a 16 bpp read-back is not the 32 bpp mode we
        // want), and it counts a rehearsal instead of a real fallback.
        let caught = unusable_mode(bad, want, bad_wanted, "fallback-rehearsal");
        if programmed && caught {
            log(&alloc::format!(
                "[gpu] fallback-rehearsal: bogus mode {MODE_W}x{MODE_H}x16 was \
                 really programmed and the read-back check detected it \
                 (bpp={} != {}) - the graceful-fallback verification is live",
                bad.2,
                want.2
            ));
        } else {
            log(&alloc::format!(
                "[gpu] fallback-rehearsal: bogus mode was NOT detected (read \
                 back xres={} yres={} bpp={} enable={}, programmed={programmed}, \
                 caught={caught}) - verification is weaker than assumed",
                bad.0,
                bad.1,
                bad.2,
                bad.3
            ));
        }
        let rb = set_mode(want.0, want.1, want.2);
        if unusable_mode(rb, want, want, "fallback-rehearsal") {
            return;
        }
        log(&alloc::format!(
            "[gpu] fallback-rehearsal: {MODE_W}x{MODE_H}x{} restored and \
             re-verified - the bootloader framebuffer stayed mapped the whole time",
            MODE_BPP * 8
        ));
    }

    // ---- 4. where is the linear framebuffer *now*? ------------------------
    // The BAR *index* comes from the registry and is NOT always 0: QEMU's
    // vmware-svga keeps its legacy IO ports in BAR0 and puts the framebuffer in
    // BAR1. Reading index 0 blindly there returned an IO BAR base (0xc000), and
    // the canary would then have written 3 MiB into *config/IO space*. Re-read
    // the chosen BAR through config space (IF=0 here): a mode switch may also
    // move or resize the VGA window, and the post-switch address is the only one
    // we may map — the registry's cached value is a floor, not the truth.
    let (bar_idx, cached_base, bar_size) = match pci::framebuffer_bar(&dev) {
        Some((idx, base, size)) => (idx, base, size),
        None => (0, 0, 0),
    };
    let fresh = pci::read_bar_base(&dev, bar_idx);
    let lfb = if fresh != 0 { fresh } else { cached_base };
    let mode_bytes = MODE_W * MODE_H * MODE_BPP;
    if lfb == 0 {
        fallback_restore(
            saved,
            "[gpu] mapping: display device has no memory BAR - fallback to the \
             bootloader framebuffer",
        );
        return;
    }
    log(&alloc::format!(
        "[gpu] bar{bar_idx} after modeset: base={lfb:#010x} size={}{} (mode surface {} KiB)",
        if bar_size == pci::UNSIZED {
            alloc::string::String::from("unknown")
        } else {
            pci::size_str(bar_size)
        },
        if bar_size != 0 && bar_size != pci::UNSIZED && mode_bytes as u32 > bar_size {
            " - WARNING: mode surface is larger than the reported BAR"
        } else {
            ""
        },
        mode_bytes >> 10
    ));

    // ---- 5. map the framebuffer BAR at FB_VADDR ---------------------------
    let Some(fb) = map_lfb(u64::from(lfb), mode_bytes) else {
        fallback_restore(
            saved,
            "[gpu] mapping: could not map the framebuffer BAR - fallback to the \
             bootloader framebuffer",
        );
        return;
    };
    log(&alloc::format!(
        "[gpu] mapping: {} KiB at {FB_VADDR:#014x} (virt) -> {lfb:#010x} (phys)",
        mode_bytes >> 10
    ));

    // ---- 6. canary: prove the mapping is the device's memory --------------
    let points = canary_points(MODE_W, MODE_H);
    let (expected, actual) = canary_roundtrip(fb, MODE_W, &points);
    log(&alloc::format!(
        "[gpu] canary: wrote {} pixels (expect sum {expected:#010x}), read back \
         {actual:#010x} - mapping {}",
        points.len(),
        if expected == actual {
            "verified"
        } else {
            "MISMATCH"
        }
    ));
    if expected != actual {
        fallback_restore(
            saved,
            "[gpu] canary: framebuffer read-back mismatch - fallback to the \
             bootloader framebuffer",
        );
        return;
    }

    // ---- 7. hand the screen over to the new framebuffer -------------------
    let info = FrameBufferInfo {
        byte_len: mode_bytes,
        width: MODE_W,
        height: MODE_H,
        pixel_format: PixelFormat::Bgr,
        bytes_per_pixel: MODE_BPP,
        stride: MODE_W,
    };
    crate::framebuffer::adopt(fb, info);
    let (cx, cy) = crate::framebuffer::cursor_pos();
    crate::serial_writeln!("[gpu] cursor re-armed at ({cx}, {cy}) on the dispi surface");

    let bar_base = if cached_base != 0 { cached_base } else { lfb };
    *MODE.lock() = Some(GpuMode {
        ven_dev: (u32::from(dev.vendor_id) << 16) | u32::from(dev.device_id),
        width: MODE_W,
        height: MODE_H,
        bpp: MODE_BPP,
        bar_idx,
        bar_base,
        bar_size,
        lfb_addr: lfb,
        enable: rb.3,
        mapped: mode_bytes,
        canary_expected: expected,
        canary_actual: actual,
    });
    MODE_PRESENT.store(true, Ordering::Release);
    log(&alloc::format!(
        "[gpu] scan-ok: display device {} {} class=0300(display) bar{bar_idx}=mem:\
         {lfb:#010x}/{}",
        dev.slot(),
        dev.identify(),
        if bar_size == pci::UNSIZED || bar_size == 0 {
            alloc::string::String::from("?")
        } else {
            pci::size_str(bar_size)
        }
    ));

    // One summary line on the *freshly adopted* console: every line above went
    // to the old surface and is gone from the screen (the serial log keeps it
    // all, which is what the suites grep).
    log(&alloc::format!(
        "[gpu] mode set: {MODE_W}x{MODE_H}x{} dispi LFB {lfb:#010x} (BAR{bar_idx} {}, \
         {} KiB mapped, canary {actual:#010x} verified) - console on the dispi \
         framebuffer",
        MODE_BPP * 8,
        if bar_size == pci::UNSIZED || bar_size == 0 {
            alloc::string::String::from("size unknown")
        } else {
            pci::size_str(bar_size)
        },
        mode_bytes >> 10
    ));
}

/// Map `size` bytes of physical framebuffer at `phys` onto [`FB_VADDR`] and
/// return the slice, or `None` (mapping refused / impossible â€” caller falls
/// back). 4 KiB pages, deliberately: the bootloader's own mappings are
/// 4 KiB-page based and the BAR is page-aligned, so a normal page mapping
/// cannot shadow a huge-page entry of another region.
fn map_lfb(phys: u64, size: usize) -> Option<&'static mut [u8]> {
    if phys % 4096 != 0 {
        crate::serial_writeln!("[gpu] mapping: BAR base {phys:#x} is not page aligned");
        return None;
    }
    let pages = (size + 4095) / 4096;
    let Some(mut mapper) = crate::memory::runtime_mapper() else {
        crate::serial_writeln!("[gpu] mapping: no runtime mapper (frames not handed over yet)");
        return None;
    };
    // Refuse to map over something that is already there: this address is
    // supposed to be free, and clobbering a live mapping would be a silent
    // corruption of whoever owns it.
    if mapper
        .translate_addr(VirtAddr::new(FB_VADDR))
        .is_some()
    {
        crate::serial_writeln!(
            "[gpu] mapping: {FB_VADDR:#014x} is already mapped - refusing to clobber it"
        );
        return None;
    }
    let mut ok = true;
    // The whole loop runs under the kernel-service lock + frame allocator
    // guard (M9.8-(d)): no other CPU may be editing the page tables next to us.
    crate::memory::with_global_frames(|frames| {
        for i in 0..pages {
            let off = i as u64 * 4096;
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(FB_VADDR + off));
            let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys + off));
            let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
            match unsafe { mapper.map_to(page, frame, flags, frames) } {
                Ok(flush) => flush.flush(),
                Err(e) => {
                    crate::serial_writeln!(
                        "[gpu] mapping: page {i} ({:#x}) failed: {e:?}",
                        FB_VADDR + off
                    );
                    ok = false;
                    break;
                }
            }
        }
    })?;
    if !ok {
        return None;
    }
    // SAFETY: just mapped, exclusively ours, aligned, `size` bytes long.
    Some(unsafe { core::slice::from_raw_parts_mut(FB_VADDR as *mut u8, size) })
}

// ---------------------------------------------------------------------------
// Canary: is the mapped surface really the display's memory?
// ---------------------------------------------------------------------------

/// Pixels written per canary region (8x8 = one 64-byte half-line).
const CANARY_PER_REGION: usize = 64;
/// Tag bits so a stray zero-filled read-back cannot look like success.
const CANARY_TAG: u32 = 0xA500_0000;

/// The pixels the canary touches: top-left, bottom-right and the right end of
/// the middle row. Deliberately NOT the whole surface (192 pixels Ã— 4 bytes =
/// 768 bytes): the point is to prove the mapping reaches the device's memory
/// and that the row stride the console uses is the stride the hardware uses â€”
/// a full-surface write would cost ~25 ms of guest time under TCG.
///
/// All three regions stay inside the mode's own 3 MiB surface, so the writes
/// are inside the BAR even if the (probed) BAR size were wrong.
fn canary_points(w: usize, h: usize) -> Vec<(usize, usize)> {
    let mut pts = Vec::with_capacity(3 * CANARY_PER_REGION);
    for dy in 0..8 {
        for dx in 0..8 {
            pts.push((dx, dy)); // top-left
            pts.push((w - 8 + dx, h - 8 + dy)); // bottom-right
            pts.push((w - 8 + dx, h / 2 + dy)); // middle-right: stride proof
        }
    }
    pts
}

/// The 32-bit value stored at `(x, y)` (x/y are folded in, so a wrong stride or
/// a shifted mapping reads back different values, not just garbage).
fn canary_value(x: usize, y: usize) -> u32 {
    CANARY_TAG | ((y as u32) << 10) | (x as u32)
}

/// Write the canary pattern into `fb` and read it back. Returns
/// `(expected_sum, actual_sum)` â€” equal means the mapping round-trips.
///
/// Byte-wise, little-endian: independent of the pixel format, so it works the
/// same for the 32bpp dispi surface and any other layout.
fn canary_roundtrip(fb: &mut [u8], w: usize, points: &[(usize, usize)]) -> (u32, u32) {
    let stride = w * MODE_BPP;
    let mut expected = 0u32;
    for &(x, y) in points {
        let v = canary_value(x, y);
        expected = expected.wrapping_add(v);
        let o = y * stride + x * MODE_BPP;
        fb[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut actual = 0u32;
    for &(x, y) in points {
        let o = y * stride + x * MODE_BPP;
        let mut b = [0u8; 4];
        b.copy_from_slice(&fb[o..o + 4]);
        actual = actual.wrapping_add(u32::from_le_bytes(b));
    }
    (expected, actual)
}

// ---------------------------------------------------------------------------
// Task half: the installed mode is verified again from scheduler context
// ---------------------------------------------------------------------------

/// Extra (verbose) per-boot check: also probe one byte of the mapped LFB from
/// task context. Read-only on purpose â€” by then the console is *drawing* into
/// that memory, so a canary re-run there would fight the renderer; the point of
/// the probe is only that the translation still works from a preempted task
/// (`read_volatile` on a raw pointer: no long-lived reference is formed).
const TASK_LFB_PROBE: bool = true;

/// Runs as a scheduler task, so nothing here may assume boot context: this CPU
/// may be any CPU, the timer is live, and the task can migrate mid-run.
///
/// Its job is the serial-observable proof that the modeset *survived* the
/// hand-over from the boot path to the scheduler: the dispi registers are still
/// programmed and the mapped LFB still translates.
fn task() {
    let Some(m) = mode() else {
        crate::serial_writeln!(
            "[gpu] task: no modeset ({} fallback step(s)) - the bootloader framebuffer is live",
            fallbacks()
        );
        crate::scheduler::exit_current();
    };
    // Two-step port access from a task must be one IF=0 window: the LAPIC timer
    // preempting between the index and the data write would leave the pair
    // half-applied (same hazard class as the PCI config port, M9.6-B1).
    let (xres, yres, bpp, enable) =
        x86_64::instructions::interrupts::without_interrupts(|| unsafe {
            (
                dispi_read(IDX_XRES),
                dispi_read(IDX_YRES),
                dispi_read(IDX_BPP),
                dispi_read(IDX_ENABLE),
            )
        });
    crate::serial_writeln!(
        "[gpu] task: dispi registers from task context: {xres}x{yres}x{bpp} enable={enable} \
         (cpu {}); dispi id={:#06x} fallbacks={} rehearsals={} mode_present={}",
        crate::smp::cpu_index(),
        dispi_id(),
        fallbacks(),
        rehearsals(),
        mode_present()
    );
    if TASK_LFB_PROBE {
        let row = m.height / 2;
        let mid = FB_VADDR + (row * m.width * MODE_BPP) as u64;
        let probe = unsafe { core::ptr::read_volatile(mid as *const u8) };
        crate::serial_writeln!(
            "[gpu] task: LFB probe byte @{mid:#014x} row {row} = {probe:#04x} - mapping translates"
        );
    }
    // M10b transport visibility: report the virtio probe result from task
    // context too (what the display backend will build on next).
    match crate::virtio::device() {
        Some(v) => crate::serial_writeln!(
            "[vgpu] task: virtio-gpu transport: {} - queues {} scanouts {} \
             features {:#010x} isr {}",
            v.transport(),
            v.num_queues,
            v.num_scanouts,
            v.device_features,
            match v.caps.isr {
                Some((b, o, l)) => alloc::format!("bar{b}/{o:#x}/{l}"),
                None => alloc::string::String::from("none"),
            }
        ),
        None if crate::virtio::probe_ok() => {
            crate::serial_writeln!("[vgpu] task: probe ok but no device recorded")
        }
        None => {}
    }
    crate::serial_writeln!(
        "[gpu] task: mode {}x{}x{} on {:04x}:{:04x}, BAR{} {:#010x} (LFB {:#010x}), \
         {} KiB mapped at {FB_VADDR:#014x}, canary {:#010x}/{}, BAR size {}, enable={}",
        m.width,
        m.height,
        m.bpp * 8,
        (m.ven_dev >> 16) as u16,
        m.ven_dev as u16,
        m.bar_idx,
        m.bar_base,
        m.lfb_addr,
        m.mapped >> 10,
        m.canary_actual,
        if m.canary_actual == m.canary_expected {
            "verified"
        } else {
            "MISMATCH"
        },
        if m.bar_size == 0 || m.bar_size == crate::pci::UNSIZED {
            alloc::string::String::from("unknown")
        } else {
            crate::pci::size_str(m.bar_size)
        },
        m.enable
    );
    crate::scheduler::exit_current();
}

/// Spawn the GPU verification task. Called from `kernel_main` next to
/// `smp::spawn_bringup_task` â€” the task runs only once the scheduler is live,
/// i.e. strictly after `init` finished the modeset.
pub fn spawn_task() {
    crate::scheduler::spawn(task);
}

