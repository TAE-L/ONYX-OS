//! M9.6-B1: PCI bus enumeration.
//!
//! Scans the PCI config space through the legacy CF8h/CFCh ports
//! (bus/device/function), decodes type-0 (endpoint) and type-1 (bridge)
//! headers, sizes the BARs (write-all-ones, read-back), and classifies each
//! function by class/subclass with a small vendor/device name table.
//!
//! The device list is kept for the shell's `lspci` command (SYS_LSPCI) and
//! for the M10 GPU track: the display controller's BARs are the framebuffer's
//! next address source, and M10a probes the display function's MEM BARs so the
//! driver knows the linear framebuffer's real size.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::port::Port;

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

const VENDOR_NONE: u16 = 0xFFFF;

/// Base class code of a display controller (VGA compatible subclass 0x00).
/// M10a drives the first function of this class found on the bus.
pub const CLASS_DISPLAY: u8 = 0x03;

/// One PCI function found on the bus.
#[derive(Clone, Copy)]
pub struct PciFunction {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    /// Up to 6 decoded BARs: (is_io, base, size_bytes). size 0 = absent.
    pub bars: [(bool, u32, u32); 6],
}

impl PciFunction {
    /// `00:02.0` style slot string.
    pub fn slot(&self) -> String {
        alloc::format!("{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }

    /// Human class name from the base class code.
    pub fn class_name(&self) -> &'static str {
        match self.class {
            0x00 => "legacy",
            0x01 => match self.subclass {
                0x00 | 0x05 => "ide",
                0x06 => "sata",
                _ => "storage",
            },
            0x02 => "network",
            0x03 => "display",
            0x04 => "multimedia",
            0x05 => "memory",
            0x06 => "bridge",
            0x08 => "generic",
            0x0C => match self.subclass {
                0x03 => "usb",
                _ => "serial-bus",
            },
            _ => "other",
        }
    }

    /// `8086:7010 (piix3-ide)` style identification string.
    pub fn identify(&self) -> String {
        let dev = device_name(self.vendor_id, self.device_id);
        alloc::format!("{:04x}:{:04x} {}", self.vendor_id, self.device_id, dev)
    }
}

fn device_name(vendor: u16, device: u16) -> &'static str {
    match (vendor, device) {
        (0x8086, 0x1237) => "440fx-host-bridge",
        (0x8086, 0x7000) => "piix3-isa",
        (0x8086, 0x7010) => "piix3-ide",
        (0x8086, 0x7113) => "piix4-pm",
        (0x8086, 0x100E) => "e1000",
        (0x1234, 0x1111) => "qemu-std-vga",
        (0x1AF4, _) => "virtio",
        (0x1022, _) => "amd",
        _ => "unknown",
    }
}

#[inline]
fn config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC);
    let mut a = Port::new(CONFIG_ADDR);
    let mut d = Port::new(CONFIG_DATA);
    unsafe {
        a.write(addr);
        d.read()
    }
}

/// Byte-granular config read — the virtio capability list (M10b) has
/// unaligned fields (cap type at +3, BAR index at +4, 32-bit offsets at
/// +5/+9), which the 4-byte-aligned `config_read_u32` cannot address.
#[inline]
fn config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let addr = 0x8000_0000u32
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC);
    let mut a = Port::new(CONFIG_ADDR);
    let mut d = Port::new(CONFIG_DATA);
    unsafe {
        a.write(addr);
        let v: u32 = d.read();
        (v >> (8 * u32::from(offset & 3))) as u8
    }
}

/// The byte-granular config reads M10b's virtio capability walk needs.
/// (M10a only needed BAR bases and the header/regs above, which are aligned.)
pub(crate) fn config_byte(f: &PciFunction, offset: u8) -> u8 {
    debug_assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "PCI config access with interrupts enabled (see M9.6-B1)"
    );
    config_read_u8(f.bus, f.device, f.function, offset)
}

#[inline]
fn config_write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let addr = 0x8000_0000u32
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC);
    let mut a = Port::new(CONFIG_ADDR);
    let mut d = Port::new(CONFIG_DATA);
    unsafe {
        a.write(addr);
        d.write(value);
    }
}

