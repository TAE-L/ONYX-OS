//! M10b: virtio-gpu transport probe + the stage-2 control queue, GEM-lite
//! resource and kernel-drawn present path.
//!
//! QEMU's virtio-vga wraps a **virtio-gpu** device (PCI 1af4:1050, modern
//! virtio 1.0) behind a VGA-compatible shell (that shell is what lets SeaBIOS
//! boot it at all: there is no VGA BIOS for bare `virtio-gpu-pci`). The modern
//! interface lives in the PCI *capability list*: vendor-specific capabilities
//! (id 0x09) each describing one config structure — common (feature
//! negotiation, queues), notify (doorbells), ISR, and the GPU-specific device
//! config (`num_scanouts`).
//!
//! **Stage 1 (transport probe).** Walk that capability list, map the config
//! pages, negotiate ACKNOWLEDGE|DRIVER|FEATURES_OK, read the device
//! identity/features, and enable PCI bus mastering. This runs in the boot path
//! (IF=0), where the two-step PCI config port and the register writes are safe
//! from preemption.
//!
//! **Stage 2 (control queue + present path).** Give the device ONE queue —
//! the controlq (index 0) — inside a single 4 KiB frame (descriptor table +
//! avail + used rings), program it, latch DRIVER_OK, and use it to run the
//! virtio-gpu 2D command sequence:
//!
//! ```text
//! RESOURCE_CREATE_2D → RESOURCE_ATTACH_BACKING → SET_SCANOUT
//!                     → TRANSFER_TO_HOST_2D → RESOURCE_FLUSH
//! ```
//!
//! The resource's backing is one contiguous guest-RAM span described with a
//! single backing entry ("GEM-lite": no object manager, no BOs, no fences).
//! Every ring and backing byte is reached through the bootloader's
//! physical-memory window, so the device DMAs exactly the addresses the
//! common config was programmed with. The console is adopted onto that
//! surface LAST — only after a canary proves the backing round-trips and a
//! transfer+flush proves the device is driving it — and the GPU task then
//! becomes the 100 ms flusher.
//!
//! **Damage-rect present (stage 3a).** The flusher does not push the whole
//! surface: it asks `framebuffer::take_dirty_rect()` for the bounding box of
//! everything the console drew since the last push, and issues
//! TRANSFER_TO_HOST_2D + RESOURCE_FLUSH for that rect only — skipping both
//! commands entirely when nothing changed. Measured on a `-vga virtio` boot:
//! ~90 % less DMA, and an idle console costs nothing. A box covers a burst of
//! text but stays tiny for a single keystroke, so the saving holds under load.
//!
//! Conventions inherited from M10a: every failure is a logged, counted
//! fallback, never a panic; serial lines are one-shot summaries; the
//! graceful-fallback path is asserted on machines with no virtio device at
//! all (boots 1/2 of `test-gpu.ps1`).
//!
//! Log prefix: `[vgpu]` (the `[gpu]` prefix stays the display-backend story as
//! a whole — dispi lines keep it; these lines are the virtio transport's own).

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use core::ptr::{read_volatile, write_bytes, write_volatile};
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::structures::paging::{
    mapper::Translate, FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::pci::PciFunction;

/// Virtio vendor id (QEMU's virtio-vga reports it as subsystem vendor too).
const VIRTIO_VENDOR: u16 = 0x1AF4;
/// PCI device id for a modern virtio-gpu: 0x1040 + virtio device id 0x10.
const VIRTIO_PCI_DEVICE_GPU: u16 = 0x1050;

/// Capability ids in the PCI capability list.
const CAP_VENDOR: u8 = 0x09;
/// `VirtioPCICap.cfg_type` values.
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

/// Common-config `device_status` bits (`device_status` is at +0x14).
/// The *boot-path* probe (IF=0) stops at `FEATURES_OK` (8) — the honest state
/// of a driver that finished feature negotiation. `DRIVER_OK` (4) means
/// "queues are up and I am ready for commands", so it is set later, by
/// [`queue_init`] in task context, and only after the queue's rings are
/// programmed and the read-backs verify.
const ST_RESET: u8 = 0;
const ST_ACKNOWLEDGE: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_DRIVER_OK: u8 = 4;
const ST_FEATURES_OK: u8 = 8;

/// Common-config queue registers (virtio 1.2 §4.1.4.2, byte offsets from the
/// start of the common structure). The 64-bit address registers are written
/// as two u32 halves (low at +0, high at +4) because the config window is
/// byte-addressed and the high half is what a device above 4 GiB would read.
const CC_QUEUE_SELECT: u16 = 0x16;
const CC_QUEUE_SIZE: u16 = 0x18;
const CC_QUEUE_ENABLE: u16 = 0x1C;
const CC_QUEUE_NOTIFY_OFF: u16 = 0x1E;
const CC_QUEUE_DESC: u16 = 0x20;
const CC_QUEUE_DRIVER: u16 = 0x28;
const CC_QUEUE_USED: u16 = 0x30;

// --- virtio-gpu 2D control protocol -----------------------------------------

/// Command request types (virtio-gpu 1.0 §5.7 "Device Commands").
///
/// The 2D block is a CONTIGUOUS enum starting at `0x0100`
/// (`VIRTIO_GPU_CMD_GET_DISPLAY_INFO = 0x0100`), so the codes are assigned by
/// *position*, not grouped by meaning. Transcribing them from the spec's prose
/// list instead of the enum shifts every code after UNREF by one — and the
/// failure is silent and convincing: the device accepts the command, dispatches
/// it to the wrong handler, and answers with a plausible-looking error about
/// an unrelated field. Verified against `include/uapi/linux/virtio_gpu.h`:
///
///   0x0100 GET_DISPLAY_INFO      0x0104 RESOURCE_FLUSH
///   0x0101 RESOURCE_CREATE_2D    0x0105 TRANSFER_TO_HOST_2D
///   0x0102 RESOURCE_UNREF        0x0106 RESOURCE_ATTACH_BACKING
///   0x0103 SET_SCANOUT           0x0107 RESOURCE_DETACH_BACKING
const GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

/// `VIRTIO_GPU_RESP_OK_NODATA` — the only success a 2D command can return
/// (they carry no payload). Every error is >= 0x1200 and is reported as
/// `ERR_*` (e.g. 0x1201 `INVALID`, 0x1207 `NO_MEM`).
const GPU_RESP_OK: u32 = 0x1100;

/// `VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM` = 2: the byte order of a little-endian
/// u32 `0xXXRRGGBB`, i.e. B,G,R,X in memory — the same layout the console's
/// `PixelFormat::Bgr` draws, and DRM's XRGB8888.
///
/// NOTE the enum value, which is easy to get wrong: the "simple formats"
/// block is `B8G8R8A8=1`, `B8G8R8X8=2`, `A8R8G8B8=3`, `X8R8G8B8=4`. Sending
/// `1` would ask for BGRA-with-alpha, which is a *different* format from the
/// one the console's Bgr/32bpp renderer produces, so the scanout would show
/// every colour with a forced alpha byte. The X variant is the honest one
/// here: the console never stores alpha (it is always opaque), and the
/// device ignores it.
const GPU_FMT_B8G8R8X8: u32 = 2;

/// Modern virtio only: the interface is *modern* when the driver walks its
/// capability list (`virtio_pci_cap`, id 0x09) **and** the device advertises
/// `VIRTIO_F_VERSION_1` (bit 0 of feature word 1).
///
/// The modern common config has **no version register**. An earlier revision of
/// this probe read a `version` at common-config +4 and compared it to 2 — which
/// "passed" because +4 is `device_feature` and QEMU's low feature half happens
/// to be 0x0002 (EDID). See the M10b bug log in PLAN.md.
const FEAT_WORD0: u32 = 0;
const FEAT_WORD1: u32 = 1;
const FEAT_VERSION_1: u32 = 1 << 0;

/// GPU-specific feature bits (word 0) — `virtio_gpu_features`.
/// `VIRGL` is what turns the device into a host-GPU 3D renderer; QEMU only sets
/// it when a GL-capable display backend is attached (`-display egl-headless`
/// + virglrenderer, which QEMU for Windows cannot provide), so the driver must
/// treat it as optional and stay usable without it.
const GPU_F_VIRGL: u32 = 1 << 0;
const GPU_F_EDID: u32 = 1 << 1;
const GPU_F_RESOURCE_BLOB: u32 = 1 << 3;
const GPU_F_CONTEXT_INIT: u32 = 1 << 4;

/// Where M10b maps the device's config pages (one page per config structure).
/// 5 PiB (PML4 slot 10) — kernel=0, phys window=5, heap slot 136, the M10a LFB
/// at slot 8; `map_config` refuses to clobber anything already mapped.
pub const CFG_VADDR: u64 = 0x0000_1400_0000_0000;

/// The device's config structures, as the capability list described them.
#[derive(Clone, Copy)]
pub struct VirtioGpuCaps {
    /// (bar_index, byte_offset, byte_length) per config structure.
    pub common: (usize, u32, u32),
    pub notify: Option<(usize, u32, u32)>,
    pub isr: Option<(usize, u32, u32)>,
    pub device: Option<(usize, u32, u32)>,
    /// `notify_off_multiplier` from the notify cap (doorbell arithmetic).
    pub notify_mul: u32,
}

/// What the probe learned (stored in `DEVICE` after a successful `init`).
#[derive(Clone, Copy)]
pub struct VirtioGpuInfo {
    pub caps: VirtioGpuCaps,
    /// Common-config `config_generation` (+0x15): the device config's own
    /// change counter — the honest "this interface is alive" byte.
    pub config_generation: u8,
    /// `num_queues` (the GPU device defines 2: control + cursor).
    pub num_queues: u16,
    /// `device_feature` word 0 (GPU-specific bits).
    pub device_features: u32,
    /// `device_feature` word 1 (transport bits, `VIRTIO_F_VERSION_1`).
    pub device_features_hi: u32,
    /// `VIRTIO_F_VERSION_1` — the modern-transport proof.
    pub version_1: bool,
    /// `VIRTIO_GPU_F_VIRGL` — host-GPU 3D available (needs a GL backend).
    pub virgl: bool,
    /// `VIRTIO_GPU_F_EDID` — the device can report the display's EDID.
    pub edid: bool,
    /// `VIRTIO_GPU_F_RESOURCE_BLOB` — host-visible blobs (zero-copy VRAM).
    pub resource_blob: bool,
    /// `VIRTIO_GPU_F_CONTEXT_INIT` — multi-context 3D (Virgin per-process).
    pub context_init: bool,
    /// `num_scanouts` from the GPU device config.
    pub num_scanouts: u32,
    /// Mapped virtual address of the common config (queue registers live
    /// here) — kept so the task-context queue bring-up does not have to
    /// re-resolve (and possibly re-map) the capability.
    pub common_va: u64,
    /// Mapped virtual address of the *notify* config (the doorbell window);
    /// 0 when the device exposes no notify capability or it could not be
    /// mapped. The queue engine refuses to arm without it.
    pub notify_va: u64,
}

/// The probed device (`None` until a virtio display function probed OK).
static DEVICE: Mutex<Option<VirtioGpuInfo>> = Mutex::new(None);

impl VirtioGpuInfo {
    /// Human name of the negotiated transport.
    ///
    /// There is **no version register** in the modern common config: "modern" is
    /// proven by the driver walking the capability list *and* by
    /// `VIRTIO_F_VERSION_1` being advertised and negotiated — which is what this
    /// string reports. (An earlier revision of this probe read a `version` at
    /// common-config +4, which is `device_feature`; see the M10b bug log.)
    pub fn transport(&self) -> &'static str {
        if self.version_1 {
            "modern (VIRTIO_F_VERSION_1 negotiated)"
        } else {
            "legacy (VERSION_1 absent)"
        }
    }
}

