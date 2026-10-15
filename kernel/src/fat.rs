//! M6/M8: FAT32 filesystem driver over the block layer.
//!
//! Mounts the bootloader's FAT32 partition (type 0x0C, located by `block.rs`),
//! supports path-based access (M8), listing, reading, creating and writing
//! files, and directory creation:
//!   * reading is LFN-aware (long names created by Windows round-trip),
//!   * creation uses plain 8.3 short names (no LFN entries written yet),
//!   * subdirectories work end-to-end (M8): path resolution walks directory
//!     cluster chains; `mkdir` initializes `.` / `..` and links the cluster.
//!
//! All methods take `&mut AtaDrive` (the block layer's mutex stays outside),
//! so `vfs.rs` can hold the mounted FS while borrowing the drive per call.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ata::{AtaDrive, SECTOR_SIZE};
use crate::vfs::FsError;

const ATTR_LFN: u8 = 0x0F;
/// P16-1.2: byte 12 of the LAST LFN slot (the one adjacent to the short entry)
/// carries this bit. Without it a reader walking forward from the short entry
/// cannot tell where the run of LFN slots ends.
const LFN_LAST: u8 = 0x40;
const ATTR_DIR: u8 = 0x10;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_ARCHIVE: u8 = 0x20;
/// End-of-chain marker we write; any value >= 0x0FFFFFF8 reads back as EOC.
const EOC: u32 = 0x0FFF_FFFF;

/// Current wall clock as Unix seconds, or `0` when the RTC never initialised.
///
/// P16-1.3. `0` means "no timestamp available" all the way down to the
/// directory entry, which is honest: every other choice here would stamp files
/// with a date the user never asked for.
fn now_unix() -> i64 {
    crate::rtc::now_utc().to_unix().unwrap_or(0)
}

/// FAT directory-entry DATE word -> Unix seconds at 00:00:00.
///
/// P16-1.3. Layout: `((year-1980) << 9) | (month << 5) | day`. FAT can only
/// express 1980..2107, so anything outside is clamped rather than wrapped --
/// wrapping would put a file in the year 19xx and look like corruption.
fn fat_date_to_unix(d: u16) -> u32 {
    if d == 0 {
        return 0; // no date recorded
    }
    let year = (d >> 9) + 1980;
    let month = (d >> 5) & 0x0F;
    let day = d & 0x1F;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    crate::rtc::DateTime {
        second: 0,
        minute: 0,
        hour: 0,
        day: day as u8,
        month: month as u8,
        year: year.min(2107) as u16,
    }
    .to_unix()
    .unwrap_or(0)
    .clamp(0, u32::MAX as i64) as u32
}

/// FAT directory-entry TIME word -> seconds since midnight.
///
/// P16-1.3. Layout: `(hour << 11) | (minute << 5) | second/2`. FAT stores
/// 2-second granularity, so odd seconds are unrepresentable and decode to the
/// even second at or before them.
fn fat_time_secs(t: u16) -> u32 {
    if t == 0 {
        return 0;
    }
    u32::from((t >> 11) & 0x1F) * 3600 + u32::from((t >> 5) & 0x3F) * 60 + u32::from(t & 0x1F) * 2
}

/// Unix seconds -> `(TIME, DATE)` words for a FAT directory entry.
///
/// Returns `(0, 0)` when there is no clock or the date predates 1980, which is
/// exactly how FAT represents "unknown" (a zeroed word).
fn unix_to_fat_time_date(secs: i64) -> (u16, u16) {
    let dt = crate::rtc::from_unix(secs);
    if dt.year < 1980 || dt.year > 2107 {
        return (0, 0);
    }
    let date = ((dt.year - 1980) << 9) | (u16::from(dt.month) << 5) | u16::from(dt.day);
    let time = (u16::from(dt.hour) << 11)
        | (u16::from(dt.minute) << 5)
        | (u16::from(dt.second) / 2);
    (time, date)
}

/// One raw 32-byte directory slot.
struct RawEntry {
    name: String,
    /// The entry's decoded 8.3 short name. Lookups match against BOTH `name`
    /// and `short`: the mkfat-side fatfs crate misplaces the LFN terminator
    /// for names >= 12 chars (e.g. it decodes "AUTOEXEC.TXT" as
    /// "AUTOEXEC.TX"), so the short name is the reliable identifier.
    short: String,
    first_cluster: u32,
    size: u32,
    /// P16-1.3: Unix seconds. `0` means "absent on disk" (an entry written before
    /// timestamps existed, or by a tool that zeroed them), never "the epoch".
    atime: u32,
    mtime: u32,
    ctime: u32,
    is_dir: bool,
    /// Index of the slot within its directory's cluster chain.
    dir_index: u32,
    /// First cluster of the directory that holds this entry.
    dir_cluster: u32,
}

/// What a resolved path points at: the root directory or a concrete entry.
enum Resolution {
    /// The root directory (`/`).
    Root,
    /// A concrete directory entry (file or subdirectory).
    Entry(RawEntry),
}

pub struct Fat32 {
    part_start: u64,
    sectors_per_cluster: u8,
    reserved: u16,
    num_fats: u8,
    fat_sectors: u32,
    root_cluster: u32,
    /// Number of clusters in the data region (valid clusters: 2..=1+total).
    total_clusters: u32,
}

impl Fat32 {
    /// Parse the BPB of the partition at `part_start` and sanity-check it.
    pub fn mount(dev: &mut AtaDrive, part_start: u64) -> Result<Self, FsError> {
        let mut sec = [0u8; SECTOR_SIZE];
        dev.read_sectors(part_start, 1, &mut sec)
            .map_err(FsError::Io)?;
        let bps = u16::from_le_bytes(sec[11..13].try_into().unwrap());
        if bps != SECTOR_SIZE as u16 {
            return Err(FsError::BadFs);
        }
        let spc = sec[13];
        if spc == 0 || spc as usize * SECTOR_SIZE > 8192 {
            return Err(FsError::BadFs);
        }
        let reserved = u16::from_le_bytes(sec[14..16].try_into().unwrap());
        let num_fats = sec[16];
        let root_entries = u16::from_le_bytes(sec[17..19].try_into().unwrap());
        let fat_sectors16 = u16::from_le_bytes(sec[22..24].try_into().unwrap());
        let fat_sectors = u32::from_le_bytes(sec[36..40].try_into().unwrap());
        let root_cluster = u32::from_le_bytes(sec[44..48].try_into().unwrap());
        let total32 = u32::from_le_bytes(sec[32..36].try_into().unwrap());
        if root_entries != 0
            || fat_sectors16 != 0
            || num_fats == 0
            || fat_sectors == 0
            || root_cluster < 2
            || total32 == 0
            || reserved == 0
        {
            return Err(FsError::BadFs);
        }
        let data_start = u32::from(reserved) + u32::from(num_fats) * fat_sectors;
        if data_start >= total32 {
            return Err(FsError::BadFs);
        }
        let total_clusters = (total32 - data_start) / u32::from(spc);
        Ok(Self {
            part_start,
            sectors_per_cluster: spc,
            reserved,
            num_fats,
            fat_sectors,
            root_cluster,
            total_clusters,
        })
    }

    /// One-line summary for boot logs.
    pub fn summary(&self) -> String {
        alloc::format!(
            "cluster={}KiB FATs={}x{}sectors root_cluster={}",
            self.cluster_bytes() / 1024,
            self.num_fats,
            self.fat_sectors,
            self.root_cluster
        )
    }

    fn cluster_lba(&self, c: u32) -> u64 {
        self.part_start
            + u64::from(self.reserved)
            + u64::from(self.num_fats) * u64::from(self.fat_sectors)
            + u64::from(c - 2) * u64::from(self.sectors_per_cluster)
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR_SIZE
    }

    fn read_cluster(&self, dev: &mut AtaDrive, c: u32, buf: &mut [u8]) -> Result<(), FsError> {
        dev.read_sectors(self.cluster_lba(c), self.sectors_per_cluster, buf)
            .map_err(FsError::Io)
    }

    fn write_cluster(&self, dev: &mut AtaDrive, c: u32, buf: &[u8]) -> Result<(), FsError> {
        dev.write_sectors(self.cluster_lba(c), self.sectors_per_cluster, buf)
            .map_err(FsError::Io)
    }

