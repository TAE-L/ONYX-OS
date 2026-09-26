//! M5: block layer — partition table scanning (MBR + GPT) over the ATA drive.
//!
//! The boot image (`bios.img`) contains a classic MBR with two partitions:
//!   1. the bootloader's own raw area (stage-2 + kernel, type 0x20),
//!   2. a FAT32 partition (type 0x0C) where the bootloader keeps the kernel
//!      ELF — from M6 we mount it read/write.
//! GPT parsing is implemented for the M9 UEFI images (which use a GPT).

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

use crate::ata::{AtaDrive, DriveInfo, SECTOR_SIZE};
use crate::serial_writeln;

/// M10b P3: sanity bounds for a GPT partition-entry array read off the disk.
/// These bound the disk-controlled `entry_size` / `entry_count` so a corrupt
/// header cannot drive a slice out of range (a kernel panic under
/// `panic = abort`). The minimum is the UEFI entry layout size (128 bytes: the
/// 72-byte name at offset 56..128 must fit); the maxima are generous well
/// beyond any real disk but still bound the allocation.
const GPT_ENTRY_MIN: usize = 128;
const GPT_ENTRY_MAX: usize = 4096;
const GPT_MAX_ENTRIES: u32 = 4096;
/// Cap on the entry-array read: 4096 sectors = 2 MiB.
const GPT_MAX_SECTORS: u64 = 4096;

/// The (single) boot disk. `None` until `init()` succeeds.
static DISK: Mutex<Option<Disk>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
pub enum Layout {
    Mbr,
    Gpt,
}

pub struct Partition {
    pub start_lba: u64,
    pub sectors: u64,
    /// Human-readable type: MBR type byte or GPT type GUID.
    pub kind: String,
    /// GPT partition name (UTF-16), if present.
    pub name: Option<String>,
}

pub struct Disk {
    pub drive: AtaDrive,
    pub info: DriveInfo,
    pub layout: Layout,
    pub partitions: Vec<Partition>,
    /// GPT disk GUID (text form), empty for MBR.
    pub disk_guid: String,
}

/// Run `f` with exclusive access to the boot disk (used by M6+ syscalls).
pub fn with_disk<T>(f: impl FnOnce(&mut Disk) -> T) -> Option<T> {
    let mut guard = DISK.lock();
    guard.as_mut().map(f)
}

pub fn init() {
    let (mut drive, info) = match AtaDrive::new_primary_master() {
        Ok(x) => x,
        Err(e) => {
            serial_writeln!("M5: ATA probe failed: {e:?}");
            return;
        }
    };
    serial_writeln!(
        "M5: ATA primary master: model=\"{}\" sectors={}",
        info.model,
        info.total_sectors
    );

    // Stability check: read LBA0 twice; the bytes must match exactly.
    // Uses the RAW path (bypasses the A5 cache) so the check is genuine.
    let mut a = [0u8; SECTOR_SIZE];
    let mut b = [0u8; SECTOR_SIZE];
    let read_ok = drive.read_sectors_raw(0, 1, &mut a).is_ok()
        && drive.read_sectors_raw(0, 1, &mut b).is_ok()
        && a == b;
    if !read_ok {
        serial_writeln!("M5: read-verify FAILED (LBA0 unstable)");
        return;
    }
    serial_writeln!("M5: read-verify PASSED (LBA0 stable across 2 reads)");

    // M10b P3: prove the GPT geometry validation rejects hostile on-disk values
    // without panicking. Runs on every boot (deterministic, no disk access) so
    // a regression here aborts the kernel and is caught immediately.
    gpt_selftest();

    if a[510] != 0x55 || a[511] != 0xAA {
        serial_writeln!("M5: LBA0 has no 0x55AA signature — no partition table");
        return;
    }

    let (layout, partitions, disk_guid) = if a[446 + 4] == 0xEE {
        scan_gpt(&mut drive, &a)
    } else {
        (Layout::Mbr, scan_mbr(&a), String::new())
    };

    serial_writeln!("M5: layout={layout:?} partitions={}", partitions.len());
    for (i, p) in partitions.iter().enumerate() {
        serial_writeln!(
            "M5:  part{}: {} lba={}..{} ({} sectors){}",
            i + 1,
            p.kind,
            p.start_lba,
            p.start_lba + p.sectors,
            p.sectors,
            p.name
                .as_deref()
                .map(|n| alloc::format!(" name=\"{n}\""))
                .unwrap_or_default()
        );
    }

    // Exercise the PIO write path safely: rewrite the disk's last sector with
    // its own bytes and verify. Byte-identical, so nothing can be corrupted;
    // the QEMU test image is a throwaway copy anyway.
    let write_ok = if info.total_sectors == 0 {
        false
    } else {
        let last = (info.total_sectors - 1).min(u64::from(0x0FFF_FFFFu32)) as u32;
        let mut before = [0u8; SECTOR_SIZE];
        let mut after = [0u8; SECTOR_SIZE];
        drive.read_sectors_raw(last, 1, &mut before).is_ok()
            && drive.write_sectors_raw(last, 1, &before).is_ok()
            && drive.read_sectors_raw(last, 1, &mut after).is_ok()
            && before == after
    };
    serial_writeln!(
        "M5: write-verify {}",
        if write_ok {
            "PASSED (byte-identical rewrite)"
        } else {
            "FAILED"
        }
    );

    *DISK.lock() = Some(Disk {
        drive,
        info,
        layout,
        partitions,
        disk_guid,
    });
    serial_writeln!("M5: block layer ready");
}

