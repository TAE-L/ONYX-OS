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

    // Validate the header CRC32 (computed with the CRC field zeroed).
    let mut tmp = hdr;
    tmp[16..20].fill(0);
    let crc_ok = crc32(&tmp[..hdr_size.min(512)]) == hdr_crc_stored;
    serial_writeln!("M5: GPT header CRC {}", if crc_ok { "OK" } else { "BAD" });

    // Read the entry array (may span several sectors).
    let bytes = entry_count as usize * entry_size;
    let sectors = ((bytes + SECTOR_SIZE - 1) / SECTOR_SIZE).max(1) as u8;
    let mut table = alloc::vec![0u8; sectors as usize * SECTOR_SIZE];
    if drive
        .read_sectors(entry_lba as u32, sectors, &mut table)
        .is_err()
    {
        serial_writeln!("M5: failed to read GPT entry array");
        return (Layout::Gpt, Vec::new(), String::new());
    }

    let mut parts = Vec::new();
    for i in 0..entry_count as usize {
        let e = &table[i * entry_size..(i + 1) * entry_size];
        let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
        if type_guid == [0u8; 16] {
            continue;
        }
        let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
        // Name: UTF-16LE in bytes 56..128.
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
            sectors: last - first + 1,
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