    fn fat_entry(&self, dev: &mut AtaDrive, c: u32) -> Result<u32, FsError> {
        let off = c as usize * 4;
        let lba = self.part_start + u64::from(self.reserved) + (off / SECTOR_SIZE) as u64;
        let mut sec = [0u8; SECTOR_SIZE];
        dev.read_sectors(lba, 1, &mut sec).map_err(FsError::Io)?;
        let o = off % SECTOR_SIZE;
        Ok(u32::from_le_bytes(sec[o..o + 4].try_into().unwrap()))
    }

    /// Write a FAT entry, updating every FAT copy (mirroring).
    fn set_fat_entry(&self, dev: &mut AtaDrive, c: u32, val: u32) -> Result<(), FsError> {
        let off = c as usize * 4;
        for f in 0..self.num_fats {
            let lba = self.part_start
                + u64::from(self.reserved)
                + u64::from(f) * u64::from(self.fat_sectors)
                + (off / SECTOR_SIZE) as u64;
            let mut sec = [0u8; SECTOR_SIZE];
            dev.read_sectors(lba, 1, &mut sec).map_err(FsError::Io)?;
            let o = off % SECTOR_SIZE;
            sec[o..o + 4].copy_from_slice(&val.to_le_bytes());
            dev.write_sectors(lba, 1, &sec).map_err(FsError::Io)?;
        }
        Ok(())
    }

    /// Next cluster of a chain, or `None` at end-of-chain.
    fn next_cluster(&self, dev: &mut AtaDrive, c: u32) -> Result<Option<u32>, FsError> {
        let e = self.fat_entry(dev, c)? & 0x0FFF_FFFF;
        if e >= 0x0FFF_FFF8 {
            Ok(None)
        } else if e < 2 {
            Err(FsError::BadFs)
        } else {
            Ok(Some(e))
        }
    }

    /// Walk a chain starting at `first`, returning at most `max` clusters.
    fn walk_chain(&self, dev: &mut AtaDrive, first: u32, max: u32) -> Result<Vec<u32>, FsError> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            out.push(c);
            if out.len() as u32 >= max {
                break;
            }
            match self.next_cluster(dev, c)? {
                Some(n) => c = n,
                None => break,
            }
        }
        Ok(out)
    }

    /// Allocate one free cluster, mark EOC, link after `prev` if given.
    fn alloc_cluster(&self, dev: &mut AtaDrive, prev: Option<u32>) -> Result<u32, FsError> {
        let last = 1 + self.total_clusters;
        for c in 2..=last {
            if self.fat_entry(dev, c)? == 0 {
                self.set_fat_entry(dev, c, EOC)?;
                if let Some(p) = prev {
                    self.set_fat_entry(dev, p, c)?;
                }
                return Ok(c);
            }
        }
        Err(FsError::NoSpace)
    }

    /// Keep the first `keep` clusters of a chain (EOC-terminated), free the rest.
    fn free_from(&self, dev: &mut AtaDrive, first: u32, keep: usize) -> Result<(), FsError> {
        let chain = self.walk_chain(dev, first, self.total_clusters)?;
        for (i, &c) in chain.iter().enumerate() {
            if i >= keep {
                self.set_fat_entry(dev, c, 0)?;
            }
        }
        if keep > 0 {
            self.set_fat_entry(dev, chain[keep - 1], EOC)?;
        }
        Ok(())
    }

    /// Collect every entry of the directory at `dir_cluster` (LFN-aware).
    /// `.`/`..` entries are included; deleted slots and volume labels skip.
    fn scan_dir(&self, dev: &mut AtaDrive, dir_cluster: u32) -> Result<Vec<RawEntry>, FsError> {
        let chain = self.walk_chain(dev, dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as u32;
        let mut out = Vec::new();
        let mut lfn: Vec<(u8, [u8; 26])> = Vec::new();
        // Checksum of the run currently in `lfn`, from the slot's byte 13. It is
        // the only thing that ties a run to the short entry after it.
        let mut lfn_sum: u8 = 0;
        // Order number this run expects NEXT. One run is stored in DESCENDING
        // order (N, N-1, ... 1), so a slot that is not the expected one means a
        // different run has begun.
        let mut lfn_next: u8 = 0;
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        for (ci, &c) in chain.iter().enumerate() {
            self.read_cluster(dev, c, &mut buf)?;
            for (si, slot) in buf.chunks_exact(32).enumerate() {
                match slot[0] {
                    0x00 => return Ok(out), // end-of-directory marker
                    0xE5 => {
                        lfn.clear(); // deleted slot: reusable
                        lfn_sum = 0;
                        lfn_next = 0;
                        continue;
                    }
                    _ => {}
                }
                let attr = slot[11];
                if attr == ATTR_LFN {
                    let mut part = [0u8; 26];
                    // P16-1.2: the character offsets were WRONG. The LFN layout
                    // is 5 units at bytes 1..11, 6 units at 14..26, and 2 units
                    // at 28..32 - bytes 26..28 are the first-cluster field, which
                    // belongs to the SHORT entry only.
                    //
                    // This read `slot[14..28]`, so it swallowed the two cluster
                    // bytes as if they were a character, and `slot[28..30]`, so it
                    // only ever recovered 12 of the 13 characters per slot. Any
                    // LFN that Windows wrote still "mostly" decoded, which is why
                    // this survived: a 12th-character name reads back with the
                    // 13th silently dropped and a garbage unit spliced in.
                    part[..10].copy_from_slice(&slot[1..11]); // units 0..4
                    part[10..22].copy_from_slice(&slot[14..26]); // units 5..10
                    part[22..26].copy_from_slice(&slot[28..32]); // units 11..12
                    // A reader is supposed to scan BACKWARDS from the short entry until it finds
                    // the LAST_LONG slot. Scanning forward, an orphaned run left
                    // in front of a new entry would be concatenated onto it and
                    // BOTH names would be lost, so the run is restarted whenever
                    // the descending sequence is broken. Note this cannot simply
                    // key off LAST_LONG: order 1 is stored LAST, so clearing on
                    // it would discard the very run it terminates.
                    let order = slot[0] & 0x1F;
                    if lfn_next == 0 || order != lfn_next {
                        lfn.clear();
                        lfn_sum = 0;
                    }
                    lfn.push((order, part));
                    lfn_sum = slot[13];
                    lfn_next = if order > 1 { order - 1 } else { 0 };
                    continue;
                }
                if attr & ATTR_VOLUME != 0 {
                    lfn.clear(); // volume label entry
                    lfn_sum = 0;
                    lfn_next = 0;
                    continue;
                }
                // Regular (short) entry — the pending run names it, but ONLY if
                // its checksum matches this short name. Without the test an
                // orphaned run is displayed: `rename` rewrites the short bytes
                // and leaves the old LFN slots in front of them, so the entry
                // would keep reporting the name it was renamed AWAY from.
                // Windows checks this field for exactly this reason.
                let short = short_name(slot);
                let name = if lfn.is_empty() || lfn_sum != lfn_checksum(&slot[..11].try_into().unwrap())
                {
                    short.clone()
                } else {
                    lfn_name(&lfn)
                };
                lfn.clear();
                lfn_sum = 0;
                lfn_next = 0;
                let cluster_lo = u16::from_le_bytes(slot[26..28].try_into().unwrap());
                let cluster_hi = u16::from_le_bytes(slot[20..22].try_into().unwrap());
                out.push(RawEntry {
                    name,
                    short,
                    first_cluster: (u32::from(cluster_hi) << 16) | u32::from(cluster_lo),
                    size: u32::from_le_bytes(slot[28..32].try_into().unwrap()),
                    // P16-1.3: byte layout per the FAT spec --
                    // 14..16 create time, 16..18 create date, 18..20 access date,
                    // 22..24 write time, 24..26 write date. Note bytes 26..28 are
                    // the low cluster (read just above), NOT a timestamp: the
                    // fields are not contiguous, which is the kind of thing that
                    // silently writes a cluster number into a date.
                    ctime: fat_date_to_unix(u16::from_le_bytes(slot[16..18].try_into().unwrap())),
                    atime: fat_date_to_unix(u16::from_le_bytes(slot[18..20].try_into().unwrap())),
                    mtime: {
                        let t = fat_time_secs(u16::from_le_bytes(slot[22..24].try_into().unwrap()));
                        let d = fat_date_to_unix(u16::from_le_bytes(slot[24..26].try_into().unwrap()));
                        d.saturating_add(t)
                    },
                    is_dir: attr & ATTR_DIR != 0,
                    dir_index: (ci as u32 * slots_per_cluster) + si as u32,
                    dir_cluster,
                });
            }
        }
        Ok(out)
    }

    /// Case-insensitive lookup by name inside one directory. Matches the
    /// LFN name or the 8.3 short name (see `RawEntry::short`).
    fn find_in(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
        name: &str,
    ) -> Result<Option<RawEntry>, FsError> {
        Ok(self
            .scan_dir(dev, dir_cluster)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name) || e.short.eq_ignore_ascii_case(name)))
    }

    /// Directory listing of the path for the VFS (`/` = root).
    pub fn list(
        &self,
        dev: &mut AtaDrive,
        path: &str,
    ) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        match self.resolve(dev, path)? {
            Some(Resolution::Root) => self.list_dir(dev, self.root_cluster),
            Some(Resolution::Entry(e)) if e.is_dir && e.first_cluster >= 2 => {
                self.list_dir(dev, e.first_cluster)
            }
            Some(Resolution::Entry(_)) => Err(FsError::NotSupported), // a file
            None => Err(FsError::NotFound),
        }
    }

    /// DirEntry conversion of one directory's raw entries, `.`/`..` filtered.
    fn list_dir(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
    ) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        Ok(self
            .scan_dir(dev, dir_cluster)?
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .map(|e| crate::vfs::DirEntry {
                name: e.name,
                size: u64::from(e.size),
                is_dir: e.is_dir,
            })
            .collect())
    }

    /// Validate that `path` exists (used by SYS_OPEN).
    pub fn open(&self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        match self.resolve(dev, path)? {
            Some(_) => Ok(()),
            None => Err(FsError::NotFound),
        }
    }

    /// Resolve `path` against the root (`/` = root itself). Intermediate
    /// components must exist and be directories; a missing final component
    /// yields `Ok(None)`.
    fn resolve(&self, dev: &mut AtaDrive, path: &str) -> Result<Option<Resolution>, FsError> {
        let parts = crate::vfs::split_path(path).ok_or(FsError::NotSupported)?;
        if parts.is_empty() {
            return Ok(Some(Resolution::Root));
        }
        let (last, parents) = parts.split_last().unwrap();
        let mut dir = self.root_cluster;
        for comp in parents {
            let e = self.find_in(dev, dir, comp)?.ok_or(FsError::NotFound)?;
            if !e.is_dir || e.first_cluster < 2 {
                return Err(FsError::BadFs); // a file in the middle of the path
            }
            dir = e.first_cluster;
        }
        Ok(self.find_in(dev, dir, last)?.map(Resolution::Entry))
    }

    /// Walk to the parent directory of `path`: returns the parent's first
    /// cluster and the final component. The root has no parent (the caller
    /// handles `/` up front).
    fn resolve_parent(&self, dev: &mut AtaDrive, path: &str) -> Result<(u32, String), FsError> {
        let parts = crate::vfs::split_path(path).ok_or(FsError::NotSupported)?;
        let Some((last, parents)) = parts.split_last() else {
            return Err(FsError::NotSupported); // "/" has nothing to create in
        };
        let mut dir = self.root_cluster;
        for comp in parents {
            let e = self.find_in(dev, dir, comp)?.ok_or(FsError::NotFound)?;
            if !e.is_dir || e.first_cluster < 2 {
                return Err(FsError::BadFs);
            }
            dir = e.first_cluster;
        }
        Ok((dir, (*last).to_string()))
    }
}