/// Parse the 4 classic MBR entries in sector 0.
fn scan_mbr(mbr: &[u8; SECTOR_SIZE]) -> Vec<Partition> {
    let mut out = Vec::new();
    for i in 0..4 {
        let e = &mbr[446 + i * 16..446 + i * 16 + 16];
        let ptype = e[4];
        if ptype == 0 || ptype == 0xEE {
            continue;
        }
        out.push(Partition {
            start_lba: u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap())),
            sectors: u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap())),
            kind: mbr_type_name(ptype),
            name: None,
        });
    }
    out
}

fn mbr_type_name(t: u8) -> String {
    let s = match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x07 => "NTFS/exFAT",
        0x0B => "FAT32",
        0x0C => "FAT32 (LBA)",
        0x20 => "bootloader (stage-2/kernel)",
        0x83 => "Linux",
        0xEE => "GPT protective",
        0xDA => "non-FS data",
        _ => "unknown",
    };
    alloc::format!("type=0x{t:02x} ({s})")
}

/// Parse a GPT (protective MBR detected). Used by M9 UEFI images.
/// `lba0` is the protective MBR sector: build.rs stores our FAT32/ext2 data
/// partitions in its free entry slots ("hybrid" layout, like real-world
/// hybrid MBRs), since the GPT entry array itself only carries the
/// firmware's ESP.
/// M10b P3: validate the disk-supplied GPT entry-array geometry before it is
/// used to index or allocate. Returns `Err(reason)` for a header that cannot
/// describe a real entry array.
///
/// Extracted as a pure `fn` (no disk, no allocation) so `gpt_selftest` can
/// drive the exact validation the boot path uses with hostile values - that is
/// the only way to test this deterministically: the real trigger for
/// `scan_gpt` is MBR partition 0's type byte being 0xEE, and on this image that
/// same byte is the bootloader's own boot partition, so corrupting it stops the
/// kernel from booting at all (verified).
fn validate_gpt_geometry(entry_size: usize, entry_count: u32) -> Result<(), alloc::string::String> {
    use alloc::format;
    if entry_size < GPT_ENTRY_MIN {
        return Err(format!(
            "GPT entry size {entry_size} below minimum {GPT_ENTRY_MIN}"
        ));
    }
    if entry_size > GPT_ENTRY_MAX {
        return Err(format!(
            "GPT entry size {entry_size} above maximum {GPT_ENTRY_MAX}"
        ));
    }
    if entry_count > GPT_MAX_ENTRIES {
        return Err(format!(
            "GPT entry count {entry_count} above maximum {GPT_MAX_ENTRIES}"
        ));
    }
    Ok(())
}