#[inline]
fn config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let v = config_read_u32(bus, device, function, offset & 0xFC);
    ((v >> (u32::from(offset & 2) * 8)) & 0xFFFF) as u16
}

/// M10b stage 2: read-modify-write OR `mask` into the 16-bit register at
/// `offset` (the PCI *command* register at +0x04 for bus-master enable) and
/// return the value read back after the write.
///
/// The RMW goes through the aligned u32 at `offset & 0xFC`, so for the command
/// register this dword also contains the *status* register — writing its
/// RW1C bits back is harmless (a 1 to an already-clear RW1C bit does nothing;
/// same benign side-effect class as `size_bar`'s own RMW).
///
/// # Safety contract (callers): interrupts MUST be disabled — the config port
/// is the two-step CF8h/CFCh handshake (M9.6-B1). Hard guard below, like
/// `config_byte`: the caller loses the write instead of the machine.
pub(crate) fn config_or_u16(f: &PciFunction, offset: u8, mask: u16) -> u16 {
    assert!(
        offset & 2 == 0,
        "config_or_u16 expects a 2-byte-aligned register offset"
    );
    if x86_64::instructions::interrupts::are_enabled() {
        let msg = alloc::format!(
            "[pci] refused command |= {mask:#06x} with interrupts enabled \\\n             (two-step config access) - see M9.6-B1\n"
        );
        crate::serial_writeln!("{}", msg.trim_end());
        crate::framebuffer::console_bytes(msg.as_bytes());
        return 0;
    }
    let base = offset & 0xFC;
    let raw = config_read_u32(f.bus, f.device, f.function, base);
    let new = raw | u32::from(mask);
    config_write_u32(f.bus, f.device, f.function, base, new);
    (config_read_u32(f.bus, f.device, f.function, base) & 0xFFFF) as u16
}

/// Size a 32-bit MEM BAR: write all-ones, read back, restore.
/// Returns (base, size) ??? size 0 for an absent/unavailable BAR.
///
/// DANGER: only MEM-type BARs are probed. Probing IO-type BARs is
/// destructive ??? the PIIX3 IDE's IO BARs alias our LIVE ATA ports (0x1F0),
/// and write-0xFFFFFFFF to them wedges the drive (observed). IO BARs are
/// reported base-only (size = UNSIZED marker).
fn size_bar(bus: u8, device: u8, function: u8, bar_off: u8) -> (u32, u32) {
    let base = config_read_u32(bus, device, function, bar_off);
    config_write_u32(bus, device, function, bar_off, 0xFFFF_FFFF);
    let mask = config_read_u32(bus, device, function, bar_off);
    config_write_u32(bus, device, function, bar_off, base);
    if mask == 0 {
        return (base, 0);
    }
    let size_bits = mask & 0xFFFF_FFF0;
    let size = size_bits.wrapping_neg();
    (base, size)
}

/// Marker size for a present IO-type BAR whose real size we do not probe
/// (see `size_bar` ??? probing IO BARs can wedge the live ATA drive).
pub const UNSIZED: u32 = 0xFFFF_FFFF;