/// Set once the probe completed (cheap check for the task summary).
static PROBE_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The probed device, if any.
pub fn device() -> Option<VirtioGpuInfo> {
    *DEVICE.lock()
}
/// Did the probe complete? (cheap, lock-free)
pub fn probe_ok() -> bool {
    PROBE_OK.load(core::sync::atomic::Ordering::Acquire)
}

/// `[vgpu]` line to serial + console (same convention as `[gpu]`).
fn log(line: &str) {
    crate::serial_writeln!("{}", line);
    crate::framebuffer::console_bytes(line.as_bytes());
    crate::framebuffer::console_bytes(b"\n");
}

/// Is this function a modern virtio-gpu?
pub fn is_virtio_gpu(f: &PciFunction) -> bool {
    f.vendor_id == VIRTIO_VENDOR && f.device_id == VIRTIO_PCI_DEVICE_GPU
}

/// Verbose capability-list dump (M10b bring-up aid): prints the raw config
/// bytes from 0x30 to 0xFF — 26 serial lines, hence OFF by default (serial is
/// the slowest device in the machine). Flip it on when a device's capabilities
/// do not parse: it is how the +8/+12/+16 offsets below were verified against
/// QEMU's actual output instead of guessed. The same gate style as
/// `smp::AP_TRACE`.
const CAP_DUMP: bool = false;

/// First byte of the PCI capability list (0 = none).
fn pci_cap_ptr(f: &PciFunction) -> u8 {
    crate::pci::config_byte(f, 0x34)
}

/// Raw hex dump of the config-space area that holds the capability list.
fn dump_config_area(f: &PciFunction) {
    let mut off: u8 = 0x30;
    loop {
        let mut line = alloc::format!("[vgpu] cfg {off:#04x}:");
        for i in 0..8u8 {
            line.push_str(&alloc::format!(" {:02x}", crate::pci::config_byte(f, off + i)));
        }
        log(&line);
        if off >= 0xF8 {
            break;
        }
        off += 8;
    }
}

/// 32-bit little-endian value from the config space at a possibly unaligned
/// offset (virtio cap fields are not 4-byte aligned).
fn config_dword_unaligned(f: &PciFunction, offset: u8) -> u32 {
    let mut v = 0u32;
    for i in 0..4 {
        v |= u32::from(crate::pci::config_byte(f, offset + i)) << (8 * i);
    }
    v
}

/// Read the device's PCI capability list and pick out the virtio structures.
/// `None` if the device has no modern virtio caps (legacy-only, or not virtio).
///
/// # Safety contract (callers): interrupts MUST be disabled — this walks the
/// PCI config port (two-step CF8h/CFC handshake, M9.6-B1).
fn walk_caps(f: &PciFunction) -> Option<VirtioGpuCaps> {
    if CAP_DUMP {
        dump_config_area(f);
    }
    let mut ptr = pci_cap_ptr(f);
    let mut common: Option<(usize, u32, u32)> = None;
    let mut notify: Option<(usize, u32, u32)> = None;
    let mut isr: Option<(usize, u32, u32)> = None;
    let mut device: Option<(usize, u32, u32)> = None;
    let mut notify_mul = 0u32;
    // Bounded walk: a malformed device must not spin forever on a
    // self-looping next-pointer.
    for _ in 0..48 {
        if ptr == 0 || ptr > 0xFC {
            break;
        }
        let cap_id = crate::pci::config_byte(f, ptr);
        let next = crate::pci::config_byte(f, ptr + 1);
        let cap_len = crate::pci::config_byte(f, ptr + 2);
        // virtio_pci_cap (virtio 1.2 §4.1.4), byte offsets from the cap start:
        //   +0 cap_vndr · +1 cap_next · +2 cap_len · +3 cfg_type · +4 bar
        //   +5 id · +6 padding[2] · +8 offset(u32) · +12 length(u32)
        //   (notify only) +16 notify_off_multiplier(u32)
        // The +3/+5 shift is the trap: `bar` sits at +4 but `offset` does NOT
        // follow it — id+padding sit in between. Verified against QEMU's own
        // config dump (CAP_DUMP below) instead of trusting the memory of the
        // spec; the dump is how the layout was confirmed.
        if cap_id == CAP_VENDOR && cap_len >= 16 {
            let cfg_type = crate::pci::config_byte(f, ptr + 3);
            let bar = crate::pci::config_byte(f, ptr + 4) as usize;
            let off = config_dword_unaligned(f, ptr + 8);
            let len = config_dword_unaligned(f, ptr + 12);
            if bar > 5 {
                ptr = next;
                continue;
            }
            match cfg_type {
                CFG_COMMON => common = Some((bar, off, len)),
                CFG_NOTIFY => {
                    notify = Some((bar, off, len));
                    // Only the notify cap is 20 bytes long; reading the
                    // multiplier out of a 16-byte cap would eat the next cap's
                    // header bytes as a multiplier.
                    if cap_len >= 20 {
                        notify_mul = config_dword_unaligned(f, ptr + 16);
                    }
                }
                CFG_ISR => isr = Some((bar, off, len)),
                CFG_DEVICE => device = Some((bar, off, len)),
                _ => {}
            }
        }
        ptr = next;
    }
    // The common config is the minimum a modern device must expose.
    Some(VirtioGpuCaps {
        common: common?,
        notify,
        isr,
        device,
        notify_mul,
    })
}

// ---------------------------------------------------------------------------
// MMIO mapping for the config pages
// ---------------------------------------------------------------------------

/// Map one physical page at the next free CFG_VADDR slot. Returns the virtual
/// address, or `None` (logged) if mapping is impossible.
fn map_config_page(phys: u64, slot: usize) -> Option<VirtAddr> {
    let vaddr = VirtAddr::new(CFG_VADDR + slot as u64 * 0x1000);
    let Some(mut mapper) = crate::memory::runtime_mapper() else {
        log("[vgpu] probe: no runtime mapper - cannot map device config");
        return None;
    };
    if mapper.translate_addr(vaddr).is_some() {
        log(&alloc::format!(
            "[vgpu] probe: {vaddr:#014x} already mapped - refusing to clobber it"
        ));
        return None;
    }
    let page = Page::<Size4KiB>::containing_address(vaddr);
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    let mut ok = false;
    // Page-table edit + frame allocation under the KSL (M9.8-(d)); IF=0 comes
    // from the boot-path caller.
    crate::memory::with_global_frames(|frames| {
        match unsafe { mapper.map_to(page, frame, flags, frames) } {
            Ok(flush) => {
                flush.flush();
                ok = true;
            }
            Err(e) => log(&alloc::format!(
                "[vgpu] probe: map {vaddr:#014x} -> {phys:#x} failed: {e:?}"
            )),
        }
    })?;
    if !ok {
        return None;
    }
    Some(vaddr)
}