/// Assemble an LFN (long file name) from its 26-byte parts, in **descending**
/// sequence order; terminates at U+0000 / U+FFFF padding.
///
/// P16-1.2 **second bug found by the independent verifier (1.5)**: this sorted
/// ASCENDING by sequence number. That pairs with a writer that puts the first
/// chunk in the order-1 slot, which is backwards: the spec has the slot numbered
/// *N* -- the highest, physically furthest from the short entry -- holding
/// characters `[(N-1)*13 .. N*13)`, i.e. the FIRST 13, while order 1, next to the
/// short entry, holds the LAST 13.
///
/// Ascending made us self-consistent and format-inconsistent, so every name over
/// 13 characters was stored with its chunks transposed and would have appeared
/// in Windows as "CharsLongInteropTwenty" instead of "InteropTwentyCharsLong".
/// Descending is both the spec order and the physical order, so this sort is
/// only re-establishing what the slot order already said.
fn lfn_name(parts: &[(u8, [u8; 26])]) -> String {
    let mut sorted = parts.to_vec();
    sorted.sort_by(|a, b| b.0.cmp(&a.0)); // N, N-1, ... 1
    let mut s = String::new();
    for (_, p) in &sorted {
        for ch in p.chunks_exact(2) {
            let c = u16::from_le_bytes(ch.try_into().unwrap());
            if c == 0x0000 || c == 0xFFFF {
                return s;
            }
            s.push(char::from_u32(u32::from(c)).unwrap_or('?'));
        }
    }
    s
}

/// Decode the 8.3 short name (honoring the NT lowercase flags in byte 12).
fn short_name(slot: &[u8]) -> String {
    let mut base = [0u8; 8];
    base.copy_from_slice(&slot[0..8]);
    if base[0] == 0x05 {
        base[0] = 0xE5; // a literal 0xE5 first byte is escaped this way
    }
    let lower = slot[12];
    let mut s = String::new();
    for &b in base.iter().take_while(|&&b| b != b' ') {
        let c = if lower & 0x08 != 0 {
            b.to_ascii_lowercase()
        } else {
            b
        };
        s.push(c as char);
    }
    if slot[8..11].iter().any(|&b| b != b' ') {
        s.push('.');
        for &b in slot[8..11].iter().take_while(|&&b| b != b' ') {
            let c = if lower & 0x10 != 0 {
                b.to_ascii_lowercase()
            } else {
                b
            };
            s.push(c as char);
        }
    }
    s
}

impl Fat32 {
    /// Read up to `buf.len()` bytes of `path` at file offset `off`.
    pub fn read_at(
        &self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let e = match self.resolve(dev, path)? {
            Some(Resolution::Entry(e)) if !e.is_dir => e,
            Some(_) => return Err(FsError::NotSupported), // a directory
            None => return Err(FsError::NotFound),
        };
        if e.first_cluster == 0 || off >= u64::from(e.size) {
            return Ok(0);
        }
        let cb = self.cluster_bytes() as u64;
        let chain = self.walk_chain(dev, e.first_cluster, self.total_clusters)?;
        let start_idx = (off / cb) as usize;
        if start_idx >= chain.len() {
            return Ok(0);
        }
        let mut cbuf = alloc::vec![0u8; cb as usize];
        let mut file_pos = off;
        let mut out = 0usize;
        let mut idx = start_idx;
        loop {
            self.read_cluster(dev, chain[idx], &mut cbuf)?;
            let in_off = (file_pos - idx as u64 * cb) as usize;
            let remaining_file = (e.size as u64 - file_pos) as usize;
            let n = (cb as usize - in_off)
                .min(buf.len() - out)
                .min(remaining_file);
            buf[out..out + n].copy_from_slice(&cbuf[in_off..in_off + n]);
            out += n;
            file_pos += n as u64;
            if out == buf.len() || file_pos >= u64::from(e.size) {
                break;
            }
            idx += 1;
            if idx >= chain.len() {
                break;
            }
        }
        Ok(out)
    }

    /// Write `buf` at offset `off`, growing/truncating the chain as needed.
    /// Creates the file if it does not exist (8.3-compatible names only; the
    /// parent directory must already exist — `mkdir` makes those).
    pub fn write_at(
        &self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        let mut e = match self.resolve(dev, path)? {
            Some(Resolution::Entry(e)) if !e.is_dir => e,
            Some(_) => return Err(FsError::NotSupported),
            None => {
                // Create-if-missing inside the existing parent directory.
                let (dir_cluster, name) = self.resolve_parent(dev, path)?;
                self.create_entry(dev, dir_cluster, &name, 0, false)?
            }
        };
        let new_end = off + buf.len() as u64;
        if new_end > u32::MAX as u64 {
            return Err(FsError::NotSupported);
        }
        let cb = self.cluster_bytes() as u64;
        let need = ((new_end + cb - 1) / cb) as usize;

        let mut chain = if e.first_cluster == 0 {
            Vec::new()
        } else {
            self.walk_chain(dev, e.first_cluster, self.total_clusters)?
        };
        while chain.len() < need {
            let prev = chain.last().copied();
            let c = self.alloc_cluster(dev, prev)?;
            if prev.is_none() {
                e.first_cluster = c;
            }
            chain.push(c);
        }
        if chain.len() > need {
            self.free_from(dev, e.first_cluster, need)?;
            chain.truncate(need);
        }

        // Write the data, read-modify-writing partial cluster edges.
        let mut cbuf = alloc::vec![0u8; cb as usize];
        let mut written = 0usize;
        let mut file_pos = off;
        let mut idx = (off / cb) as usize;
        while written < buf.len() {
            let c = chain[idx];
            let in_off = (file_pos - idx as u64 * cb) as usize;
            let n = (cb as usize - in_off).min(buf.len() - written);
            if n == cb as usize {
                cbuf[..n].copy_from_slice(&buf[written..written + n]);
            } else {
                self.read_cluster(dev, c, &mut cbuf)?;
                cbuf[in_off..in_off + n].copy_from_slice(&buf[written..written + n]);
            }
            self.write_cluster(dev, c, &cbuf)?;
            written += n;
            file_pos += n as u64;
            idx += 1;
        }

        // Persist size + (possibly new) first cluster to the directory slot.
        if new_end > u64::from(e.size) {
            e.size = new_end as u32;
        }
        self.write_dirent(dev, &e)?;
        Ok(buf.len())
    }

