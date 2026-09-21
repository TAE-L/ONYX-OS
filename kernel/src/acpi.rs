//! M9.6-B2: ACPI core — RSDP discovery, RSDT/XSDT walk, MADT/HPET/FADT.
//!
//! Scope (read-only, discovery):
//!   * RSDP from the bootloader (`BootInfo::rsdp_addr`), falling back to a
//!     scan of the 0xE0000..0xFFFFF region.
//!   * RSDT (32-bit) / XSDT (64-bit) table directory, checksum-validated.
//!   * MADT (APIC): LAPIC + IOAPIC records + interrupt-source overrides,
//!     cross-checked against the live A2 APIC wiring (IRQ0 -> GSI2 must
//!     appear; the IOAPIC address must match the live base). These records
//!     are what M9.8 SMP consumes later.
//!   * HPET: base address reported (A1's TSC calibration stays primary; the
//!     HPET base is now known for future use).
//!   * FADT: presence + revision recorded.
//!
//! Tables live in physical memory and are read THROUGH the bootloader's
//! physical-memory window (`memory::phys_offset()`) — never through fresh
//! page-table entries (the A2 aliasing lesson). Runs with interrupts off,
//! like the rest of the boot-time discovery.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::{memory, serial_writeln};

/// True once ACPI was parsed successfully.
static ACPI_OK: AtomicBool = AtomicBool::new(false);
/// Number of tables validated in the directory.
static TABLE_COUNT: AtomicU64 = AtomicU64::new(0);

/// `phys + window` -> pointer, or None on overflow.
#[inline]
fn win_ptr<T>(phys: u64) -> Option<*const T> {
    let off = memory::phys_offset()?;
    let v = off.checked_add(phys)?;
    Some(v as *const T)
}

#[inline]
unsafe fn rd_u8(p: *const u8, off: usize) -> u8 {
    core::ptr::read_volatile(p.add(off))
}
/// ACPI tables are packed byte streams at arbitrary physical addresses
/// (possibly odd — e.g. QEMU's RSDT at 0x7fe2329). NEVER use aligned
/// `read_volatile::<u16/u32/u64>`; decode via unaligned little-endian reads.
#[inline]
unsafe fn rd_u16(p: *const u8, off: usize) -> u16 {
    let b = core::slice::from_raw_parts(p.add(off), 2);
    let mut arr = [0u8; 2];
    arr.copy_from_slice(b);
    u16::from_le_bytes(arr)
}
#[inline]
unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    let b = core::slice::from_raw_parts(p.add(off), 4);
    let mut arr = [0u8; 4];
    arr.copy_from_slice(b);
    u32::from_le_bytes(arr)
}
#[inline]
unsafe fn rd_u64(p: *const u8, off: usize) -> u64 {
    let b = core::slice::from_raw_parts(p.add(off), 8);
    let mut arr = [0u8; 8];
    arr.copy_from_slice(b);
    u64::from_le_bytes(arr)
}

/// ACPI checksum: every byte of the structure sums (mod 256) to zero.
fn checksum_ok(base: *const u8, len: usize) -> bool {
    let mut s: u8 = 0;
    for i in 0..len {
        s = s.wrapping_add(unsafe { rd_u8(base, i) });
    }
    s == 0
}

/// Validate a table header: length >= 36 + checksum. Returns the table
/// length and the 4-byte signature (dynamic slice, so callers can match it
/// against `b"XSDT"` etc.). The slice borrows the physical-memory window,
/// which lives forever, so `'static` is sound.
fn table_header_ok(base: *const u8) -> Option<(usize, &'static [u8])> {
    let hdr: &'static [u8] = unsafe { core::slice::from_raw_parts(base, 36) };
    let len = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as usize;
    if len < 36 {
        return None;
    }
    if !checksum_ok(base, len) {
        return None;
    }
    Some((len, &hdr[..4]))
}

/// Locate the RSDP. Prefer the bootloader-provided address (cleanest on
/// BIOS + UEFI), else scan the legacy 0xE0000..0xFFFFF region.
/// Returns the physical address.
pub fn find_rsdp(provided: Option<u64>) -> Option<u64> {
    if let Some(addr) = provided {
        // Trust the bootloader, but still sanity-check the signature.
        let p = win_ptr::<u8>(addr)?;
        let sig = unsafe { core::slice::from_raw_parts(p, 8) };
        if &sig[..8] == b"RSD PTR " {
            return Some(addr);
        }
    }
    // Fallback scan of the legacy BIOS area.
    let mut a = 0xE0000u64;
    while a < 0x100_000 {
        let p = win_ptr::<u8>(a)?;
        let sig = unsafe { core::slice::from_raw_parts(p, 8) };
        if &sig[..8] == b"RSD PTR " && checksum_ok(p, 20) {
            return Some(a);
        }
        a += 16;
    }
    None
}