/// Resolve a cap's (bar, offset) to a mapped virtual address. The cap pages
/// are page-aligned in the BAR (QEMU places each config structure in its own
/// 4 KiB slot), so one page per structure is enough this increment.
fn resolve_cap(f: &PciFunction, cap: (usize, u32, u32), slot: usize) -> Option<VirtAddr> {
    let (bar, off, len) = cap;
    let base = crate::pci::read_bar_base(f, bar);
    if base == 0 {
        log(&alloc::format!(
            "[vgpu] probe: cap bar{bar} has no base - device misconfigured"
        ));
        return None;
    }
    let phys = u64::from(base) + u64::from(off);
    if len == 0 || len > 0x1000 {
        log(&alloc::format!(
            "[vgpu] probe: cap at bar{bar}+{off:#x} has implausible length {len}"
        ));
        return None;
    }
    // One page per structure this increment, so the structure must not straddle
    // a page boundary — and the cap's own address must keep its offset inside
    // that page (QEMU happens to place every structure page-aligned, which is
    // exactly why a version of this that returned the page base looked fine).
    let in_page = phys & 0xFFF;
    if in_page + u64::from(len) > 0x1000 {
        log(&alloc::format!(
            "[vgpu] probe: cap at bar{bar}+{off:#x} len {len} crosses a page \
             boundary - only the first page would be mapped"
        ));
        return None;
    }
    if in_page != 0 {
        log(&alloc::format!(
            "[vgpu] probe: cap at bar{bar}+{off:#x} is not page aligned - using \
             the page offset {in_page:#x} into the mapping"
        ));
    }
    let page_va = map_config_page(phys & !0xFFF, slot)?;
    Some(page_va + in_page)
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

/// Probe the virtio-gpu behind the display function. Returns the learned info,
/// or `None` (logged) — the caller then stays on the dispi backend.
///
/// MUST run in the boot path (IF=0, no tasks yet): config-space walk, MMIO
/// mapping and the status write are all preemption-sensitive.
pub fn init(f: &PciFunction) -> Option<VirtioGpuInfo> {
    // ---- capability walk ----------------------------------------------------
    let Some(caps) = walk_caps(f) else {
        log("[vgpu] probe: no modern virtio capability list on the display device");
        return None;
    };
    let fmt = |c: Option<(usize, u32, u32)>| match c {
        Some((b, o, l)) => alloc::format!("bar{b}+{o:#x}/{l}"),
        None => alloc::string::String::from("none"),
    };
    log(&alloc::format!(
        "[vgpu] caps: common {} · notify {} (mul {}) · isr {} · device {}",
        fmt(Some(caps.common)),
        fmt(caps.notify),
        caps.notify_mul,
        fmt(caps.isr),
        fmt(caps.device),
    ));

    // ---- map the common config ----------------------------------------------
    let Some(common_va) = resolve_cap(f, caps.common, 0) else {
        return None;
    };
    // M10b stage 2: the notify capability (slot 1) is the doorbell window.
    // Slot 2 (isr) stays unmapped on purpose - the queue engine polls the used
    // ring instead of taking an interrupt. A device without a usable notify
    // cap is not fatal here: `notify_va` stays 0 and `queue_init` refuses to
    // arm, so the dispi console keeps the screen.
    let notify_va = caps
        .notify
        .and_then(|c| resolve_cap(f, c, 1))
        .map(|v| v.as_u64())
        .unwrap_or(0);
    let common = common_va.as_u64() as *mut u8;
    let rd8 = |off: usize| unsafe { read_volatile(common.add(off)) };
    let rd16 = |off: usize| unsafe { read_volatile(common.add(off) as *const u16) };
    let rd32 = |off: usize| unsafe { read_volatile(common.add(off) as *const u32) };
    let wr8 = |off: usize, v: u8| unsafe { write_volatile(common.add(off), v) };
    let wr32 = |off: usize, v: u32| unsafe { write_volatile(common.add(off) as *mut u32, v) };

    // ---- PCI command register: I/O + memory + bus master -------------------
    // Bus mastering (bit 2) is what lets the device DMA our rings and resource
    // backing; without it every doorbell is silently ignored. SeaBIOS sets
    // IO|MEM for the BARs it mapped but bus master is the driver's job, and
    // this runs in the boot path (IF=0), which the two-step config port needs.
    let command = crate::pci::config_or_u16(f, 0x04, 0x0007);
    log(&alloc::format!(
        "[vgpu] probe: PCI command |= io|mem|bus-master -> {command:#06x}"
    ));

    // ---- negotiate: reset, ACK, DRIVER --------------------------------------
    // The common config has NO version register (it starts with
    // `device_feature_select` at +0). "Modern" is proven by
    // `VIRTIO_F_VERSION_1` in feature word 1 — see the constants above and the
    // M10b bug log. Reset first: the VGA-compatible shell lets firmware walk
    // this device before we do, so it must not inherit an unknown status.
    wr8(0x14, ST_RESET);
    wr8(0x14, ST_ACKNOWLEDGE);
    wr8(0x14, ST_ACKNOWLEDGE | ST_DRIVER);
    let status = rd8(0x14);
    if status & (ST_ACKNOWLEDGE | ST_DRIVER) != (ST_ACKNOWLEDGE | ST_DRIVER) {
        log(&alloc::format!(
            "[vgpu] probe: device_status did not latch ACK|DRIVER (read back \
             {status:#04x}) - virtio transport unusable"
        ));
        return None;
    }

    // ---- device features: both words, then the modern-transport proof -------
    wr32(0x00, FEAT_WORD0);
    let device_features = rd32(0x04);
    wr32(0x00, FEAT_WORD1);
    let device_features_hi = rd32(0x04);
    let version_1 = device_features_hi & FEAT_VERSION_1 != 0;
    if !version_1 {
        log(&alloc::format!(
            "[vgpu] probe: VIRTIO_F_VERSION_1 not advertised (device_feature \
             word 1 = {device_features_hi:#010x}) - legacy-only device, staying \
             on the dispi backend"
        ));
        return None;
    }
    // Accept exactly what we implement: VERSION_1 in word 1, nothing in word 0.
    // EDID/RESOURCE_BLOB/VIRGL stay un-negotiated until the increments that
    // actually use them — a feature bit the driver sets is a promise the device
    // is allowed to rely on.
    wr32(0x08, FEAT_WORD0);
    wr32(0x0C, 0);
    wr32(0x08, FEAT_WORD1);
    wr32(0x0C, FEAT_VERSION_1);
    wr8(0x14, ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK);
    let status = rd8(0x14);
    if status & ST_FEATURES_OK == 0 {
        log(&alloc::format!(
            "[vgpu] probe: device rejected FEATURES_OK (read back {status:#04x}) \
             - sticking to the dispi backend"
        ));
        return None;
    }
    // +0x15 config_generation: the device config's own change counter, i.e. the
    // honest "this interface is alive and keeping state" byte.
    let config_generation = rd8(0x15);
    // `num_queues` is a little-endian u16 at +0x12 — reading it as a byte would
    // silently truncate a device that declares more than 255 queues (QEMU's GPU
    // declares 2, which is exactly why the bug would never show up here).
    let num_queues = rd16(0x12);

    // ---- GPU device config: events + num_scanouts ----------------------------
    let (dev, events_read, events_clear, num_scanouts) = match caps.device {
        Some(cap) if cap.2 >= 12 => {
            let Some(dev_va) = resolve_cap(f, cap, 3) else {
                return None;
            };
            let dev = dev_va.as_u64() as *mut u8;
            // struct virtio_gpu_config: events_read(4) events_clear(4)
            //                            num_scanouts(4) num_capsets(4)
            let (er, ec, ns) = unsafe {
                (
                    read_volatile(dev as *const u32),
                    read_volatile(dev.add(4) as *const u32),
                    read_volatile(dev.add(8) as *const u32),
                )
            };
            (dev, er, ec, ns)
        }
        _ => {
            log("[vgpu] probe: no GPU device config cap - cannot read num_scanouts");
            return None;
        }
    };
    if num_scanouts == 0 {
        // A GPU device with no display output cannot be modeset; QEMU reports 1.
        log(&alloc::format!(
            "[vgpu] probe: device reports {num_scanouts} scanouts - nothing to \
             drive, staying on the dispi backend"
        ));
        return None;
    }
    if events_read != 0 {
        // Buffered display events from a previous user of the device: clear them
        // so the driver's own event queue starts honest (write-1-to-clear).
        // Reuse the pointer from above — resolving the cap again would hit
        // `map_config_page`'s "already mapped" guard and abort the probe.
        unsafe { write_volatile(dev.add(4) as *mut u32, events_read) };
    }

    let info = VirtioGpuInfo {
        caps,
        config_generation,
        num_queues,
        device_features,
        device_features_hi,
        version_1,
        virgl: device_features & GPU_F_VIRGL != 0,
        edid: device_features & GPU_F_EDID != 0,
        resource_blob: device_features & GPU_F_RESOURCE_BLOB != 0,
        context_init: device_features & GPU_F_CONTEXT_INIT != 0,
        num_scanouts,
        common_va: common_va.as_u64(),
        notify_va,
    };
    log(&alloc::format!(
        "[vgpu] probe: features lo={device_features:#010x} hi={device_features_hi:#010x} \
         accepted={FEAT_VERSION_1:#010x} - GPU bits(want): virgl={} edid={} blob={} ctx={}",
        info.virgl as u8,
        info.edid as u8,
        info.resource_blob as u8,
        info.context_init as u8
    ));
    log(&alloc::format!(
        "[vgpu] probe: modern virtio-gpu live - status={status:#04x} (FEATURES_OK; \
         DRIVER_OK waits for the control virtqueue) cfg_gen={config_generation} \
         queues={num_queues} scanouts={num_scanouts} events_read={events_read:#010x} \
         events_clear={events_clear:#010x} - queue bring-up runs from the gpu task"
    ));
    *DEVICE.lock() = Some(info);
    PROBE_OK.store(true, core::sync::atomic::Ordering::Release);
    Some(info)
}

// ---------------------------------------------------------------------------
// M10b stage 2: the control virtqueue engine + GEM-lite resource + present path
// ---------------------------------------------------------------------------
//
// Stage 1 left the device at FEATURES_OK with four config windows mapped and
// no queues. Stage 2 gives it ONE queue - the controlq (queue 0) - and uses it
// to build a single 2D resource out of ordinary guest RAM, point the scanout at
// it, and hand the console over to it. The command sequence is the spec's own
// (virtio-gpu 1.0 §5.7):
//
//   RESOURCE_CREATE_2D -> RESOURCE_ATTACH_BACKING -> SET_SCANOUT ->
//   TRANSFER_TO_HOST_2D -> RESOURCE_FLUSH
//
// Every ring byte lives in a bump-allocated frame accessed through the
// bootloader's physical-memory window, so the device DMAs exactly the
// addresses the common config was told about (no page-table edits, no
// mapping dance, and the same 1:1 view the CPU writes the console through).
//
// Everything here runs in TASK context (the GPU task calls `present_bringup`),
// which is why the queue lock is a dedicated spin mutex and NOT the KSL: the
// used-ring poll can wait, and holding the kernel-service lock across a device
// wait would stall every other subsystem behind the display.

/// Split-ring layout inside the ONE frame allocated for the queue.
///
/// `size <= 64` keeps the whole queue inside a single 4 KiB frame:
///
///   desc  @0x000  size*16 bytes (addr u64, len u32, flags u16, next u16)
///   avail @0x400  flags u16, idx u16, ring u16[size]
///   used  @0x800  flags u16, idx u16, ring {id u32, len u32}[size]
///
/// The gaps are deliberate (fixed offsets, not packed): 64 descriptors end at
/// 0x400 exactly, and 0x400 + 4 + 64*2 = 0x484 stays clear of the used ring
/// at 0x800. Worst case used ends at 0xA04 < 0x1000, so one frame is enough.
const Q_SIZE_MAX: u16 = 64;
const DESC_OFF: u64 = 0x000;
const AVAIL_OFF: u64 = 0x400;
const USED_OFF: u64 = 0x800;
/// avail.idx / used.idx live right after their ring's flags word.
const AVAIL_IDX_OFF: u64 = AVAIL_OFF + 2;
const USED_IDX_OFF: u64 = USED_OFF + 2;
const AVAIL_RING_OFF: u64 = AVAIL_OFF + 4;
const USED_RING_OFF: u64 = USED_OFF + 4;
/// Request at +0, response at +512 of the second (command) frame.
const CMD_REQ_OFF: u64 = 0;
const CMD_RESP_OFF: u64 = 512;
const CMD_BUF_LEN: usize = 512;
/// Control-request header (virtio-gpu 1.0 §5.7.1): type u32, flags u32,
/// fence_id u64, ctx_id u32, ring_idx u8, pad[3] = 24 bytes. Everything but
/// `type` stays zero for a 2D command.
const CMD_HDR_LEN: usize = 24;
/// Descriptor flags (virtio 1.2 §2.7.5).
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// The single control queue the GPU device exposes (queue index 0).
///
/// `doorbell` is the pre-computed MMIO address of THIS queue's doorbell
/// (`notify_base + notify_off_multiplier * queue_notify_off`); the value
/// written there is the queue index itself, because NOTIFICATION_DATA was
/// not negotiated (word 0 is 0 - see the feature negotiation in `init`).
struct QueueState {
    ring_phys: u64,
    cmd_phys: u64,
    size: u16,
    /// Next avail slot the driver will publish (wraps; compare != ).
    avail_idx: u16,
    /// Next used slot the driver will consume.
    used_idx: u16,
    doorbell: u64,
}

/// The live control queue (`None` until `queue_init` succeeds).
///
/// A dedicated per-queue lock, deliberately NOT the KSL: the submit path
/// polls the used ring for up to [`CMD_TIMEOUT_MS`], and the KSL must never
/// be held across a device wait (M9.8-(d) discipline - the lock exists to
/// serialize kernel services, not to park the machine on a GPU).
static QUEUE: Mutex<Option<QueueState>> = Mutex::new(None);

/// Set once the console has been adopted onto the virtio surface. The
/// flusher only touches the device after this, so a failed bring-up cannot
/// start DMA traffic against a console that still lives on the dispi LFB.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// The resource the console is currently drawn on, published by
/// `present_bringup` once the adopt succeeded. The flusher task reads it
/// without another lock ordering: it is `Copy`, so the guard is dropped before
/// any command builder takes the queue lock.
static RESOURCE: Mutex<Option<GpuResource>> = Mutex::new(None);

/// How long one command may wait for its used-ring entry before it is
/// declared lost. 500 ms is ~10^5 times the device's real latency; it only
/// ever fires on a wedged or mis-addressed queue.
const CMD_TIMEOUT_MS: u64 = 500;


// ---------------------------------------------------------------------------
// Physical-window accessors: every ring/command/backing byte is reached as
// `phys_offset() + phys`, the same 1:1 mapping the device DMAs from
// ---------------------------------------------------------------------------

/// Virtual address of `phys` through the bootloader's physical-memory window,
/// or `None` if paging was never initialized (a bring-up cannot continue).
#[inline]
fn phys_va(phys: u64) -> Option<u64> {
    crate::memory::phys_offset().map(|off| off + phys)
}

/// Read a little-endian u32 out of guest RAM at `phys`.
///
/// # Safety
/// `phys` must be RAM mapped through the physical window (caller-owned frames
/// only). Used for device-populated buffers (the response type word, used-ring
/// entries) and ring state, so the loads are volatile: the compiler must not
/// cache or reorder them across the fences in `submit`.
#[inline]
unsafe fn phys_read_u32(phys: u64) -> u32 {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    read_volatile(va as *const u32)
}

/// Read a little-endian u16 out of guest RAM at `phys` (ring indices).
///
/// # Safety
/// Same contract as [`phys_read_u32`].
#[inline]
unsafe fn phys_read_u16(phys: u64) -> u16 {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    read_volatile(va as *const u16)
}

/// Write a little-endian u32 into guest RAM at `phys` (ring publishing).
///
/// # Safety
/// Same contract as [`phys_read_u32`].
#[inline]
unsafe fn phys_write_u32(phys: u64, value: u32) {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    write_volatile(va as *mut u32, value);
}

/// Write a little-endian u16 into guest RAM at `phys`.
///
/// # Safety
/// Same contract as [`phys_read_u32`].
#[inline]
unsafe fn phys_write_u16(phys: u64, value: u16) {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    write_volatile(va as *mut u16, value);
}

/// Zero `len` bytes of guest RAM at `phys` (fresh rings, command area, and
/// the scanout backing before the first canary write).
///
/// # Safety
/// Same contract as [`phys_read_u32`], plus `phys+len` must be inside RAM.
#[inline]
unsafe fn phys_write_bytes(phys: u64, len: usize) {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    write_bytes(va as *mut u8, 0, len);
}

/// Write one little-endian u32 field into the command request buffer.
///
/// # Safety
/// `off` must be inside the 512-byte request buffer (`phys` being the
/// request's physical address).
#[inline]
unsafe fn cmd_write_u32(phys: u64, off: usize, value: u32) {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    write_volatile((va as *mut u8).add(off) as *mut u32, value);
}

/// Write one little-endian u64 field into the command request buffer.
///
/// # Safety
/// Same contract as [`cmd_write_u32`].
#[inline]
unsafe fn cmd_write_u64(phys: u64, off: usize, value: u64) {
    let va = phys_va(phys).expect("physical-memory window not initialized");
    write_volatile((va as *mut u8).add(off) as *mut u64, value);
}

// ---------------------------------------------------------------------------
// Queue bring-up: allocate, program, verify, then DRIVER_OK (and only then)
// ---------------------------------------------------------------------------

/// Bring the control queue (index 0) up and latch `DRIVER_OK`.
///
/// Order matters and is the spec's order: negotiate (done in `init`) -> set up
/// the queue -> *then* DRIVER_OK. Setting DRIVER_OK first would tell the
/// device "I am ready for commands" about a queue that does not exist yet.
///
/// Runs from the GPU task (not the boot path): the common config window is
/// already mapped, and `pci::config_or_u16` — the only two-step-port access —
/// already ran in `init` with interrupts off.
///
/// Returns the queue's own log summary on success, or a short reason on
/// failure. Every step is read-back verified: QEMU silently ignores queue
/// registers it does not like (the same "geometry is the truth" lesson as the
/// dispi path in `gpu.rs`), so a write that does not read back is a failure,
/// never a warning.
fn queue_init() -> Result<(), &'static str> {
    let Some(info) = device() else {
        return Err("no virtio-gpu device recorded by the probe");
    };
    if info.notify_va == 0 {
        // Without the notify window there is no doorbell to ring, so no
        // command can ever complete. Refuse instead of arming a queue whose
        // completion path is unreachable.
        return Err("notify capability is not mapped");
    }
    if info.num_queues == 0 {
        return Err("device advertises zero queues");
    }

    let common = info.common_va as *mut u8;
    let rd8 = |off: usize| unsafe { read_volatile(common.add(off)) };
    let rd16 = |off: usize| unsafe { read_volatile(common.add(off) as *const u16) };
    let wr8 = |off: usize, v: u8| unsafe { write_volatile(common.add(off), v) };
    let wr16 = |off: usize, v: u16| unsafe { write_volatile(common.add(off) as *mut u16, v) };
    let wr32 = |off: usize, v: u32| unsafe { write_volatile(common.add(off) as *mut u32, v) };

    // Select the control queue before touching any queue register.
    wr16(CC_QUEUE_SELECT as usize, 0);
    let max_size = rd16(CC_QUEUE_SIZE as usize);
    // Power-of-two floor: the spec requires a power of two, and the ring
    // arithmetic below (idx % size) depends on it.
    let mut size = core::cmp::min(max_size, Q_SIZE_MAX);
    if size > 2 {
        // Round DOWN to a power of two (1 -> 2 -> 4 -> ... <= size).
        let mut p = 2u16;
        while p * 2 <= size {
            p *= 2;
        }
        size = p;
    }
    if size < 2 {
        return Err("device offers a control queue smaller than 2 descriptors");
    }

    // Two frames: one for all three rings, one for request + response. Plain
    // single-frame allocation (the free list is fine here - nothing needs the
    // span to be contiguous, only the ring base addresses to be valid).
    let (ring_phys, cmd_phys) = crate::memory::with_global_frames(|frames| {
        let ring = frames.allocate_frame().map(|f| f.start_address().as_u64());
        let cmd = frames.allocate_frame().map(|f| f.start_address().as_u64());
        ring.zip(cmd)
    })
    .ok_or("global frame allocator is not installed")?
    .ok_or("out of frames for the control queue")?;

    // Zero both: avail/used flags and indices, the whole descriptor table, and
    // the command header's "everything but type" requirement all start clean.
    unsafe {
        phys_write_bytes(ring_phys, 4096);
        phys_write_bytes(cmd_phys, 4096);
    }

    // Program the queue. Both halves of each 64-bit address are written even
    // though RAM is below 4 GiB (high half = 0): a device that reads only the
    // low half would work here and break on the first machine where it does
    // not, and the cost is six stores.
    wr16(CC_QUEUE_SIZE as usize, size);
    if rd16(CC_QUEUE_SIZE as usize) != size {
        return Err("device did not accept the programmed queue size");
    }
    let desc = ring_phys + DESC_OFF;
    let avail = ring_phys + AVAIL_OFF;
    let used = ring_phys + USED_OFF;
    wr32(CC_QUEUE_DESC as usize, desc as u32);
    wr32(CC_QUEUE_DESC as usize + 4, (desc >> 32) as u32);
    wr32(CC_QUEUE_DRIVER as usize, avail as u32);
    wr32(CC_QUEUE_DRIVER as usize + 4, (avail >> 32) as u32);
    wr32(CC_QUEUE_USED as usize, used as u32);
    wr32(CC_QUEUE_USED as usize + 4, (used >> 32) as u32);

    // Doorbell: notify_base + notify_off_multiplier * queue_notify_off. The
    // *value* written there is the queue index (0) - NOTIFICATION_DATA was
    // not negotiated, so the device reads the index, not an address.
    let notify_off = rd16(CC_QUEUE_NOTIFY_OFF as usize);
    let doorbell = info.notify_va + u64::from(notify_off) * u64::from(info.caps.notify_mul);
    if doorbell < info.notify_va || doorbell + 4 > info.notify_va + 0x1000 {
        // The computed doorbell fell outside the mapped notify page: the cap's
        // multiplier or offset is not what we think it is. Refuse rather than
        // write to a stranger's MMIO.
        return Err("computed doorbell lies outside the mapped notify page");
    }

    // Enable, read back, then DRIVER_OK.
    wr16(CC_QUEUE_ENABLE as usize, 1);
    if rd16(CC_QUEUE_ENABLE as usize) != 1 {
        return Err("device did not enable the control queue");
    }
    let status = rd8(0x14) | ST_DRIVER_OK;
    wr8(0x14, status);
    let status = rd8(0x14);
    if status & ST_DRIVER_OK == 0 {
        return Err("device did not latch DRIVER_OK");
    }

    *QUEUE.lock() = Some(QueueState {
        ring_phys,
        cmd_phys,
        size,
        avail_idx: 0,
        used_idx: 0,
        doorbell,
    });
    log(&alloc::format!(
        "[vgpu] queue: controlq size={size} rings at phys {ring_phys:#x} - DRIVER_OK (status={status:#04x})"
    ));
    Ok(())
}