    /// Derive the on-disk identity (`short` alias + LFN slots) for `name` in the
    /// directory `dir_cluster`, excluding `except_short` from the collision probe.
    ///
    /// P16-1.2.1. Shared by `create_entry` and `rename` so both agree on what a
    /// name becomes on disk. `except_short` exists for rename: renaming a long
    /// file must not see its OWN alias as taken, or a rename that keeps the same
    /// long name would silently bump `~1` to `~2`.
    fn derive_name(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
        name: &str,
        except_short: Option<&str>,
    ) -> Result<([u8; 11], Vec<[u8; 32]>), FsError> {
        let long = needs_lfn(name);
        let short = if long {
            let taken: Vec<String> = self
                .scan_dir(dev, dir_cluster)?
                .into_iter()
                .map(|e| e.short)
                .filter(|s| !except_short.is_some_and(|x| s.eq_ignore_ascii_case(x)))
                .collect();
            make_unique_short(name, |cand| {
                taken.iter().any(|t| t.eq_ignore_ascii_case(cand))
            })?
        } else {
            make_short_name(name)?
        };
        let lfn = if long {
            lfn_slots(name, &short)
        } else {
            Vec::new()
        };
        Ok((short, lfn))
    }

    /// Reserve `need` CONTIGUOUS free slots for an LFN run plus its short slot,
    /// extending the directory chain when it is full.
    ///
    /// Contiguity is the requirement: a reader scans backwards from the short
    /// entry, so a gap between the run and the short slot loses the name. Both
    /// `0x00` (end-of-directory) and `0xE5` (deleted) count as free, and writing
    /// into the `0x00` region is fine because [`Self::write_run`] re-marks the
    /// following slot.
    fn alloc_run(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
        need: usize,
    ) -> Result<(Vec<u32>, usize, usize), FsError> {
        let mut chain = self.walk_chain(dev, dir_cluster, self.total_clusters)?;
        let total = self.cluster_bytes() / 32;
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        let mut target: Option<(usize, usize)> = None;
        'outer: for (ci, &c) in chain.iter().enumerate() {
            self.read_cluster(dev, c, &mut buf)?;
            for si in 0..total {
                if si + need > total {
                    break; // not enough room left in THIS cluster
                }
                let free = (0..need).all(|k| {
                    let s = buf[(si + k) * 32];
                    s == 0x00 || s == 0xE5
                });
                if free {
                    target = Some((ci, si));
                    break 'outer;
                }
            }
        }
        let (ci, si) = match target {
            Some(t) => t,
            None => {
                // Directory full (or too fragmented): extend its chain with a
                // zeroed cluster and start the run there.
                let prev = chain.last().copied();
                let c = self.alloc_cluster(dev, prev)?;
                chain.push(c);
                (chain.len() - 1, 0)
            }
        };
        Ok((chain, ci, si))
    }

    /// Write an LFN run followed by the short entry at `si`, then re-terminate
    /// the directory.
    ///
    /// The slot after the short entry is forced back to `0x00`; without that a
    /// reader walks into whatever follows. It is only touched when the run did
    /// not end the cluster, because past the end is a different allocation which
    /// terminates itself.
    fn write_run(
        &self,
        dev: &mut AtaDrive,
        chain: &[u32],
        ci: usize,
        si: usize,
        lfn: &[[u8; 32]],
        slot: &[u8; 32],
    ) -> Result<(), FsError> {
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        self.read_cluster(dev, chain[ci], &mut buf)?;
        let base = si * 32;
        for (k, l) in lfn.iter().enumerate() {
            buf[base + k * 32..base + k * 32 + 32].copy_from_slice(l);
        }
        let short_at = base + lfn.len() * 32;
        buf[short_at..short_at + 32].copy_from_slice(slot);
        // P16-1.2.1: do NOT re-terminate the directory here.
        //
        // This used to write 0x00 into the slot after the new entry,
        // unconditionally. That destroys a live entry whenever the run is
        // allocated into a hole that is immediately followed by one: with
        // [A][B][C], deleting B leaves [A][0xE5][C], creating D lands D in B's
        // slot and then zeroes C -- C vanishes from the directory while its data
        // clusters stay allocated. Nothing in a well-formed directory needs the
        // write: `alloc_run` only hands out slots that are ALREADY 0x00 or 0xE5,
        // so the following slot is either already a terminator, another deleted
        // slot, or a live entry that must stay visible.
        self.write_cluster(dev, chain[ci], &buf)
    }

    /// Create an entry (file or directory) named `name` in the directory at
    /// `dir_cluster`. `first_cluster` is the entry's start cluster (0 = empty
    /// file).
    ///
    /// P16-1.2: `name` may be a long filename. If it is not 8.3-representable a
    /// unique short alias (`NAME~1.EXT`) is generated and the name is stored in
    /// LFN slots immediately in front of the short entry. If it IS 8.3-legal,
    /// exactly one slot is written and the on-disk layout is byte-identical to
    /// before this change.
    fn create_entry(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
        name: &str,
        first_cluster: u32,
        is_dir: bool,
    ) -> Result<RawEntry, FsError> {
        if name.contains('/') || name.contains('\\') {
            return Err(FsError::NotSupported);
        }
        let mut chain = self.walk_chain(dev, dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as u32;
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];

        // P16-1.2.1: the on-disk identity and the slot reservation are shared
        // with `rename`, so a name can never be derived one way on create and
        // another way on rename.
        let (short, lfn) = self.derive_name(dev, dir_cluster, name, None)?;
        // One short slot plus every LFN slot. LFN and its short entry MUST be
        // CONTIGUOUS — a reader scans backwards from the short entry until it
        // finds LAST_LONG, so a gap between them loses the name.
        let need = lfn.len() + 1;

        // Find `need` CONTIGUOUS free slots, extending the chain if the directory
        // is full. See [`Self::alloc_run`].
        let (chain, ci, si) = self.alloc_run(dev, dir_cluster, need)?;
        let dir_index = ci as u32 * slots_per_cluster + si as u32 + lfn.len() as u32;

        // The short entry, LAST: LFN slots precede it on disk.
        let mut slot = [0u8; 32];
        slot[..11].copy_from_slice(&short);
        slot[11] = if is_dir { ATTR_DIR } else { ATTR_ARCHIVE };
        slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
        // P16-1.3: stamp the three times. FAT has no sub-second field worth using
        // and no separate "ctime", so creation maps to ctime, and the access date
        // gets the date only (bytes 18..20 hold a DATE with no time field -- that
        // asymmetry is the format's, not a shortcut here).
        let now = now_unix();
        if now != 0 {
            let (ct, cd) = unix_to_fat_time_date(now);
            let (_, ad) = unix_to_fat_time_date(now);
            slot[14..16].copy_from_slice(&ct.to_le_bytes()); // create time
            slot[16..18].copy_from_slice(&cd.to_le_bytes()); // create date
            slot[18..20].copy_from_slice(&ad.to_le_bytes()); // last access date
            slot[22..24].copy_from_slice(&ct.to_le_bytes()); // last write time
            slot[24..26].copy_from_slice(&cd.to_le_bytes()); // last write date
        }

        self.write_run(dev, &chain, ci, si, &lfn, &slot)?;

        Ok(RawEntry {
            name: name.to_string(),
            short: String::from_utf8_lossy(&short).trim().to_string(),
            first_cluster,
            size: 0,
            atime: now as u32,
            mtime: now as u32,
            ctime: now as u32,
            is_dir,
            dir_index,
            dir_cluster,
        })
    }

    /// Persist first_cluster + size of an entry back to its directory slot.
    fn write_dirent(&self, dev: &mut AtaDrive, e: &RawEntry) -> Result<(), FsError> {
        let chain = self.walk_chain(dev, e.dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as usize;
        let ci = e.dir_index as usize / slots_per_cluster;
        let si = e.dir_index as usize % slots_per_cluster;
        if ci >= chain.len() {
            return Err(FsError::BadFs);
        }
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        self.read_cluster(dev, chain[ci], &mut buf)?;
        let slot = &mut buf[si * 32..si * 32 + 32];
        slot[20..22].copy_from_slice(&((e.first_cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((e.first_cluster & 0xFFFF) as u16).to_le_bytes());
        slot[28..32].copy_from_slice(&e.size.to_le_bytes());
        // P16-1.3: this runs on every size change, i.e. on every write, so it is
        // where mtime and the access date advance. A rename does NOT come through
        // here (it writes its own slot), which is what keeps the two distinct.
        let now = now_unix();
        if now != 0 {
            let (wt, wd) = unix_to_fat_time_date(now);
            let (_, ad) = unix_to_fat_time_date(now);
            slot[22..24].copy_from_slice(&wt.to_le_bytes());
            slot[24..26].copy_from_slice(&wd.to_le_bytes());
            slot[18..20].copy_from_slice(&ad.to_le_bytes());
        }
        self.write_cluster(dev, chain[ci], &buf)
    }

    /// Create the directory at `path`: the parent must exist and the final name
    /// must be free in it. A long name gets LFN slots for free, since the entry
    /// itself is always built by `create_entry`. Allocates one cluster,
    /// initializes the
    /// mandatory `.` (self) and `..` (parent; 0 for the root) entries and
    /// links the directory entry into the parent.
    pub fn mkdir(&self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        if self.resolve(dev, path)?.is_some() {
            return Err(FsError::Exists);
        }
        let (parent, name) = self.resolve_parent(dev, path)?;

        // Fresh cluster for the directory contents (zero-initialized below).
        let c = self.alloc_cluster(dev, None)?;
        let up = if parent == self.root_cluster { 0 } else { parent };
        let mut dot = [0u8; 32];
        dot[..11].copy_from_slice(b".          ");
        dot[11] = ATTR_DIR;
        dot[20..22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
        dot[26..28].copy_from_slice(&(c as u16).to_le_bytes());
        let mut dotdot = [0u8; 32];
        dotdot[..11].copy_from_slice(b"..         ");
        dotdot[11] = ATTR_DIR;
        dotdot[20..22].copy_from_slice(&((up >> 16) as u16).to_le_bytes());
        dotdot[26..28].copy_from_slice(&(up as u16).to_le_bytes());
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        buf[..32].copy_from_slice(&dot);
        buf[32..64].copy_from_slice(&dotdot);
        self.write_cluster(dev, c, &buf)?;

        self.create_entry(dev, parent, &name, c, true)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // C3: metadata, unlink, rename.
    // -----------------------------------------------------------------------

    /// Metadata about `path` (`FileStat`, C3).
    pub fn stat(&self, dev: &mut AtaDrive, path: &str) -> Result<crate::vfs::FileStat, FsError> {
        match self.resolve(dev, path)? {
            Some(Resolution::Root) => Ok(crate::vfs::FileStat {
                size: 0,
                is_dir: true,
                atime: 0,
                mtime: 0,
                ctime: 0,
            }),
            Some(Resolution::Entry(e)) => Ok(crate::vfs::FileStat {
                size: u64::from(e.size),
                is_dir: e.is_dir,
                atime: u64::from(e.atime),
                mtime: u64::from(e.mtime),
                ctime: u64::from(e.ctime),
            }),
            None => Err(FsError::NotFound),
        }
    }

    /// P16-1.4: make this file's metadata durable.
    ///
    /// The block cache is write-through and never holds dirty sectors, so the
    /// data clusters are already on the device by the time `write` returns. What
    /// `fsync` actually has to do here is re-issue the directory entry write --
    /// which is what carries the size, the cluster pointer and the timestamps --
    /// and then invalidate the cache so a subsequent read is served from the disk
    /// rather than from a copy taken before the sync.
    ///
    /// Conservative on purpose: it flushes the WHOLE cache rather than just the
    /// LBAs behind this file. Targeted invalidation is an optimisation; getting
    /// the set wrong is a correctness bug, so start broad (see
    /// `docs/OPEN_ISSUES.md` B7).
    pub fn fsync(&self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        let e = match self.resolve(dev, path)? {
            Some(Resolution::Entry(e)) => e,
            // The root directory has no slot to rewrite, but the caller still
            // asked for a sync, so honour it.
            Some(Resolution::Root) => {
                crate::blkcache::flush();
                return Ok(());
            }
            None => return Err(FsError::NotFound),
        };
        self.write_dirent(dev, &e)?;
        crate::blkcache::flush();
        Ok(())
    }

    /// Mark an entry's directory slots deleted (0xE5) WITHOUT freeing its data
    /// cluster chain.
    ///
    /// P16-1.2.1. Split out from [`Self::delete_entry`] because rename must move
    /// a directory entry to a different slot while keeping its data: calling
    /// `delete_entry` after writing the new slot would free clusters the file is
    /// still using.
    fn remove_dir_slots(&self, dev: &mut AtaDrive, e: &RawEntry) -> Result<(), FsError> {
        let chain = self.walk_chain(dev, e.dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as usize;
        let ci = e.dir_index as usize / slots_per_cluster;
        let si = e.dir_index as usize % slots_per_cluster;
        if ci >= chain.len() {
            return Err(FsError::BadFs);
        }
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        self.read_cluster(dev, chain[ci], &mut buf)?;
        buf[si * 32] = 0xE5; // deleted-slot marker
        // P16-1.2: reclaim the entry's LFN slots as well. They sit IMMEDIATELY
        // before the short slot, so they are identifiable without trusting the
        // forward scan. Leaving them in place is not merely untidy: it leaks
        // slots until the directory fills, and an orphaned run sitting in front
        // of a newly created entry is what makes the reader merge two names.
        //
        // Only within THIS cluster: a run we create is always contiguous in one
        // cluster (`create_entry` requires that), but a name of 255 chars spans
        // 20 slots and CAN straddle a boundary, which Windows may have written.
        // Such a straggler is simply missed here and recovered as an orphan by
        // the LAST_LONG reset in `scan_dir`.
        let mut back = si;
        while back > 0 {
            back -= 1;
            if buf[back * 32 + 11] == ATTR_LFN {
                buf[back * 32] = 0xE5;
            } else {
                break;
            }
        }
        self.write_cluster(dev, chain[ci], &buf)
    }

    /// Delete an entry: free its directory slots, then its whole data chain.
    fn delete_entry(&self, dev: &mut AtaDrive, e: &RawEntry) -> Result<(), FsError> {
        self.remove_dir_slots(dev, e)?;
        if e.first_cluster != 0 {
            // Free the whole data chain (keep = 0).
            self.free_from(dev, e.first_cluster, 0)?;
        }
        Ok(())
    }

    /// Remove the file at `path`. Directories are rejected with `IsDir`
    /// (matching Linux: unlink removes names, not directory trees).
    pub fn unlink(&self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        let e = match self.resolve(dev, path)? {
            Some(Resolution::Entry(e)) => e,
            Some(Resolution::Root) => return Err(FsError::NotSupported),
            None => return Err(FsError::NotFound),
        };
        if e.is_dir {
            return Err(FsError::IsDir);
        }
        self.delete_entry(dev, &e)
    }

    /// Same-directory rename.
    ///
    /// P16-1.2.1: preserves a long filename. The previous implementation rewrote
    /// only the 8.3 name bytes in place, so renaming to a long name silently
    /// degraded it to `LONGFIL~1.TXT` and the long name was simply lost.
    ///
    /// The entry is MOVED to a fresh slot run rather than rewritten in place.
    /// LFN slots must sit immediately before their short slot, so the slot before
    /// the entry is occupied by the old run; a new (or differently sized) run
    /// cannot be built there. Only the directory slot moves, never the data
    /// cluster chain, so this is cheap regardless of file size.
    ///
    /// Cross-directory moves are still rejected (the source's slots and cluster
    /// chain belong to one parent; moving would need copy + delete).
    pub fn rename(&self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        let e = match self.resolve(dev, from)? {
            Some(Resolution::Entry(e)) => e,
            Some(Resolution::Root) => return Err(FsError::NotSupported),
            None => return Err(FsError::NotFound),
        };
        // POSIX: rename(a, a) succeeds and changes nothing. Without this the
        // existence check below rejects it with EEXIST, so a caller that
        // "renames before writing" would churn the file's alias on every call.
        // Compare slots, not path strings: the two paths can differ in case or
        // still resolve to the same entry.
        match self.resolve(dev, to) {
            Ok(Some(Resolution::Entry(dst))) => {
                if dst.dir_cluster == e.dir_cluster && dst.dir_index == e.dir_index {
                    return Ok(());
                }
                return Err(FsError::Exists);
            }
            Ok(Some(Resolution::Root)) => return Err(FsError::Exists),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        let (to_parent, _to_name) = self.resolve_parent(dev, to)?;
        // The destination's parent cluster must be the source dir (and the
        // destination path must not be the root — it isn't: `to` would have
        // resolved as existing above).
        if to_parent != e.dir_cluster {
            return Err(FsError::NotSupported);
        }
        let to_name = crate::vfs::split_path(to)
            .and_then(|mut p| p.pop().map(|c| c.to_string()))
            .ok_or(FsError::NotSupported)?;
        // P16-1.2.1: the alias probe skips the entry's OWN current short name,
        // otherwise re-applying the same long name would read its own `~1` alias
        // as a collision and bump it to `~2` on every call.
        let (short, lfn) =
            self.derive_name(dev, e.dir_cluster, &to_name, Some(e.short.as_str()))?;

        // The new short slot, carrying over everything that identifies the DATA:
        // cluster pointer, size, attributes. Only the name changes.
        let mut slot = [0u8; 32];
        slot[..11].copy_from_slice(&short);
        slot[11] = if e.is_dir { ATTR_DIR } else { ATTR_ARCHIVE };
        slot[20..22].copy_from_slice(&((e.first_cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((e.first_cluster & 0xFFFF) as u16).to_le_bytes());
        slot[28..32].copy_from_slice(&e.size.to_le_bytes());
        // P16-1.3: rename is not a modification, so the times carry over verbatim.
        // Stamping "now" here would make every rename look like an edit.
        if e.ctime != 0 {
            let (ct, cd) = unix_to_fat_time_date(i64::from(e.ctime));
            let (_, ad) = unix_to_fat_time_date(i64::from(e.atime));
            let (wt, wd) = unix_to_fat_time_date(i64::from(e.mtime));
            slot[14..16].copy_from_slice(&ct.to_le_bytes());
            slot[16..18].copy_from_slice(&cd.to_le_bytes());
            slot[18..20].copy_from_slice(&ad.to_le_bytes());
            slot[22..24].copy_from_slice(&wt.to_le_bytes());
            slot[24..26].copy_from_slice(&wd.to_le_bytes());
        }

        // Write the new run FIRST, then free the old slots. With the opposite
        // order a failed second step would leave the entry with no directory
        // slot at all; this way a failure leaves it still reachable by old name.
        let need = lfn.len() + 1;
        let (chain, ci, si) = self.alloc_run(dev, e.dir_cluster, need)?;
        self.write_run(dev, &chain, ci, si, &lfn, &slot)?;
        self.remove_dir_slots(dev, &e)?;
        Ok(())
    }
}

/// Does `name` need long-filename entries, i.e. is it not exactly
/// representable as an 8.3 short name?
///
/// P16-1.2: the test that decides whether LFN slots are written. It mirrors
/// `make_short_name`'s acceptance rule exactly — a name that *could* be a legal
/// 8.3 name must NOT get LFN entries, because Windows (and our own reader) then
/// shows the short name and an LFN copy of the same thing is just two entries
/// where one was needed.
fn needs_lfn(name: &str) -> bool {
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (b, e),
        _ => (name, ""),
    };
    base.len() > 8 || ext.len() > 3 || base.is_empty() || name.len() > 12 || {
        // A character outside the 8.3 legal set also forces LFN, even if it
        // happens to be short enough.
        !name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'~' | b'.')
        })
    }
}

/// LFN slot checksum: the rolling 8-bit sum over the 11 short-name bytes.
///
/// The short entry carries this so a reader can tell whether the LFN slots in
/// front of it really belong to it. Getting it wrong does not corrupt the disk,
/// it just makes readers (including ours) discard the long name — which is
/// precisely the bug 1.2 exists to fix, so it is computed rather than guessed.
/// Checksum tying an LFN run to the short entry in front of it.
///
/// **This is a rotate, not a plain sum, and that is not a typo.** The FAT
/// checksum for an LFN entry (byte 13) is the original MS-DOS `ChkSum`:
///
/// ```text
///     Sum = 0
///     for each of the 11 short-name bytes:
///         Sum = ((Sum & 1) ? 0x80 : 0) + (Sum >> 1) + byte
/// ```
///
/// i.e. an 8-bit rotate-right folded into the addition. The FAT documentation
/// states it as "NOTE: The operation is an unsigned char rotate right".
///
/// This function was "fixed" once to a plain `sum & 0xFF` on the theory that the
/// spec says "sum the 11 bytes". That was **wrong**, and it is recorded here
/// because the mistake is instructive: the third-party `fatfs` crate (which
/// `mkfat` uses to build our own images) implements the same rotate, and the
/// `fatfs`-based oracle in `tools/fatfs-verify` then failed to read a single long
/// name this kernel had written. Only cross-checking against a second
/// implementation caught it -- the in-kernel self-test could not, because the
/// writer and reader shared the function either way.
///
/// Done in `u16` and truncated ONCE: 11 x 0xFF overflows `u8`.
fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in short {
        // rotate right by one, then add -- u16 so the carry is not lost early
        let rotated = (if sum & 1 != 0 { 0x80u16 } else { 0x00u16 })
            + u16::from(sum >> 1)
            + u16::from(b);
        sum = (rotated & 0xFF) as u8;
    }
    sum
}

/// Build the LFN slots for `name`, in **physical on-disk order**.
///
/// Two details are easy to get wrong and both silently lose the name:
///
///   * The slots run **in reverse**: the slot physically immediately before the
///     short entry carries order 1 (the LAST characters), not the first.
///   * Only the slot with order 1 sets the `LAST_LONG` bit (0x40) in **byte 0**,
///     it is the slot adjacent to the short entry. (Byte 12 is the LFN "type"
///     field and stays 0; setting 0x40 there is the 1.2 bug, and it produces
///     slots that read back fine here but tell no other reader where to stop.)
///   * Characters are **UTF-16LE** and stored at the odd offsets
///     (1..10, 14..25, 28..31); the bytes in between are reserved and zero.
///
/// Each slot holds 13 UTF-16 code units; the final slot is zero-padded, and that
/// zero padding is itself how the reader knows the name ended.
fn lfn_slots(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let sum = lfn_checksum(short);

    // UTF-16 code units of the name, with trailing spaces and dots removed:
    // Windows will not show "FILE." or "FILE   " as the name.
    let trimmed = name.trim_end_matches([' ', '.']);
    let units: Vec<u16> = trimmed.encode_utf16().collect();

    let count = units.len().div_ceil(13).max(1);
    let mut slots = Vec::with_capacity(count);
    // Physical order is count, count-1, ... 1.
    for order in (1..=count).rev() {
        let mut slot = [0u8; 32];
        // LAST_LONG is bit 6 of the SEQUENCE byte (byte 0), not byte 12: byte 12
        // is the LFN "type" byte and is 0 for these entries. Writing it to 12
        // produced slots that were self-consistent enough to read back here
        // (our reader took the order from byte 0 and ignored bit 6) but that
        // told Windows nothing — a reader that scans backwards from the short
        // entry until LAST_LONG would never stop.
        slot[0] = order as u8 | if order == 1 { LFN_LAST } else { 0 };
        slot[11] = ATTR_LFN;
        slot[13] = sum;
        // Slot `order` holds characters [(order-1)*13 .. order*13). This used to
        // be `(order - 1) * 13` against a physical loop that walks order DOWN,
        // which put the LAST chunk in the highest-numbered (physically first)
        // slot. The spec is the other way round: the highest-numbered slot holds
        // the FIRST characters, and order 1 -- the slot adjacent to the short
        // entry -- holds the last ones. Found by the independent host-side
        // verifier, which decoded "InteropTwentyCharsLong" as "CharsLong".
        let start = (count - order) * 13;
        for i in 0..13 {
            let u = units.get(start + i).copied().unwrap_or(0);
            // Offsets 1..10 hold units 0..4; 14..25 hold 5..10; 28..31 hold 11..12.
            let off = if i < 5 {
                1 + i * 2
            } else if i < 11 {
                14 + (i - 5) * 2
            } else {
                28 + (i - 11) * 2
            };
            slot[off..off + 2].copy_from_slice(&u.to_le_bytes());
        }
        // Bytes 26..27 (first cluster) stay 0 — an LFN entry belongs to no
        // cluster; only the short entry carries that.
        slots.push(slot);
    }
    slots
}

/// Derive a unique 8.3 alias for a long name: `NAME~1.EXT`, `NAME~2.EXT`, ...
///
/// The alias must be unique in its directory or two different long names would
/// collide on the same short entry. `exists` reports whether a candidate is
/// already taken.
fn make_unique_short(name: &str, exists: impl Fn(&str) -> bool) -> Result<[u8; 11], FsError> {
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (b, e),
        _ => (name, ""),
    };
    // Keep the base to 6 chars so "~N" always fits in the 8-char field even
    // when the tail reaches two digits.
    let stem: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '~'))
        .take(6)
        .collect();
    let stem = if stem.is_empty() { "FILE".to_string() } else { stem };
    let ext: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(3)
        .collect();

    for n in 1..100u32 {
        let cand = if ext.is_empty() {
            alloc::format!("{stem}~{n}")
        } else {
            alloc::format!("{stem}~{n}.{ext}")
        };
        if !exists(&cand) {
            // The candidate is already 8.3-legal by construction, so this
            // cannot fail on validation.
            return make_short_name(&cand);
        }
    }
    Err(FsError::NoSpace)
}