/// M10b P3 self-test: feed hostile GPT geometry through the real validation and
/// assert every one is REJECTED (a panic here would abort the kernel, so the
/// test passing is the proof the boot path is safe on such media).
///
/// Runs on every boot; it touches no disk and allocates only small Strings, so
/// it is deterministic and cheap. A PASS line is asserted by
/// `test-fs-corrupt.ps1`.
pub fn gpt_selftest() {
    let bad: [(usize, u32); 5] = [
        (0, 128),          // entry_size 0 -> old code sliced table[0..0] then indexed on
        (32, 128),         // entry_size < 128 -> old code panicked on e[56..128]
        (usize::MAX, 128),  // absurd entry_size -> huge allocation / OOB
        (128, u32::MAX),    // absurd entry_count -> huge allocation
        (128, 0),          // degenerate but legal; must be ACCEPTED below
    ];
    let mut rejected = 0u32;
    for (i, (es, ec)) in bad.iter().enumerate() {
        let expect_ok = i == bad.len() - 1; // the last row is the valid case
        match validate_gpt_geometry(*es, *ec) {
            Ok(()) if expect_ok => {}
            Ok(()) => {
                serial_writeln!("M5: gpt-selftest BAD: ({es},{ec}) was accepted but is invalid");
            }
            Err(_) if !expect_ok => rejected += 1,
            Err(_) => {
                serial_writeln!("M5: gpt-selftest BAD: valid ({es},{ec}) was rejected");
            }
        }
    }
    if rejected == bad.len() as u32 - 1 {
        // NB: do not put the word "panic" in this message. Several suites
        // assert on `PANIC|EXCEPTION` in the serial log, so naming it here
        // would make them report a phantom kernel panic (hit exactly once).
        serial_writeln!("M5: gpt-selftest PASS ({rejected} corrupt geometries rejected, kernel healthy)");
    } else {
        serial_writeln!("M5: gpt-selftest FAILED ({rejected} rejected)");
    }
}