// ---------------------------------------------------------------------------
// Submit: publish two chained descriptors, ring the doorbell, poll the used ring
// ---------------------------------------------------------------------------

/// Submit one command whose request bytes the caller already wrote at
/// `cmd_phys + CMD_REQ_OFF`, and return the device's response type word.
///
/// Two descriptors, always the same pair:
///   desc[0] = request  (device-readable, F_NEXT -> 1)
///   desc[1] = response (device-written, F_WRITE)
///
/// The publish sequence is the spec's (virtio 1.2 §2.7.13): write the avail
/// ring entry, `fence`, bump `avail.idx`, `fence`, then ring the doorbell. Both
/// fences are load-bearing - without the first the device could see the new
/// index before the entry, and without the second it could see the index but
/// not the entry's contents.
///
/// The used-ring poll is a busy wait with `spin_loop()`: this is task context
/// with a live timer, the queue lock is held, and parking would need a wait
/// queue the rest of this increment does not have. 500 ms is ~10^5x the
/// device's real latency.
fn submit(req_len: u32) -> Result<u32, &'static str> {
    let mut guard = QUEUE.lock();
    let q = guard.as_mut().ok_or("control queue is not initialized")?;
    let req = q.cmd_phys + CMD_REQ_OFF;
    let resp = q.cmd_phys + CMD_RESP_OFF;

    // Descriptor 0: the request, chained to descriptor 1.
    let d0 = q.ring_phys + DESC_OFF;
    unsafe {
        phys_write_u32(d0, req as u32);
        phys_write_u32(d0 + 4, (req >> 32) as u32);
        phys_write_u32(d0 + 8, req_len);
        phys_write_u16(d0 + 12, DESC_F_NEXT);
        phys_write_u16(d0 + 14, 1);
        // Descriptor 1: the device-written response buffer.
        phys_write_u32(d0 + 16, resp as u32);
        phys_write_u32(d0 + 20, (resp >> 32) as u32);
        phys_write_u32(d0 + 24, CMD_BUF_LEN as u32);
        phys_write_u16(d0 + 28, DESC_F_WRITE);
        phys_write_u16(d0 + 30, 0);
        // avail.ring[avail_idx % size] = head descriptor index (always 0:
        // this engine reuses the same pair for every command).
        let slot = q.ring_phys + AVAIL_RING_OFF + u64::from(q.avail_idx % q.size) * 2;
        phys_write_u16(slot, 0);
    }
    fence(Ordering::SeqCst);
    q.avail_idx = q.avail_idx.wrapping_add(1);
    unsafe { phys_write_u16(q.ring_phys + AVAIL_IDX_OFF, q.avail_idx) };
    fence(Ordering::SeqCst);
    // Doorbell VALUE = queue index (NOTIFICATION_DATA not negotiated).
    unsafe { write_volatile(q.doorbell as *mut u32, 0) };

    // Poll for the completion. `used_idx` is our cursor, so a wrapped u16 that
    // happens to equal ours would be a missed completion - at one command per
    // 100 ms the 16-bit index cannot wrap inside the 500 ms deadline.
    let deadline = crate::apic::ms_since_boot() + CMD_TIMEOUT_MS;
    loop {
        let used_idx = unsafe { phys_read_u16(q.ring_phys + USED_IDX_OFF) };
        if used_idx != q.used_idx {
            fence(Ordering::SeqCst);
            let elem = q.ring_phys + USED_RING_OFF + u64::from(q.used_idx % q.size) * 8;
            let id = unsafe { phys_read_u32(elem) };
            let len = unsafe { phys_read_u32(elem + 4) };
            q.used_idx = q.used_idx.wrapping_add(1);
            if id != 0 {
                return Err("device completed an unknown descriptor");
            }
            if len < 4 {
                return Err("response buffer too short to hold a type word");
            }
            return Ok(unsafe { phys_read_u32(resp) });
        }
        if crate::apic::ms_since_boot() >= deadline {
            return Err("device did not answer within 500 ms");
        }
        core::hint::spin_loop();
    }
}

