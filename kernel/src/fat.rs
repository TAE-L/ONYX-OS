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
const ATTR_DIR: u8 = 0x10;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_ARCHIVE: u8 = 0x20;
/// End-of-chain marker we write; any value >= 0x0FFFFFF8 reads back as EOC.
const EOC: u32 = 0x0FFF_FFFF;

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
    part_start: u32,
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
    pub fn mount(dev: &mut AtaDrive, part_start: u32) -> Result<Self, FsError> {
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

    fn cluster_lba(&self, c: u32) -> u32 {
        self.part_start
            + u32::from(self.reserved)
            + u32::from(self.num_fats) * self.fat_sectors
            + (c - 2) * u32::from(self.sectors_per_cluster)
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
        let lba = self.part_start + u32::from(self.reserved) + (off / SECTOR_SIZE) as u32;
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
                + u32::from(self.reserved)
                + u32::from(f) * self.fat_sectors
                + (off / SECTOR_SIZE) as u32;
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
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        for (ci, &c) in chain.iter().enumerate() {
            self.read_cluster(dev, c, &mut buf)?;
            for (si, slot) in buf.chunks_exact(32).enumerate() {
                match slot[0] {
                    0x00 => return Ok(out), // end-of-directory marker
                    0xE5 => {
                        lfn.clear(); // deleted slot: reusable
                        continue;
                    }
                    _ => {}
                }
                let attr = slot[11];
                if attr == ATTR_LFN {
                    let mut part = [0u8; 26];
                    part[..10].copy_from_slice(&slot[1..11]);
                    part[10..24].copy_from_slice(&slot[14..28]);
                    part[24..26].copy_from_slice(&slot[28..30]);
                    lfn.push((slot[0] & 0x1F, part));
                    continue;
                }
                if attr & ATTR_VOLUME != 0 {
                    lfn.clear(); // volume label entry
                    continue;
                }
                // Regular (short) entry — any pending LFN belongs to it.
                let short = short_name(slot);
                let name = if lfn.is_empty() {
                    short.clone()
                } else {
                    lfn_name(&lfn)
                };
                lfn.clear();
                let cluster_lo = u16::from_le_bytes(slot[26..28].try_into().unwrap());
                let cluster_hi = u16::from_le_bytes(slot[20..22].try_into().unwrap());
                out.push(RawEntry {
                    name,
                    short,
                    first_cluster: (u32::from(cluster_hi) << 16) | u32::from(cluster_lo),
                    size: u32::from_le_bytes(slot[28..32].try_into().unwrap()),
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

/// Assemble an LFN (long file name) from its 26-byte parts, ordered by
/// sequence number; terminates at U+0000 / U+FFFF padding.
fn lfn_name(parts: &[(u8, [u8; 26])]) -> String {
    let mut sorted = parts.to_vec();
    sorted.sort_by_key(|p| p.0);
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

    /// Create an entry (file or directory) named `name` in the directory at
    /// `dir_cluster`. `first_cluster` is the entry's start cluster (0 = empty
    /// file); names must be valid 8.3 short names.
    fn create_entry(
        &self,
        dev: &mut AtaDrive,
        dir_cluster: u32,
        name: &str,
        first_cluster: u32,
        is_dir: bool,
    ) -> Result<RawEntry, FsError> {
        let short = make_short_name(name)?;
        let mut chain = self.walk_chain(dev, dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as u32;
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];

        // Find a free slot: 0xE5 (deleted) or 0x00 (end-of-directory marker).
        let mut target: Option<(usize, usize)> = None;
        'outer: for (ci, &c) in chain.iter().enumerate() {
            self.read_cluster(dev, c, &mut buf)?;
            for (si, slot) in buf.chunks_exact(32).enumerate() {
                if slot[0] == 0x00 || slot[0] == 0xE5 {
                    target = Some((ci, si));
                    break 'outer;
                }
            }
        }
        let (ci, si) = match target {
            Some(t) => t,
            None => {
                // Directory full: extend its chain with a zeroed cluster.
                let prev = chain.last().copied();
                let c = self.alloc_cluster(dev, prev)?;
                chain.push(c);
                (chain.len() - 1, 0)
            }
        };
        let dir_index = ci as u32 * slots_per_cluster + si as u32;

        let mut slot = [0u8; 32];
        slot[..11].copy_from_slice(&short);
        slot[11] = if is_dir { ATTR_DIR } else { ATTR_ARCHIVE };
        slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
        // timestamps stay zero (FAT tolerates missing dirent dates)

        self.read_cluster(dev, chain[ci], &mut buf)?;
        buf[si * 32..si * 32 + 32].copy_from_slice(&slot);
        self.write_cluster(dev, chain[ci], &buf)?;

        Ok(RawEntry {
            name: name.to_string(),
            short: name.to_string(),
            first_cluster,
            size: 0,
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
        self.write_cluster(dev, chain[ci], &buf)
    }

    /// Create the directory at `path`: parent must exist, the final name must
    /// be a free 8.3 short name. Allocates one cluster, initializes the
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
            }),
            Some(Resolution::Entry(e)) => Ok(crate::vfs::FileStat {
                size: u64::from(e.size),
                is_dir: e.is_dir,
            }),
            None => Err(FsError::NotFound),
        }
    }

    /// Mark one directory slot deleted (0xE5) and free its cluster chain.
    fn delete_entry(&self, dev: &mut AtaDrive, e: &RawEntry) -> Result<(), FsError> {
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
        self.write_cluster(dev, chain[ci], &buf)?;
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

    /// Same-directory rename: rewrite the source's 8.3 slot name in place.
    /// Cross-directory moves are rejected (the source's slots and cluster
    /// chain belong to one parent; moving would need copy + delete).
    pub fn rename(&self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        let e = match self.resolve(dev, from)? {
            Some(Resolution::Entry(e)) => e,
            Some(Resolution::Root) => return Err(FsError::NotSupported),
            None => return Err(FsError::NotFound),
        };
        if self.resolve(dev, to)?.is_some() {
            return Err(FsError::Exists);
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
        let short = make_short_name(&to_name)?;

        let chain = self.walk_chain(dev, e.dir_cluster, self.total_clusters)?;
        let slots_per_cluster = (self.cluster_bytes() / 32) as usize;
        let ci = e.dir_index as usize / slots_per_cluster;
        let si = e.dir_index as usize % slots_per_cluster;
        if ci >= chain.len() {
            return Err(FsError::BadFs);
        }
        let mut buf = alloc::vec![0u8; self.cluster_bytes()];
        self.read_cluster(dev, chain[ci], &mut buf)?;
        // Rewrite only the 8.3 name bytes; cluster/size/attr/timestamps stay.
        buf[si * 32..si * 32 + 11].copy_from_slice(&short);
        self.write_cluster(dev, chain[ci], &buf)?;
        Ok(())
    }
}

/// Validate + convert a name to FAT's on-disk 11-byte 8.3 form.
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

    fn unlink(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Fat32::unlink(self, dev, path)
    }

    fn rename(&mut self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        Fat32::rename(self, dev, from, to)
    }
}