/// P16-1.2 self-test: long filenames must survive a real create + re-read.
///
/// Pure round-tripping through the in-memory helpers would prove nothing — the
/// whole failure mode is the ON-DISK layout (slot order, the LAST_LONG bit, the
/// checksum, contiguity with the short entry). So this creates real files and
/// reads the directory back through the same [`Self::scan_dir`] a caller uses.
///
/// Runs at boot from `vfs::init` and writes into the root directory, so the
/// created names are removed again at the end; a failure that leaves debris
/// would otherwise show up in the shell's `ls` and every later test.
pub fn lfn_self_test(dev: &mut AtaDrive, part_start: u64) {
    // Names chosen to exercise each branch of the LFN layout:
    //  - 11 chars: still ONE slot, first slot is also the last;
    //  - 20 chars: TWO slots, so the reverse ordering is actually load-bearing;
    //  - lowercase + spaces: forces the case-mapped 8.3 alias and proves the
    //    long name (not the alias) is what comes back.
    let cases = [
        ("SHORTNAME.TXT", "11 chars - one LFN slot"),
        ("A Very Long Document Name.txt", "20 chars - two slots, reverse order"),
        ("mixed case.txt", "lowercase must round-trip"),
    ];

    let fs = match Fat32::mount(dev, part_start) {
        Ok(f) => f,
        Err(e) => {
            crate::serial_writeln!("[lfn] SKIP: FAT32 not mountable at LBA {part_start}: {e:?}");
            return;
        }
    };

    let mut pass = 0usize;
    let mut bad = 0usize;

    for (name, why) in cases {
        let full = alloc::format!("/{name}");
        match fs.write_at(dev, &full, 0, name.as_bytes()) {
            Err(e) => {
                bad += 1;
                crate::serial_writeln!("[lfn] BAD  create {name:?} ({why}): {e:?}");
                continue;
            }
            Ok(_) => {}
        }
        // Re-read the directory and look for the name EXACTLY as written. This
        // is the assertion that matters: the reader must prefer the LFN run and
        // ignore the generated alias.
        let listed = fs.list(dev, "/");
        let found = listed.as_ref().map(|v| {
            v.iter()
                .any(|e| e.name.eq_ignore_ascii_case(name))
        });
        match found {
            Ok(true) => {
                pass += 1;
                crate::serial_writeln!("[lfn] PASS {name:?} round-trips ({why})");
            }
            Ok(false) => {
                bad += 1;
                crate::serial_writeln!("[lfn] BAD  {name:?} not found after create ({why})");
                crate::serial_writeln!("[lfn]   listing had {} entries", listed.as_ref().map(|v| v.len()).unwrap_or(0));
            }
            Err(e) => {
                bad += 1;
                crate::serial_writeln!("[lfn] BAD  list after {name:?}: {e:?}");
            }
        }
    }

    // A second file whose alias would be the SAME base must get ~2, not ~1,
    // and both names must still be readable. This is the collision case the
    // `make_unique_short` probe exists for.
    let dupes = ["Report Final.txt", "Report Draft.txt"];
    let mut dupe_ok = true;
    for d in dupes {
        let full = alloc::format!("/{d}");
        if fs.write_at(dev, &full, 0, d.as_bytes()).is_err() {
            dupe_ok = false;
        }
    }
    if dupe_ok {
        let listed = fs.list(dev, "/").unwrap_or_default();
        let both = dupes
            .iter()
            .all(|d| listed.iter().any(|e| e.name.eq_ignore_ascii_case(d)));
        // Aliases must differ, or one file's short entry would shadow the other.
        // `DirEntry` carries no alias, so resolve the two PREDICTED aliases and
        // require them to land on different directory slots. (Comparing the long
        // names, as this once did, is trivially true and proves nothing.)
        let aliases_differ = match (
            fs.find_in(dev, fs.root_cluster, "Report~1.txt"),
            fs.find_in(dev, fs.root_cluster, "Report~2.txt"),
        ) {
            (Ok(Some(a)), Ok(Some(b))) => a.dir_index != b.dir_index,
            _ => false,
        };
        if both && aliases_differ {
            pass += 1;
            crate::serial_writeln!("[lfn] PASS colliding bases get distinct aliases");
        } else {
            bad += 1;
            crate::serial_writeln!("[lfn] BAD  colliding bases: both={both} distinct={aliases_differ}");
        }
    }

    // Clean up: the volume is a shared test fixture and every later `ls` would
    // otherwise show these.
    for n in cases.iter().map(|(n, _)| *n).chain(dupes) {
        let _ = fs.unlink(dev, &alloc::format!("/{n}"));
    }

    // Rename must not leave the OLD long name visible. `rename` rewrites only
    // the short bytes, so the stale LFN run stays in front of the entry; the
    // reader has to reject it on the checksum mismatch and fall back to the new
    // short name. Without that check the entry keeps reporting the name it was
    // renamed AWAY from — silent, and it reads like "rename did nothing".
    let from = "/mixed case.txt";
    let to = "/RENAMED.TXT";
    let created = fs.write_at(dev, from, 0, b"x");
    let rename_res = match created {
        Ok(_) => fs.rename(dev, from, to),
        Err(e) => Err(e),
    };
    match rename_res {
        Ok(()) => {
            let listed = fs.list(dev, "/").unwrap_or_default();
            let old_gone = !listed
                .iter()
                .any(|e| e.name.eq_ignore_ascii_case("mixed case.txt"));
            let new_there = listed
                .iter()
                .any(|e| e.name.eq_ignore_ascii_case("RENAMED.TXT"));
            if old_gone && new_there {
                pass += 1;
                crate::serial_writeln!("[lfn] PASS rename drops the stale long name");
            } else {
                bad += 1;
                crate::serial_writeln!(
                    "[lfn] BAD  rename: old_gone={old_gone} new_there={new_there}"
                );
            }
            let _ = fs.unlink(dev, to);
        }
        Err(e) => {
            bad += 1;
            crate::serial_writeln!("[lfn] BAD  rename {from} -> {to} failed: {e:?}");
        }
    }

    // P16-1.2.1: rename must PRESERVE a long name, not degrade it to 8.3.
    // The previous implementation rewrote only the short-name bytes in place, so
    // this case used to leave the file listed as "RENAMED~1.TXT" and the long
    // name gone. This is the regression test for it.
    const LONG_FROM: &str = "Quarterly Financial Report 2026.xlsx";
    const LONG_TO: &str = "Annual Consolidated Statement.xlsx";
    let mut long_ok = false;
    if fs
        .write_at(dev, &alloc::format!("/{LONG_FROM}"), 0, b"payload")
        .is_ok()
    {
        if fs
            .rename(
                dev,
                &alloc::format!("/{LONG_FROM}"),
                &alloc::format!("/{LONG_TO}"),
            )
            .is_ok()
        {
            let listed = fs.list(dev, "/").unwrap_or_default();
            long_ok = listed.iter().any(|e| e.name.eq_ignore_ascii_case(LONG_TO))
                && !listed.iter().any(|e| e.name.eq_ignore_ascii_case(LONG_FROM));
        }
    }
    if long_ok {
        pass += 1;
        crate::serial_writeln!("[lfn] PASS rename preserves a long filename");
    } else {
        bad += 1;
        crate::serial_writeln!("[lfn] BAD  rename lost the long filename (fell back to 8.3?)");
    }

    // Renaming a long name to ITSELF must be idempotent. The alias probe has to
    // exclude the entry's OWN alias; without that every self-rename sees its
    // `~1` as taken and bumps it to `~2`, then `~3`, and the name drifts away
    // from the user every time a config loader calls rename before writing.
    const SELF: &str = "Idempotency Probe Name.txt";
    let mut self_ok = false;
    if fs
        .write_at(dev, &alloc::format!("/{SELF}"), 0, b"z")
        .is_ok()
    {
        let r1 = fs.rename(dev, &alloc::format!("/{SELF}"), &alloc::format!("/{SELF}"));
        let r2 = fs.rename(dev, &alloc::format!("/{SELF}"), &alloc::format!("/{SELF}"));
        self_ok = r1.is_ok() && r2.is_ok() && {
            let listed = fs.list(dev, "/").unwrap_or_default();
            listed.iter().any(|e| e.name.eq_ignore_ascii_case(SELF))
        };
    }
    if self_ok {
        pass += 1;
        crate::serial_writeln!("[lfn] PASS rename to the same long name is idempotent");
    } else {
        bad += 1;
        crate::serial_writeln!("[lfn] BAD  rename to the same long name lost it");
    }

    let _ = fs.unlink(dev, &alloc::format!("/{LONG_TO}"));
    let _ = fs.unlink(dev, &alloc::format!("/{SELF}"));

    // P16-1.2.1: creating a file into a DELETED slot must not destroy the entry
    // that FOLLOWS it. The old `write_run` wrote 0x00 into the next slot to
    // "re-terminate" the directory, so creating D into B's freed slot erased C
    // from the directory while leaving C's data clusters allocated -- silent
    // loss of a file that was never touched.
    //
    // Deterministic because `alloc_run` always takes the LOWEST free slot: A, B
    // and C land on three consecutive slots, deleting B frees the middle one,
    // and creating D is therefore forced back into exactly that slot.
    const HOLE_A: &str = "Zguardalpha.txt";
    const HOLE_B: &str = "Zguardbravo.txt";
    const HOLE_C: &str = "Zguardcharlie.txt";
    const HOLE_D: &str = "Zguarddelta.txt";
    let mut hole_ok = false;
    let made = [HOLE_A, HOLE_B, HOLE_C]
        .iter()
        .all(|n| fs.write_at(dev, &alloc::format!("/{n}"), 0, b"x").is_ok());
    if made
        && fs.unlink(dev, &alloc::format!("/{HOLE_B}")).is_ok()
        && fs
            .write_at(dev, &alloc::format!("/{HOLE_D}"), 0, b"x")
            .is_ok()
    {
        let listed = fs.list(dev, "/").unwrap_or_default();
        let has = |n: &str| listed.iter().any(|e| e.name.eq_ignore_ascii_case(n));
        hole_ok = has(HOLE_A) && has(HOLE_C) && has(HOLE_D) && !has(HOLE_B);
    }
    for n in [HOLE_A, HOLE_C, HOLE_D] {
        let _ = fs.unlink(dev, &alloc::format!("/{n}"));
    }
    if hole_ok {
        pass += 1;
        crate::serial_writeln!("[lfn] PASS creating into a freed slot keeps the next entry");
    } else {
        bad += 1;
        crate::serial_writeln!("[lfn] BAD  creating into a freed slot destroyed a neighbour");
    }

    // P16-1.3: timestamps. Two properties that are easy to invert and that no
    // round-trip test can see, because our writer and reader agree either way:
    //   * a WRITE must advance mtime;
    //   * a RENAME must NOT -- renaming is not an edit.
    const TS_FROM: &str = "Timestamp Probe.txt";
    const TS_TO: &str = "Timestamp Probe Renamed.txt";
    let ts_path = alloc::format!("/{TS_FROM}");
    let ts_to = alloc::format!("/{TS_TO}");
    let mut ts_ok = false;
    if fs.write_at(dev, &ts_path, 0, b"one").is_ok() {
        let created = fs.stat(dev, &ts_path).ok();
        // Extend the file so the size changes and `write_dirent` runs.
        let rewrote = fs.write_at(dev, &ts_path, 3, b"more").is_ok();
        let after_write = fs.stat(dev, &ts_path).ok();
        let renamed = fs.rename(dev, &ts_path, &ts_to).is_ok();
        let after_rename = fs.stat(dev, &ts_to).ok();

        ts_ok = match (created, after_write, after_rename) {
            (Some(a), Some(b), Some(c)) => {
                // Bounds chosen so a zero stamp (no RTC), a 1970 default and
                // FAT's 2107 ceiling all fail -- "non-zero" would not.
                let plausible = |t: u64| (1_600_000_000..4_000_000_000).contains(&t);
                plausible(a.mtime)
                    && plausible(a.ctime)
                    && plausible(a.atime)
                    // FAT stores 2-second granularity, so an advance inside the
                    // same tick is invisible: >= is the strongest honest claim.
                    && (!rewrote || b.mtime >= a.mtime)
                    && b.ctime >= a.ctime
                    && (c.mtime == b.mtime)
                    && (c.ctime == b.ctime)
                    && (c.atime == b.atime)
            }
            _ => false,
        };
        let _ = fs.unlink(dev, &ts_to);
    }
    if ts_ok {
        pass += 1;
        crate::serial_writeln!("[lfn] PASS timestamps: write advances mtime, rename preserves it");
    } else {
        bad += 1;
        crate::serial_writeln!("[lfn] BAD  timestamp semantics wrong");
    }

    crate::serial_writeln!("[lfn] done: {pass} passed, {bad} bad");
}