/// Start a command: zero the 512-byte request buffer, write the 24-byte
/// control header (type only - flags/fence/ctx/ring stay 0), and return its
/// physical address for the payload writes.
///
/// # Safety
/// Caller must submit (or discard) the command before the next call - the
/// buffer is single-slot by design, which is all a synchronous submit loop
/// needs and keeps the queue one frame.
fn cmd_begin(ty: u32) -> Result<u64, &'static str> {
    let guard = QUEUE.lock();
    let q = guard.as_ref().ok_or("control queue is not initialized")?;
    let req = q.cmd_phys + CMD_REQ_OFF;
    unsafe {
        phys_write_bytes(req, CMD_BUF_LEN);
        cmd_write_u32(req, 0, ty);
    }
    Ok(req)
}


// ---------------------------------------------------------------------------
// GEM-lite resources: one 2D resource, one contiguous backing span
// ---------------------------------------------------------------------------

/// A 2D scanout resource owned by the kernel (virtio-gpu 1.0 §5.7.3).
///
/// "GEM-lite" is the whole point: no GEM object manager, no BOs, no fences -
/// one resource whose backing is a contiguous guest-RAM span, described to the
/// device with a single `ATTACH_BACKING` entry. Scatter-gather backing and
/// proper buffer objects are stage 3; until then this is the smallest thing
/// that can own a scanout and still be drawn into by the console.
#[derive(Clone, Copy)]
struct GpuResource {
    id: u32,
    w: u32,
    h: u32,
    format: u32,
    /// Physical address of the backing's first byte (4 KiB aligned).
    backing_phys: u64,
    /// Backing length in bytes.
    backing_bytes: u64,
}