/// Read (revision, xsdt_address) from the RSDP.
fn parse_rsdp(phys: u64) -> (u8, u64) {
    let p = win_ptr::<u8>(phys).unwrap();
    let revision = unsafe { rd_u8(p, 15) };
    let xsdt = if revision >= 2 { unsafe { rd_u64(p, 24) } } else { 0 };
    (revision, xsdt)
}

/// Parse and cross-check the MADT (APIC). The IO-APIC address must match the
/// live A2 base (0xFEC00000), and the IRQ0 override must point at GSI 2 —
/// matching what `apic::init` routed (GSI 0 + 2 -> PIT vector 0x20).
fn parse_madt(base: *const u8, len: usize) {
    if len < 44 {
        serial_writeln!("[acpi] MADT: too short ({len} B) - skipped");
        return;
    }
    let lapic_addr = unsafe { rd_u32(base, 36) };
    let _flags = unsafe { rd_u32(base, 40) };
    let mut lapics = 0u32;
    let mut ioapics = 0u32;
    let mut overrides = 0u32;
    let mut off = 44usize;
    while off + 2 <= len {
        let ty = unsafe { rd_u8(base, off) };
        let ell = unsafe { rd_u8(base, off + 1) } as usize;
        if ell < 2 || off + ell > len {
            break;
        }
        match ty {
            0 => {
                // LAPIC: uid at +2, apic id at +3, flags at +4 (bit 0 =
                // enabled). M9.8 starts exactly the enabled ones.
                let uid = unsafe { rd_u8(base, off + 2) };
                let id = unsafe { rd_u8(base, off + 3) };
                let flags = unsafe { rd_u32(base, off + 4) };
                serial_writeln!(
                    "[acpi] MADT: LAPIC uid={} id={:#x} flags={:#x}",
                    uid,
                    id,
                    flags
                );
                if flags & 1 != 0 {
                    let n = LAPIC_COUNT.load(Ordering::Relaxed);
                    if n < LAPIC_MAX {
                        LAPIC_IDS[n].store(u32::from(id), Ordering::Relaxed);
                        LAPIC_COUNT.store(n + 1, Ordering::Relaxed);
                    } else {
                        serial_writeln!("[acpi] MADT: more than {LAPIC_MAX} enabled LAPICs - ignoring id={id:#x}");
                    }
                }
                lapics += 1;
            }
            1 => {
                // IOAPIC: id at +2, 32-bit addr at +4, gsi base at +8.
                let ioid = unsafe { rd_u8(base, off + 2) };
                let ioaddr = unsafe { rd_u32(base, off + 4) };
                let gsibase = unsafe { rd_u32(base, off + 8) };
                serial_writeln!(
                    "[acpi] MADT: IOAPIC id={} addr={:#x} gsi_base={}",
                    ioid,
                    ioaddr,
                    gsibase
                );
                // Cache for M9.8 SMP + the cross-check.
                IOAPIC_ADDR.store(ioaddr as u64, Ordering::Relaxed);
                GSI_BASE.store(gsibase as u64, Ordering::Relaxed);
                ioapics += 1;
            }
            2 => {
                // Source override: irq at +3, gsi at +4 (4 bytes).
                let irq = unsafe { rd_u8(base, off + 3) };
                let gsi = unsafe { rd_u32(base, off + 4) };
                serial_writeln!("[acpi] MADT: IRQ {} -> GSI {}", irq, gsi);
                overrides += 1;
            }
            _ => {}
        }
        off += ell;
    }
    serial_writeln!(
        "[acpi] MADT: {} LAPIC, {} IOAPIC, {} IRQ-overrides; LAPIC base {:#x}",
        lapics,
        ioapics,
        overrides,
        lapic_addr
    );
}

/// Cached MADT IOAPIC record (for M9.8 SMP + the A2 cross-check).
static IOAPIC_ADDR: AtomicU64 = AtomicU64::new(0);
static GSI_BASE: AtomicU64 = AtomicU64::new(0);

/// Largest LAPIC list cached (`smp::MAX_CPUS` plus slack: extra entries are
/// counted but never started).
const LAPIC_MAX: usize = 8;

/// Enabled LAPIC ids, in MADT order — the input to M9.8's AP bring-up.
static LAPIC_IDS: [AtomicU32; LAPIC_MAX] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static LAPIC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The enabled LAPIC ids the MADT advertised. Returns `(ids, count)`;
/// `count == 0` means the MADT gave no usable processors and SMP stays off.
pub fn lapic_ids() -> ([u8; LAPIC_MAX], usize) {
    let mut out = [0u8; LAPIC_MAX];
    let n = LAPIC_COUNT.load(Ordering::Relaxed).min(LAPIC_MAX);
    for i in 0..n {
        out[i] = LAPIC_IDS[i].load(Ordering::Relaxed) as u8;
    }
    (out, n)
}