/// Validate + convert a name to FAT's on-disk 11-byte 8.3 form.
///
/// P16-1.2: this still REJECTS a name that is not 8.3-representable — callers go
/// through [`make_unique_short`] first to derive an alias, and that keeps the
/// "is this a legal short name" rule in exactly one place instead of being
/// duplicated across create and rename.
fn make_short_name(name: &str) -> Result<[u8; 11], FsError> {
    if name.contains('/') || name.contains('\\') {
        return Err(FsError::NotSupported);
    }
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (b, e),
        _ => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || name.len() > 12 {
        return Err(FsError::NotSupported);
    }
    let mut out = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        if !b.is_ascii_alphanumeric() && !matches!(b, b'_' | b'-' | b'~') {
            return Err(FsError::NotSupported);
        }
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        if !b.is_ascii_alphanumeric() {
            return Err(FsError::NotSupported);
        }
        out[8 + i] = b.to_ascii_uppercase();
    }
    Ok(out)
}

/// Bridge to the VFS trait: the kernel mounts Fat32 as a `dyn FileSystem`.
/// (Inherent methods take precedence in resolution, so these thin wrappers
/// call the identically-named inherent ones.)
impl crate::vfs::FileSystem for Fat32 {
    /// P16-1.2: dispatch to the module-level self-test with the partition's
    /// real start LBA. The offset matters: mounting at 0 would read the MBR as a
    /// FAT boot sector and report `BadFs`, which is exactly what happened the
    /// first time this was wired up — the test "passed" by skipping.
    fn lfn_self_test(&mut self, dev: &mut AtaDrive) {
        lfn_self_test(dev, self.part_start);
    }

    fn list(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
    ) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        Fat32::list(self, dev, path)
    }

    fn open(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Fat32::open(self, dev, path)
    }

    fn read_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        Fat32::read_at(self, dev, path, off, buf)
    }

    fn write_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        Fat32::write_at(self, dev, path, off, buf)
    }

    fn mkdir(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Fat32::mkdir(self, dev, path)
    }

    fn stat(&mut self, dev: &mut AtaDrive, path: &str) -> Result<crate::vfs::FileStat, FsError> {
        Fat32::stat(self, dev, path)
    }

    fn fsync(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Fat32::fsync(self, dev, path)
    }

    fn unlink(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Fat32::unlink(self, dev, path)
    }

    fn rename(&mut self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        Fat32::rename(self, dev, from, to)
    }
}