/// Run one command end to end: build it, submit it, and require `GPU_RESP_OK`.
///
/// The queue lock is taken ONLY here - never while a console/KSL lock is held
/// (the flusher and the console renderer run concurrently, and nesting those
/// locks is the classic M9.8-(d) deadlock). A non-OK response becomes a named
/// error so the caller's log says which command the device refused.
fn run_command(ty: u32, name: &'static str, build: impl FnOnce(u64)) -> Result<(), &'static str> {
    let req = cmd_begin(ty)?;
    build(req);
    let resp = submit(cmd_len_for(ty))?;
    if resp != GPU_RESP_OK {
        // The response code IS the diagnosis, and the enum is short:
        //   0x1200 ERR_UNSPEC           0x1203 ERR_INVALID_RESOURCE_ID
        //   0x1201 ERR_OUT_OF_MEMORY    0x1204 ERR_INVALID_CONTEXT_ID
        //   0x1202 ERR_INVALID_SCANOUT  0x1205 ERR_INVALID_PARAMETER
        // (Linux uapi: include/uapi/linux/virtio_gpu.h). Reporting the raw
        // code with the command name is what turns "the GPU is broken" into
        // a one-line answer.
        log(&alloc::format!(
            "[vgpu] command: {name} (type {ty:#06x}, {} B) refused - response {resp:#06x}",
            cmd_len_for(ty)
        ));
        return Err("device refused the command (error response >= 0x1200)");
    }
    Ok(())
}

/// Request length for each command type, verified field by field against the
/// Linux UAPI header (`include/uapi/linux/virtio_gpu.h`), which is the same
/// layout QEMU's `VIRTIO_GPU_FILL_CMD` reads.
///
/// The trailing `__le32 padding` fields are NOT optional: the device checks
/// the descriptor length against the full C struct, so a request that stops at
/// the last real field is a *short read* and comes back as `BAD_FORMAT`
/// (0x1205) - which is exactly what a "close enough" length looks like from
/// the guest's side.
fn cmd_len_for(ty: u32) -> u32 {
    match ty {
        // hdr 24 + resource_id 4 + format 4 + width 4 + height 4
        GPU_CMD_RESOURCE_CREATE_2D => 40,
        // hdr 24 + resource_id 4 + nr_entries 4 + 1 * mem_entry 16
        //   (mem_entry = addr 8 + length 4 + padding 4)
        GPU_CMD_RESOURCE_ATTACH_BACKING => 48,
        // hdr 24 + rect 16 + scanout_id 4 + resource_id 4
        GPU_CMD_SET_SCANOUT => 48,
        // hdr 24 + rect 16 + offset 8 + resource_id 4 + padding 4
        GPU_CMD_TRANSFER_TO_HOST_2D => 56,
        // hdr 24 + rect 16 + resource_id 4 + padding 4
        GPU_CMD_RESOURCE_FLUSH => 48,
        _ => CMD_HDR_LEN as u32,
    }
}


/// `RESOURCE_CREATE_2D` - allocate the resource and its dimensions/format.
/// No backing yet: the resource is valid but has no memory until the next
/// command attaches it.
fn create_2d(res: &GpuResource) -> Result<(), &'static str> {
    run_command(
        GPU_CMD_RESOURCE_CREATE_2D,
        "RESOURCE_CREATE_2D",
        |req| unsafe {
            cmd_write_u32(req, CMD_HDR_LEN, res.id);
            cmd_write_u32(req, CMD_HDR_LEN + 4, res.format);
            cmd_write_u32(req, CMD_HDR_LEN + 8, res.w);
            cmd_write_u32(req, CMD_HDR_LEN + 12, res.h);
        },
    )
}

/// `RESOURCE_ATTACH_BACKING` - hand the device the one guest-RAM span the
/// console will draw into. The entry is (addr u64, length u32) and the length
/// covers the WHOLE backing, not one row: the device addresses it linearly.
fn attach_backing(res: &GpuResource) -> Result<(), &'static str> {
    run_command(
        GPU_CMD_RESOURCE_ATTACH_BACKING,
        "RESOURCE_ATTACH_BACKING",
        |req| unsafe {
            cmd_write_u32(req, CMD_HDR_LEN, res.id);
            cmd_write_u32(req, CMD_HDR_LEN + 4, 1); // nr_entries
            // one virtio_gpu_mem_entry at offset 32: addr, length, padding.
            // The padding is written (not left to the buffer-zero) so the
            // struct's full 16 bytes are accounted for in the descriptor.
            cmd_write_u64(req, CMD_HDR_LEN + 8, res.backing_phys);
            cmd_write_u32(req, CMD_HDR_LEN + 16, res.backing_bytes as u32);
            cmd_write_u32(req, CMD_HDR_LEN + 20, 0); // entry padding
        },
    )
}

/// `SET_SCANOUT` - point a scanout at the resource for the given rectangle.
/// On QEMU's virtio-vga this takes the muxed display away from the legacy
/// stdvga face: the dispi mode is still programmed and still verified (M10a),
/// but the pixels the user now sees come from this resource.
fn set_scanout(res: &GpuResource, scanout_id: u32) -> Result<(), &'static str> {
    run_command(GPU_CMD_SET_SCANOUT, "SET_SCANOUT", |req| unsafe {
        cmd_write_u32(req, CMD_HDR_LEN, 0); // rect.x
        cmd_write_u32(req, CMD_HDR_LEN + 4, 0); // rect.y
        cmd_write_u32(req, CMD_HDR_LEN + 8, res.w); // rect.w
        cmd_write_u32(req, CMD_HDR_LEN + 12, res.h); // rect.h
        cmd_write_u32(req, CMD_HDR_LEN + 16, scanout_id);
        cmd_write_u32(req, CMD_HDR_LEN + 20, res.id);
    })
}