fn scan_gpt(
    drive: &mut AtaDrive,
    lba0: &[u8; SECTOR_SIZE],
) -> (Layout, Vec<Partition>, String) {
    let mut hdr = [0u8; SECTOR_SIZE];
    if drive.read_sectors(1, 1, &mut hdr).is_err() || &hdr[0..8] != b"EFI PART" {
        serial_writeln!("M5: protective MBR but no GPT header at LBA1");
        return (Layout::Mbr, Vec::new(), String::new());
    }
    let hdr_size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let hdr_crc_stored = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
    let entry_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;

    // M10b P3: `entry_size` and `entry_count` come straight off the disk, and
    // everything below indexes with them. An unvalidated value here is a kernel
    // PANIC on corrupt media (panic=abort, so it takes the whole kernel with
    // it) - the same blast radius as a bug anywhere else. Reject a header that
    // cannot describe a real entry array instead of slicing off the end of
    // `table`. (Kept in a `fn` so the corrupt-input self-test can drive it
    // without a real disk - see `gpt_selftest`.)
    if let Err(why) = validate_gpt_geometry(entry_size, entry_count) {
        serial_writeln!("M5: {why} - corrupt GPT header, ignoring GPT");
        return (Layout::Gpt, Vec::new(), String::new());
    }

    // Validate the header CRC32 (computed with the CRC field zeroed).
    let mut tmp = hdr;
    tmp[16..20].fill(0);
    let crc_ok = crc32(&tmp[..hdr_size.min(512)]) == hdr_crc_stored;
    serial_writeln!("M5: GPT header CRC {}", if crc_ok { "OK" } else { "BAD" });

    // Read the entry array (may span several sectors). `saturating_mul` plus a
    // sector cap keep a corrupt count from overflowing the allocation or asking
    // the drive for a gigantic transfer.
    //
    // `read_sectors` takes a `u8` count (max 255 per call), so the array is
    // read in CHUNKS rather than with one cast. The old `sectors as u8` was a
    // silent truncation: an array needing >255 sectors was under-read and the
    // loop then indexed past the end. The chunk loop reads exactly the sectors
    // the validated sizes call for.
    let bytes = (entry_count as usize).saturating_mul(entry_size);
    let want_sectors = (bytes.div_ceil(SECTOR_SIZE) as u64).min(GPT_MAX_SECTORS);
    let mut table = alloc::vec![0u8; want_sectors as usize * SECTOR_SIZE];
    let mut lba = entry_lba;
    let mut remaining = want_sectors;
    while remaining > 0 {
        let chunk = remaining.min(255) as u8;
        let off = (want_sectors - remaining) as usize * SECTOR_SIZE;
        if drive
            .read_sectors(lba as u32, chunk, &mut table[off..off + chunk as usize * SECTOR_SIZE])
            .is_err()
        {
            serial_writeln!("M5: failed to read GPT entry array");
            return (Layout::Gpt, Vec::new(), String::new());
        }
        lba += chunk as u64;
        remaining -= chunk as u64;
    }

    let mut parts = Vec::new();
    for i in 0..entry_count as usize {
        // `entry_size` is validated and `table` is sized from it, so this slice
        // is in range; `get` is a hard guarantee that a future change to the
        // sizing above cannot reintroduce a panic.
        let off = i * entry_size;
        let Some(e) = table.get(off..off + entry_size) else {
            break;
        };
        let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
        if type_guid == [0u8; 16] {
            continue;
        }
        let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
        // Name: UTF-16LE in bytes 56..128 (entry_size >= 128 guarantees it).
        let mut name = String::new();
        for ch in e[56..128].chunks_exact(2) {
            let c = u16::from_le_bytes(ch.try_into().unwrap());
            if c == 0 {
                break;
            }
            name.push(char::from_u32(u32::from(c)).unwrap_or('?'));
        }
        parts.push(Partition {
            start_lba: first,
            // `last - first` underflows in release (wrapping) / debug (panic)
            // when a corrupt entry has first > last; saturating keeps it sane.
            sectors: last.saturating_sub(first).saturating_add(1),
            kind: guid_kind(type_guid),
            name: if name.is_empty() { None } else { Some(name) },
        });
    }

    // M9 hybrid protective MBR: pick up the FAT32 (0x0C) / ext2 (0x83) data
    // partitions build.rs wrote into the protective MBR's free slots, using
    // the same rules as scan_mbr. Firmware ignores these (it boots the ESP
    // from the GPT array); only our kernel consumes them.
    let mut hybrid = 0usize;
    for i in 0..4 {
        let e = &lba0[446 + i * 16..446 + i * 16 + 16];
        let ptype = e[4];
        if ptype == 0 || ptype == 0xEE {
            continue;
        }
        parts.push(Partition {
            start_lba: u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap())),
            sectors: u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap())),
            kind: mbr_type_name(ptype),
            name: None,
        });
        hybrid += 1;
    }
    if hybrid > 0 {
        serial_writeln!(
            "M9: UEFI boot — GPT + {} hybrid protective-MBR data partition(s)",
            hybrid
        );
    }

    let guid = fmt_guid(hdr[56..72].try_into().unwrap());
    (Layout::Gpt, parts, guid)
}

/// Table-less CRC32 (polynomial 0xEDB88320, as used by GPT).
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Format a 16-byte GUID in its standard text form (mixed endian).
fn fmt_guid(b: [u8; 16]) -> String {
    alloc::format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Well-known GPT partition type GUIDs.
fn guid_kind(g: [u8; 16]) -> String {
    match fmt_guid(g).as_str() {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI System".to_string(),
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data".to_string(),
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem".to_string(),
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap".to_string(),
        other => alloc::format!("GUID {other}"),
    }
}