/// Enumerate the whole bus and return every function found.
pub fn enumerate() -> Vec<PciFunction> {
    let mut out = Vec::new();
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            let f0 = config_read_u16(bus, device, 0, 0x00);
            if f0 == VENDOR_NONE {
                continue;
            }
            // Because this runs with IF=0, a slow device can't starve the
            // timer; but a WEDGED config port would still hang boot. Bound
            // each device by re-reading its vendor: sane hardware answers
            // instantly. (QEMU PIIX never wedges under IF=0.)
            let _ = f0;
            let multi = config_read_u32(bus, device, 0, 0x0C) & (1 << 23) != 0;
            let funcs = if multi { 8 } else { 1 };
            for function in 0..funcs {
                let vendor = config_read_u16(bus, device, function, 0x00);
                if vendor == VENDOR_NONE {
                    continue;
                }
                let device_id = config_read_u16(bus, device, function, 0x02);
                let class_rev = config_read_u32(bus, device, function, 0x08);
                let class = (class_rev >> 24) as u8;
                let subclass = (class_rev >> 16) as u8;
                let prog_if = (class_rev >> 8) as u8;
                let header_type =
                    (config_read_u32(bus, device, function, 0x0C) >> 16) as u8 & 0x7F;

                let mut bars = [(false, 0u32, 0u32); 6];
                // BAR *bases* are read, never written ??? except for display
                // devices, whose MEM BARs are sized by M10a below. (The
                // config-space size probe's writes proved unsafe in general:
                // the PIIX IDE's IO BARs alias the live ATA ports, so probing
                // an IO BAR wedges the drive.)
                for i in 0..6u8 {
                    let off = 0x10 + i * 4;
                    let raw = config_read_u32(bus, device, function, off);
                    if raw & !0x3 != 0 {
                        let is_io = raw & 1 != 0;
                        bars[i as usize] = (is_io, raw & !0xFFF, UNSIZED);
                    }
                }
                // M10a: the GPU driver needs the framebuffer BAR's *real* size
                // (to map exactly the mode's surface, no more). Only the
                // display function's MEM BARs are ever probed, and only with
                // interrupts disabled: the config port is a two-step
                // (select/read) handshake that a preemption between the two
                // halves wedges for the rest of the boot ??? the reason the whole
                // scan runs before `interrupts::enable()`. The IF check makes
                // that invariant self-enforcing instead of a comment.
                if class == CLASS_DISPLAY && !x86_64::instructions::interrupts::are_enabled() {
                    for i in 0..6usize {
                        if bars[i].2 != UNSIZED {
                            continue; // absent BAR
                        }
                        let off = 0x10 + i as u8 * 4;
                        let raw = config_read_u32(bus, device, function, off);
                        if raw & 1 != 0 {
                            continue; // IO BAR: never probe (see above)
                        }
                        if raw & 0b110 == 0b100 {
                            continue; // 64-bit BAR pair: not handled yet
                        }
                        let (_, size) = size_bar(bus, device, function, off);
                        if size != 0 {
                            bars[i].2 = size;
                        }
                    }
                }
                let _ = header_type;

                out.push(PciFunction {
                    bus,
                    device,
                    function,
                    vendor_id: vendor,
                    device_id,
                    class,
                    subclass,
                    prog_if,
                    bars,
                });

                // B1 stays single-bus: no descent behind bridges.
                let _ = header_type;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Kernel-side registry + pretty printer (serial + framebuffer console)
// ---------------------------------------------------------------------------

static DEVICES: Mutex<Vec<PciFunction>> = Mutex::new(Vec::new());

fn bar_str(f: &PciFunction, i: usize) -> String {
    let (is_io, base, size) = f.bars[i];
    if size == 0 {
        return String::new();
    }
    let kind = if is_io { "io" } else { "mem" };
    if size == UNSIZED {
        alloc::format!(" bar{}={}: {:08x}/?", i, kind, base)
    } else if size >= 0x100_000 {
        alloc::format!(" bar{}={}: {:08x}/{}M", i, kind, base, size >> 20)
    } else {
        alloc::format!(" bar{}={}: {:04x}/{}B", i, kind, base, size)
    }
}

/// Print one device line to serial AND the framebuffer console.
fn print_device(f: &PciFunction) {
    let mut line = alloc::format!(
        "[pci] {} {} class={:02x}{:02x}({})",
        f.slot(),
        f.identify(),
        f.class,
        f.subclass,
        f.class_name()
    );
    for i in 0..6 {
        if f.bars[i].2 != 0 {
            line.push_str(&bar_str(f, i));
        }
    }
    crate::serial_writeln!("{}", line);
    crate::framebuffer::console_bytes(line.as_bytes());
    crate::framebuffer::console_bytes(b"\n");
}

/// Scan the bus, print everything, keep the registry. Call once at boot.
pub fn init() {
    crate::serial_writeln!("[pci] enumerating PCI bus 0..255 ...");
    let devices = enumerate();
    let count = devices.len();
    for f in &devices {
        print_device(f);
    }
    crate::serial_writeln!("[pci] {} functions found", count);
    crate::framebuffer::console_bytes(
        alloc::format!("[pci] {} pci functions found\n", count).as_bytes(),
    );
    // M10a: the GPU track asks the registry for the display function itself
    // (`find_display`); a machine with no display controller is the "*no device*"
    // case there, which reads very differently from "device present but
    // modesetting failed" (a real bug) — the M10a bug log has that story.
    *DEVICES.lock() = devices;
}

/// Render every device as text lines (SYS_LSPCI / shell `lspci`).
pub fn render_lines() -> Vec<String> {
    let g = DEVICES.lock();
    let mut out = Vec::new();
    for f in g.iter() {
        let mut line = alloc::format!(
            "[pci] {} {} class={:02x}{:02x}({})",
            f.slot(),
            f.identify(),
            f.class,
            f.subclass,
            f.class_name()
        );
        for i in 0..6 {
            if f.bars[i].2 != 0 {
                line.push_str(&bar_str(f, i));
            }
        }
        out.push(line);
    }
    out
}

/// M10a: the display function the GPU driver owns.
///
/// Prefers the VGA-compatible subclass (0x00) ??? that is the one with the
/// legacy VBE/dispi register window QEMU's `std`, `bochs-display` and
/// `virtio-vga` expose ??? then falls back to any other display subclass.
/// `None` when the machine has no display device at all (`-vga none`).
pub fn find_display() -> Option<PciFunction> {
    let g = DEVICES.lock();
    let mut other: Option<PciFunction> = None;
    for f in g.iter() {
        if f.class != CLASS_DISPLAY {
            continue;
        }
        if f.subclass == 0x00 {
            return Some(*f);
        }
        if other.is_none() {
            other = Some(*f);
        }
    }
    other
}

/// M10a: the function's framebuffer BAR ??? the first MEM-type BAR with a real
/// size (QEMU's VGA puts the linear framebuffer in BAR0). Returns
/// `(index, base, size)`.
pub fn framebuffer_bar(f: &PciFunction) -> Option<(usize, u32, u32)> {
    for i in 0..6usize {
        let (is_io, base, size) = f.bars[i];
        if !is_io && base != 0 && size != UNSIZED && size != 0 {
            return Some((i, base, size));
        }
    }
    None
}

/// M10a: re-read one BAR's base address straight from config space.
///
/// The registry's copy is a boot-time snapshot; the GPU driver needs the
/// *current* value after programming a mode (a mode switch may move or resize
/// the VGA window). Returns 0 if the BAR is absent or the offset is invalid.
///
/// # Safety contract (callers): interrupts MUST be disabled — the config port is
/// the two-step CF8h/CFCh handshake that a preemption between the two halves
/// wedges for the rest of the boot (PLAN.md M9.6-B1). The check below is a hard
/// guard, not a comment: a caller that gets it wrong loses the read instead of
/// the machine.
pub fn read_bar_base(f: &PciFunction, index: usize) -> u32 {
    if index > 5 {
        return 0;
    }
    if x86_64::instructions::interrupts::are_enabled() {
        // Do not touch the config port from a preemptible context (M9.6-B1).
        // Make it loud on the console as well as serial: a silently-zero BAR
        // would look like "no framebuffer" downstream and hide the real bug.
        let msg = alloc::format!(
            "[pci] refused BAR{index} read with interrupts enabled (two-step \
             config access) - see M9.6-B1\n"
        );
        crate::serial_writeln!("{}", msg.trim_end());
        crate::framebuffer::console_bytes(msg.as_bytes());
        return 0;
    }
    let off = 0x10 + index as u8 * 4;
    let raw = config_read_u32(f.bus, f.device, f.function, off);
    if raw & 1 != 0 {
        raw & !0x3 // IO BAR: 4-byte granularity
    } else {
        raw & !0xF // MEM BAR: 16-byte granularity
    }
}

/// Human size for log lines ("16 MiB" / "4 KiB" / "512 B").
pub fn size_str(size: u32) -> alloc::string::String {
    if size >= 0x10_0000 {
        alloc::format!("{} MiB", size >> 20)
    } else if size >= 0x400 {
        alloc::format!("{} KiB", size >> 10)
    } else {
        alloc::format!("{} B", size)
    }
}