/// `TRANSFER_TO_HOST_2D` - copy the given rect out of guest RAM into the
/// device's host-side surface. Semantics that matter: the console keeps
/// drawing into guest RAM only, and this is what makes the device's copy
/// current. `offset` is added to the guest address of the rect's top-left.
///
/// The rect is explicit rather than "always the whole resource" because the
/// present path pushes only what changed (see `flush_loop`); a full-surface
/// transfer at 10 Hz is ~160 MiB/s of DMA for a console that moves a few
/// hundred bytes per keystroke.
fn transfer_to_host_2d_rect(
    res: &GpuResource,
    offset: u64,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(), &'static str> {
    run_command(
        GPU_CMD_TRANSFER_TO_HOST_2D,
        "TRANSFER_TO_HOST_2D",
        |req| unsafe {
            cmd_write_u32(req, CMD_HDR_LEN, x);
            cmd_write_u32(req, CMD_HDR_LEN + 4, y);
            cmd_write_u32(req, CMD_HDR_LEN + 8, w); // rect.width
            cmd_write_u32(req, CMD_HDR_LEN + 12, h); // rect.height
            cmd_write_u64(req, CMD_HDR_LEN + 16, offset);
            cmd_write_u32(req, CMD_HDR_LEN + 24, res.id);
            cmd_write_u32(req, CMD_HDR_LEN + 28, 0); // struct padding
        },
    )
}

/// Full-surface transfer (the initial present and the fallback path).
fn transfer_to_host_2d(res: &GpuResource, offset: u64) -> Result<(), &'static str> {
    transfer_to_host_2d_rect(res, offset, 0, 0, res.w, res.h)
}

/// `RESOURCE_FLUSH` - push the host-side surface to the display. Without it a
/// successful transfer changes nothing the user can see: this is the command
/// that actually updates the scanout.
fn flush_rect(res: &GpuResource, x: u32, y: u32, w: u32, h: u32) -> Result<(), &'static str> {
    run_command(GPU_CMD_RESOURCE_FLUSH, "RESOURCE_FLUSH", |req| unsafe {
        cmd_write_u32(req, CMD_HDR_LEN, x);
        cmd_write_u32(req, CMD_HDR_LEN + 4, y);
        cmd_write_u32(req, CMD_HDR_LEN + 8, w); // rect.width
        cmd_write_u32(req, CMD_HDR_LEN + 12, h); // rect.height
        cmd_write_u32(req, CMD_HDR_LEN + 16, res.id);
        cmd_write_u32(req, CMD_HDR_LEN + 20, 0); // struct padding
    })
}

/// Full-surface flush.
fn flush(res: &GpuResource) -> Result<(), &'static str> {
    flush_rect(res, 0, 0, res.w, res.h)
}


// ---------------------------------------------------------------------------
// The present path: build the resource, verify the backing, take the scanout,
// then adopt the surface. Every failure is a logged `false`, never a panic.
// ---------------------------------------------------------------------------

/// Build a kernel-drawn virtio-gpu scanout and hand the console over to it.
///
/// `width`/`height` come from the mode M10a already installed, so the virtio
/// resource and the dispi surface agree on geometry. Returns `true` only when
/// the console is now drawn on the virtio surface; `false` means "keep the
/// console exactly where it was", which is the graceful-fallback contract the
/// whole GPU track is built on.
///
/// The order is deliberate and is the lesson of this increment:
///
/// 1. queue up (a queue that failed must not black the screen out),
/// 2. allocate + zero the backing,
/// 3. create + attach the resource,
/// 4. canary the backing BEFORE the scanout moves - a mismatch with the
///    display still on the dispi LFB is recoverable, the same mismatch after
///    `SET_SCANOUT` is a black screen,
/// 5. `SET_SCANOUT` + `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`,
/// 6. adopt the console LAST, once the device is demonstrably driving it.
pub fn present_bringup(width: usize, height: usize) -> bool {
    if !probe_ok() {
        log("[vgpu] present: no virtio-gpu transport - keeping the current console surface");
        return false;
    }
    if let Err(e) = queue_init() {
        log(&alloc::format!(
            "[vgpu] present: control queue setup failed: {e} - keeping the current console surface"
        ));
        return false;
    }

    // ---- virgl-capable devices keep the dispi console in stage 2 -----------
    // A device that advertises VIRGL routes SET_SCANOUT to its *virgl*
    // renderer, which resolves the resource in the 3D namespace. A plain
    // 2D resource (RESOURCE_CREATE_2D) is invisible there, so the device
    // answers `illegal resource specified <id>` and the scanout never moves.
    // Taking the scanout on such a device needs RESOURCE_CREATE_3D + a
    // virtio_gpu_set_scanout_3d-capable flow, which is M10b stage 3's job
    // (virgl/3D). Proceeding here would only produce a refused command after
    // an 8 MiB allocation, so the honest move now is to say so and keep the
    // dispi console - the graceful fallback this whole track is built on.
    if device().map(|v| v.virgl).unwrap_or(false) {
        log("[vgpu] present: device is virgl-capable - 2D scanout is owned by the \
              3D path (M10b stage 3); keeping the current console surface");
        return false;
    }

    // ---- backing: one contiguous span, zeroed ----------------------------
    // 1920x1080x32 = 8 100 KiB exactly = 2025 frames of the 512 MiB bump.
    // Contiguous by design: scatter-gather multi-entry backing is stage 3,
    // and one span keeps the device-side addressing trivially linear.
    let bytes = (width * height * 4) as u64;
    let frames = (bytes + 4095) / 4096;
    let kib = frames * 4;
    let Some(backing_phys) = crate::memory::alloc_contiguous(frames) else {
        log(&alloc::format!(
            "[vgpu] present: could not allocate {kib} KiB contiguous backing - keeping the current console surface"
        ));
        return false;
    };
    // Zero it: the device will read every byte of the first transfer, and an
    // uninitialized surface is both a privacy leak and a canary confound.
    unsafe { phys_write_bytes(backing_phys, bytes as usize) };

    let res = GpuResource {
        id: 1,
        w: width as u32,
        h: height as u32,
        format: GPU_FMT_B8G8R8X8,
        backing_phys,
        backing_bytes: bytes,
    };

    // ---- create + attach --------------------------------------------------
    if let Err(e) = create_2d(&res) {
        log(&alloc::format!(
            "[vgpu] present: RESOURCE_CREATE_2D failed: {e} - keeping the current console surface"
        ));
        return false;
    }
    if let Err(e) = attach_backing(&res) {
        log(&alloc::format!(
            "[vgpu] present: RESOURCE_ATTACH_BACKING failed: {e} - keeping the current console surface"
        ));
        return false;
    }
    log(&alloc::format!(
        "[vgpu] resource: created {width}x{height} B8G8R8X8 id={}, backing {kib} KiB at phys {backing_phys:#x} (1 entries)",
        res.id
    ));


    // ---- canary BEFORE the scanout moves ----------------------------------
    // Same pattern the dispi LFB canary uses, pointed at the resource backing
    // through the physical window: if this does not read back, the span is not
    // the memory the device will read and we must not point a scanout at it.
    let Some(backing_va) = phys_va(backing_phys) else {
        log("[vgpu] present: no physical-memory window - keeping the current console surface");
        return false;
    };
    let fb = unsafe { core::slice::from_raw_parts_mut(backing_va as *mut u8, bytes as usize) };
    let points = crate::gpu::canary_points(width, height);
    let (expected, actual) = crate::gpu::canary_roundtrip(fb, width, &points);
    if expected != actual {
        log(&alloc::format!(
            "[vgpu] canary: wrote {} pixels (expect sum {expected:#010x}), read back {actual:#010x} - MISMATCH (scanout left untouched)",
            points.len()
        ));
        return false;
    }
    log(&alloc::format!(
        "[vgpu] canary: wrote {} pixels (expect sum {expected:#010x}), read back {actual:#010x} - backing verified",
        points.len()
    ));

    // ---- take the scanout, push one frame --------------------------------
    if let Err(e) = set_scanout(&res, 0) {
        log(&alloc::format!(
            "[vgpu] present: SET_SCANOUT failed: {e} - keeping the current console surface"
        ));
        return false;
    }
    if let Err(e) = transfer_to_host_2d(&res, 0).and_then(|()| flush(&res)) {
        log(&alloc::format!(
            "[vgpu] present: initial transfer+flush failed: {e} - keeping the current console surface"
        ));
        return false;
    }
    log(&alloc::format!(
        "[vgpu] scanout: set_scanout ok, transfer+flush ok - display reads resource {} at {width}x{height}",
        res.id
    ));

    // ---- adopt LAST -------------------------------------------------------
    // The console renderer draws into this mapping; it must be a stable
    // 'static slice, and the adopt (clear + rescale + re-arm cursor) runs with
    // interrupts off: the timer must not preempt a half-drawn console.
    let info = FrameBufferInfo {
        byte_len: bytes as usize,
        width,
        height,
        pixel_format: PixelFormat::Bgr,
        bytes_per_pixel: 4,
        stride: width,
    };
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: the backing is a live, exclusively-owned allocation that
        // nothing else maps, and `adopt` is documented as task-safe with
        // interrupts disabled (it re-points the console at this surface).
        let fb: &'static mut [u8] =
            unsafe { core::slice::from_raw_parts_mut(backing_va as *mut u8, bytes as usize) };
        crate::framebuffer::adopt(fb, info);
    });
    let (cx, cy) = crate::framebuffer::cursor_pos();
    log(&alloc::format!(
        "[vgpu] present: console adopted the virtio surface (cursor re-armed at ({cx}, {cy}))"
    ));

    PRESENT.store(true, Ordering::Release);
    *RESOURCE.lock() = Some(res);
    log("[vgpu] present: flusher scheduled - damage-rect transfer+flush every 100 ms \
          (only what the console drew since the last push)");
    true
}


