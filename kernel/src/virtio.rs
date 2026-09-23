//! M10b: virtio-gpu transport probe — the modern display backend's first step.
//!
//! QEMU's virtio-vga wraps a **virtio-gpu** device (PCI 1af4:1050, modern
//! virtio 1.0) behind a VGA-compatible shell (that shell is what lets SeaBIOS
//! boot it at all: there is no VGA BIOS for bare `virtio-gpu-pci`). The modern
//! interface lives in the PCI *capability list*: vendor-specific capabilities
//! (id 0x09) each describing one config structure — common (feature
//! negotiation, queues), notify (doorbells), ISR, and the GPU-specific device
//! config (`num_scanouts`).
//!
//! This module walks that capability list, maps the config pages, negotiates
//! ACKNOWLEDGE|DRIVER and reads the device identity/features. That is the
//! foundation everything else in M10b builds on (virtqueues next, then DMA
//! resources and scanout).
//!
//! Conventions inherited from M10a: all access happens in the boot path
//! (IF=0, before any task exists — the PCI config port and the two-step
//! register accesses must not be preempted); every failure is a logged,
//! counted fallback, never a panic; serial lines are one-shot summaries.
//!
//! Log prefix: `[vgpu]` (the `[gpu]` prefix stays the display-backend story as
//! a whole — dispi lines keep it; these lines are the virtio transport's own).

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;
use x86_64::structures::paging::{
    mapper::Translate, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB,
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
/// `DRIVER_OK` (4) is deliberately not set yet: it means "queues are up and I
/// am ready for commands", which is the next M10b increment. `FEATURES_OK` (8)
/// is the honest state a driver that has finished feature negotiation sits in.
const ST_RESET: u8 = 0;
const ST_ACKNOWLEDGE: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_FEATURES_OK: u8 = 8;

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
    let common = common_va.as_u64() as *mut u8;
    let rd8 = |off: usize| unsafe { read_volatile(common.add(off)) };
    let rd16 = |off: usize| unsafe { read_volatile(common.add(off) as *const u16) };
    let rd32 = |off: usize| unsafe { read_volatile(common.add(off) as *const u32) };
    let wr8 = |off: usize, v: u8| unsafe { write_volatile(common.add(off), v) };
    let wr32 = |off: usize, v: u32| unsafe { write_volatile(common.add(off) as *mut u32, v) };

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
         events_clear={events_clear:#010x} - command path lands in the next M10b \
         increment"
    ));
    *DEVICE.lock() = Some(info);
    PROBE_OK.store(true, core::sync::atomic::Ordering::Release);
    Some(info)
}