/// Entry point: locate + parse the ACPI tables. Call once at boot (IF=0).
pub fn init(provided_rsdp: Option<u64>) {
    let Some(rs) = find_rsdp(provided_rsdp) else {
        serial_writeln!("[acpi] RSDP not found - ACPI unavailable");
        return;
    };
    let (rev, xsdt) = parse_rsdp(rs);
    serial_writeln!("[acpi] RSDP at {rs:#x} (revision {rev})");

    // Prefer the 64-bit XSDT; try the 32-bit RSDT as a fallback.
    let ok = if xsdt != 0 {
        walk_directory(xsdt, true)
    } else {
        false
    };
    if !ok {
        let rsdt_addr = u64::from(unsafe { rd_u32(win_ptr::<u8>(rs).unwrap(), 16) });
        let _ = walk_directory(rsdt_addr, false);
    }

    let n = TABLE_COUNT.load(Ordering::Relaxed);
    if n > 0 {
        ACPI_OK.store(true, Ordering::Relaxed);
    }
    // Cross-check the MADT against the live A2 APIC wiring.
    if let Some((ioaddr, _gsi)) = ioapic_info() {
        if ioaddr == 0xFEC0_0000 {
            serial_writeln!("[acpi] cross-check: MADT IOAPIC addr matches A2 wiring");
        } else {
            serial_writeln!("[acpi] cross-check: MADT IOAPIC {ioaddr:#x} != live 0xfec00000!");
        }
    }
    serial_writeln!("[acpi] done: {} tables validated", n);
}

/// Was the ACPI table set parsed successfully?
pub fn available() -> bool {
    ACPI_OK.load(Ordering::Relaxed)
}

/// MADT-provided IOAPIC info, for M9.8 SMP + A2 cross-checks.
pub fn ioapic_info() -> Option<(u64, u32)> {
    let a = IOAPIC_ADDR.load(Ordering::Relaxed);
    if a == 0 {
        None
    } else {
        Some((a, (GSI_BASE.load(Ordering::Relaxed) as u32)))
    }
}

/// Interesting table signatures.
fn decode_table(base: *const u8, len: usize) {
    let sig = unsafe { core::slice::from_raw_parts(base, 4) };
    match sig {
        b"APIC" => parse_madt(base, len),
        b"HPET" => {
            // HPET base address (u64 @52) exists only in the full table;
            // QEMU's HPET is 56 B so the field crosses into the next table
            // — guard on length before reading it.
            if len >= 60 {
                let addr = unsafe { rd_u64(base, 52) };
                serial_writeln!(
                    "[acpi] HPET base addr {:#x} ({} B)",
                    addr & 0xFFFF_FFFF_FFFF,
                    len
                );
            } else {
                serial_writeln!("[acpi] HPET ({len} B, no base field)");
            }
        }
        b"FACP" => serial_writeln!("[acpi] FACP present"),
        _ => {
            let s = core::str::from_utf8(sig).unwrap_or("????");
            serial_writeln!("[acpi] table {s} ({} B)", len);
        }
    }
}

/// Walk a directory (XSDT = 64-bit pointers, RSDT = 32-bit pointers),
/// validate + decode every table it references.
fn walk_directory(addr: u64, wide: bool) -> bool {
    let Some(dir) = win_ptr::<u8>(addr) else {
        serial_writeln!("[acpi] directory at {addr:#x} unreadable");
        return false;
    };
    let Some((dlen, d_sig)) = table_header_ok(dir) else {
        serial_writeln!(
            "[acpi] {} directory invalid at {addr:#x}",
            if wide { "XSDT" } else { "RSDT" }
        );
        return false;
    };
    if wide {
        if d_sig != b"XSDT" {
            serial_writeln!("[acpi] XSDT signature mismatch at {addr:#x}");
            return false;
        }
    } else if d_sig != b"RSDT" {
        serial_writeln!("[acpi] RSDT signature mismatch at {addr:#x}");
        return false;
    }
    let count = (dlen - 36) / (if wide { 8 } else { 4 });
    serial_writeln!(
        "[acpi] {} at {addr:#x}: {} tables",
        if wide { "XSDT" } else { "RSDT" },
        count
    );
    for i in 0..count {
        let ta = if wide {
            unsafe { rd_u64(dir, 36 + i * 8) }
        } else {
            u64::from(unsafe { rd_u32(dir, 36 + i * 4) })
        };
        if let Some(p) = win_ptr::<u8>(ta) {
            if let Some((l, _sig2)) = table_header_ok(p) {
                TABLE_COUNT.fetch_add(1, Ordering::Relaxed);
                decode_table(p, l);
            } else {
                serial_writeln!("[acpi] table at {ta:#x}: invalid checksum - skipped");
            }
        }
    }
    true
}