// ---------------------------------------------------------------------------
// The flusher: the GPU task BECOMES the present loop (no spawn)
// ---------------------------------------------------------------------------

/// Period between full-rect `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` pairs.
///
/// 100 ms = 10 fps of full-frame pushes at 8.1 MiB, which is plenty for a
/// text console and cheap enough not to starve the other tasks under TCG.
/// Damage rects (push only what changed) are the next increment - until then
/// correctness wins: every frame is complete, and the cost is visible only in
/// the `flush:` failure counters.
const FLUSH_PERIOD_MS: u64 = 100;

/// Consecutive failures after which the flusher gives up. The display is
/// frozen by then, but the KERNEL is not: the console keeps accepting input
/// and rendering into guest RAM, so a later recovery (or a debug shell) can
/// still see what happened. Giving up turns a silent 10 Hz retry storm into
/// one honest line and a stopped task.
const FLUSH_GIVE_UP: u32 = 50;

/// Report the damage-rect saving this often, in flush TICKS (not pushes): a
/// tick that found nothing dirty still counts, so an idle console still
/// produces reports and the idle-skip count is visible. At 10 Hz this is one
/// line every 5 s.
const FLUSH_REPORT_EVERY: u64 = 50;

/// Bytes pushed to the device since boot (guest→host transfers only), and how
/// many of those pushes were skipped because nothing was dirty.
///
/// This is the *measurement* the damage-rect change exists to produce: the
/// full-rect baseline is `flushes × width × height × 4`, so the counters make
/// the saving auditable on serial instead of estimated.
static FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
static FLUSH_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// The flusher loop. Called by the GPU task after a successful
/// `present_bringup`, and it never returns on the happy path.
///
/// This task does NOT exit while the console lives on the virtio surface: it
/// is the present loop. Spawning a second task would need its own lock
/// ordering story (queue lock vs. console lock) for no benefit - one task
/// alternating between "copy what the console drew" and "wait 100 ms" is the
/// whole job.
///
/// Only the *dirty* rectangle is pushed. The console accumulator is a
/// bounding box, so a burst of text between two flushes collapses into one
/// rect; an idle console produces no rect at all and the loop does nothing
/// but sleep. No per-flush logging: a 10 Hz success line would drown the
/// serial log, and a failure is logged ONCE (plus once more at the give-up
/// threshold).
pub fn flush_loop() {
    let mut failures: u32 = 0;
    let mut since_report: u64 = 0;
    // Snapshot of the cumulative counters at the last report, so each report
    // describes only its own window.
    let mut last_bytes: u64 = FLUSH_BYTES.load(Ordering::Relaxed);
    let mut last_skipped: u64 = FLUSH_SKIPPED.load(Ordering::Relaxed);
    loop {
        // Sleep first, flush second: the initial frame was already pushed by
        // `present_bringup`, so there is nothing to send right now.
        crate::scheduler::sleep_kernel(FLUSH_PERIOD_MS);
        if !PRESENT.load(Ordering::Acquire) {
            continue;
        }
        // Copy the resource out from under the lock so the queue lock is only
        // ever held inside the command builders, never across a sleep.
        let Some(res) = *RESOURCE.lock() else {
            continue;
        };
        // Count EVERY tick, including the idle ones: the report's saving figure
        // is only meaningful against the ticks it covers.
        since_report += 1;

        // Nothing drawn since the last push: skip both commands entirely.
        // This is the whole point of the damage rect - an idle console costs
        // zero DMA instead of a full 8.1 MiB surface.
        let Some((x0, y0, x1, y1)) = crate::framebuffer::take_dirty_rect() else {
            FLUSH_SKIPPED.fetch_add(1, Ordering::Relaxed);
            if since_report >= FLUSH_REPORT_EVERY {
                report_flush(&res, since_report, &mut last_bytes, &mut last_skipped);
                since_report = 0;
            }
            continue;
        };
        // Inclusive pixel bounds -> device rect dimensions, clamped to the
        // resource so a stale/oversized box can never address outside the
        // backing (the device would refuse the transfer, or worse, accept a
        // rect that runs off the end).
        let x0 = x0.min(res.w.saturating_sub(1) as usize);
        let y0 = y0.min(res.h.saturating_sub(1) as usize);
        let w = (x1.saturating_sub(x0) + 1).min(res.w as usize - x0);
        let h = (y1.saturating_sub(y0) + 1).min(res.h as usize - y0);
        if w == 0 || h == 0 {
            continue;
        }
        let (w32, h32) = (w as u32, h as u32);
        let (x32, y32) = (x0 as u32, y0 as u32);
        match transfer_to_host_2d_rect(&res, 0, x32, y32, w32, h32)
            .and_then(|()| flush_rect(&res, x32, y32, w32, h32))
        {
            Ok(()) => {
                FLUSH_BYTES.fetch_add((w * h * 4) as u64, Ordering::Relaxed);
                failures = 0;
                // M10b 3b latency probe: this damage-rect carried a cursor
                // move from the input IRQ; the delta from that stamp to now
                // (just after the device acknowledged both commands) is the
                // end-to-end input->present latency. If no move is pending
                // the take returns None and nothing is recorded.
                if let Some(input_ns) = crate::framebuffer::latency::take_pending() {
                    let now_ns = crate::time::now_ns();
                    if now_ns > input_ns {
                        crate::framebuffer::latency::record(now_ns - input_ns);
                    }
                }
            }
            Err(e) => {
                // Re-arm the damage: this rect was NOT presented, so if we
                // stop tracking it the pixels stay stale on screen forever.
                crate::framebuffer::mark_dirty_public(x32 as usize, y32 as usize, w32 as usize, h32 as usize);
                failures += 1;
                if failures == 1 {
                    log(&alloc::format!(
                        "[vgpu] flush: failed - console updates will lag ({e})"
                    ));
                } else if failures >= FLUSH_GIVE_UP {
                    log(&alloc::format!(
                        "[vgpu] flush: giving up after {FLUSH_GIVE_UP} consecutive failures - display frozen ({e})"
                    ));
                    return;
                }
            }
        }
        // One summary line per ~5 s: enough to see the working rate and the
        // damage-rect saving on a live boot without flooding the serial log.
        if since_report >= FLUSH_REPORT_EVERY {
            report_flush(&res, since_report, &mut last_bytes, &mut last_skipped);
            since_report = 0;
        }
    }
}

/// Emit one damage-rect report line for the window that just ended.
///
/// `ticks` is the number of flush ticks covered, `since_report` the window's
/// start snapshot of the cumulative counters. The baseline is what the SAME
/// number of pushes would have cost at full surface — the point of the line is
/// that saving, so it is computed from this window only (differencing the
/// process-wide counters) and never from cumulative totals.
fn report_flush(res: &GpuResource, ticks: u64, last_bytes: &mut u64, last_skipped: &mut u64) {
    let bytes_now = FLUSH_BYTES.load(Ordering::Relaxed);
    let skipped_now = FLUSH_SKIPPED.load(Ordering::Relaxed);
    let pushed = bytes_now - *last_bytes;
    let skipped = skipped_now - *last_skipped;
    let full = (res.w as u64) * (res.h as u64) * 4;
    // What the same number of pushes would have cost at full rect.
    let baseline = ticks * full;
    let saved = baseline.saturating_sub(pushed);
    log(&alloc::format!(
        "[vgpu] flush: {ticks} ticks, {skipped} idle-skips, \
         {} KiB pushed (full-rect would be {} KiB, saved {} KiB)",
        pushed >> 10,
        baseline >> 10,
        saved >> 10
    ));
    *last_bytes = bytes_now;
    *last_skipped = skipped_now;
    // Latency (M10b 3b): the number a latency-driven OS is judged by. Emitted
    // alongside the DMA line so the two are read together - a flush policy
    // that saves bandwidth but inflates input->present is a bad trade for a
    // game, and only a side-by-side line makes that visible.
    let lat = crate::framebuffer::latency::reset_window();
    if lat.samples == 0 {
        log("[vgpu] latency: no input samples this window (idle console)");
    } else {
        let avg = lat.sum_us / lat.samples;
        log(&alloc::format!(
            "[vgpu] latency: input->present n={} avg={} us min={} us max={} us",
            lat.samples,
            avg,
            if lat.min_us == u64::MAX { 0 } else { lat.min_us },
            lat.max_us
        ));
    }
}
