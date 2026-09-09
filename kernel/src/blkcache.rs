//! M9.6-A5: sector cache for the ATA block layer.
//!
//! Every FS read/write funnels through `AtaDrive::read_sectors` /
//! `write_sectors` — now cache-mediated — so FAT32 + ext2 benefit with zero
//! driver changes. Cache policy:
//!   * 2048 × 512 B entries (1 MiB), second-chance clock LRU.
//!   * Write-through: writes hit the disk FIRST, then the cache, so the
//!     cache is never dirty and a "flush" only invalidates.
//!   * Sequential read-ahead (4 sectors) warms the cache during contiguous
//!     FS reads (cluster/block chains that are physically contiguous).
//!
//! Lock rules (inherited from the existing DISK/FS spin-lock discipline):
//! the cache lock is only ever held with interrupts disabled (syscall
//! context IF=0, boot, or the boot-time `blk_test`). A task must NOT hold it
//! across a preemption, or another task spinning on it would deadlock.

use spin::Mutex;

use crate::ata::{AtaDrive, AtaError, SECTOR_SIZE};

/// Cache entries: 2048 × 512 B = 1 MiB of sector data.
pub const CACHE_ENTRIES: usize = 2048;
/// Sectors to prefetch after a detected sequential read.
pub const READ_AHEAD: usize = 4;

struct Entry {
    lba: u32,
    valid: bool,
    /// Second-chance bit: set on access, cleared on a sweep pass.
    dwell: bool,
    data: [u8; SECTOR_SIZE],
}

const ZERO_ENTRY: Entry = Entry {
    lba: 0,
    valid: false,
    dwell: false,
    data: [0; SECTOR_SIZE],
};

struct Cache {
    entries: [Entry; CACHE_ENTRIES],
    /// Next slot to examine for eviction (clock hand).
    next: usize,
    hits: usize,
    misses: usize,
    /// End LBA (lba+count) of the last read; set when that read was a
    /// sequential continuation of the previous one.
    last_seq_end: u32,
    seq_valid: bool,
}

impl Cache {
    const fn new() -> Self {
        Self {
            entries: [ZERO_ENTRY; CACHE_ENTRIES],
            next: 0,
            hits: 0,
            misses: 0,
            last_seq_end: 0,
            seq_valid: false,
        }
    }

    /// Look up a sector, returning its data as a copy so the caller's borrow
    /// of the cache ends before any later `hits += 1` / `insert`.
    fn find(&mut self, lba: u32) -> Option<[u8; SECTOR_SIZE]> {
        self.entries
            .iter_mut()
            .find(|e| e.valid && e.lba == lba)
            .map(|e| {
                e.dwell = true;
                e.data
            })
    }

    /// Check presence without touching the dwell bit (RA + tests).
    fn contains(&self, lba: u32) -> bool {
        self.entries.iter().any(|e| e.valid && e.lba == lba)
    }

    /// Insert (second-chance eviction when full). If this LBA is already
    /// cached, the entry is updated IN PLACE — otherwise a write of a
    /// previously-read sector would leave a stale duplicate that `find`
    /// (first-match scan) would keep returning.
    fn insert(&mut self, lba: u32, data: &[u8; SECTOR_SIZE]) {
        for e in self.entries.iter_mut() {
            if e.valid && e.lba == lba {
                e.data.copy_from_slice(data);
                e.dwell = true;
                return;
            }
        }
        loop {
            let e = &mut self.entries[self.next];
            self.next = (self.next + 1) % CACHE_ENTRIES;
            if !e.valid {
                e.lba = lba;
                e.valid = true;
                e.dwell = true;
                e.data.copy_from_slice(data);
                return;
            }
            if e.dwell {
                // Give it one more chance; keep sweeping.
                e.dwell = false;
                continue;
            }
            // Evict (not dirty: write-through keeps the cache clean).
            e.lba = lba;
            e.data.copy_from_slice(data);
            e.valid = true;
            e.dwell = true;
            return;
        }
    }
}

static CACHE: Mutex<Cache> = Mutex::new(Cache::new());

/// Cached read of `count` sectors at `lba` into `buf` (cache hits + coalesced
/// raw misses + sequential read-ahead). Runs with IF=0.
pub fn read(dev: &mut AtaDrive, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), AtaError> {
    if count == 0 || buf.len() < count as usize * SECTOR_SIZE {
        return dev.read_sectors_raw(lba, count, buf);
    }
    let mut c = CACHE.lock();
    let sequential = c.seq_valid && lba == c.last_seq_end;

    let cnt = u32::from(count);
    let mut i: u32 = 0;
    while i < cnt {
        let s = lba + i;
        match c.find(s) {
            Some(data) => {
                let dst = i as usize * SECTOR_SIZE;
                buf[dst..dst + SECTOR_SIZE].copy_from_slice(&data);
                c.hits += 1;
                i += 1;
            }
            None => {
                // Coalesce consecutive miss sectors into one raw read.
                let start = s;
                let mut run: u32 = 1;
                while i + run < cnt && !c.contains(lba + i + run) {
                    run += 1;
                }
                let from = i as usize * SECTOR_SIZE;
                let to = from + run as usize * SECTOR_SIZE;
                dev.read_sectors_raw(start, run as u8, &mut buf[from..to])?;
                for k in 0..run {
                    let off = from + k as usize * SECTOR_SIZE;
                    let mut sec = [0u8; SECTOR_SIZE];
                    sec.copy_from_slice(&buf[off..off + SECTOR_SIZE]);
                    c.insert(start + k, &sec);
                }
                c.misses += run as usize;
                i += run;
            }
        }
    }
    c.seq_valid = true;
    c.last_seq_end = lba + cnt;

    // Read-ahead on a sequential continuation (best-effort past EOF).
    if sequential {
        let mut scratch = [0u8; READ_AHEAD * SECTOR_SIZE];
        let mut k: u32 = 0;
        while (k as usize) < READ_AHEAD {
            let s = c.last_seq_end + k;
            if c.contains(s) {
                k += 1;
                continue;
            }
            let start = s;
            let mut run: u32 = 1;
            while (run as usize) < READ_AHEAD - (k as usize) && !c.contains(s + run) {
                run += 1;
            }
            let from = k as usize * SECTOR_SIZE;
            let to = from + run as usize * SECTOR_SIZE;
            if dev.read_sectors_raw(start, run as u8, &mut scratch[from..to]).is_ok() {
                for j in 0..run {
                    let off = from + j as usize * SECTOR_SIZE;
                    let mut sec = [0u8; SECTOR_SIZE];
                    sec.copy_from_slice(&scratch[off..off + SECTOR_SIZE]);
                    c.insert(start + j, &sec);
                }
            }
            k += run;
        }
    }
    Ok(())
}

/// Cached (write-through) write of `count` sectors: disk first, then cache.
/// Runs with IF=0.
pub fn write(dev: &mut AtaDrive, lba: u32, count: u8, buf: &[u8]) -> Result<(), AtaError> {
    dev.write_sectors_raw(lba, count, buf)?;
    let mut c = CACHE.lock();
    for k in 0..u32::from(count) {
        let off = k as usize * SECTOR_SIZE;
        let mut sec = [0u8; SECTOR_SIZE];
        sec.copy_from_slice(&buf[off..off + SECTOR_SIZE]);
        c.insert(lba + k, &sec);
    }
    // A write breaks the sequential-read tuple.
    c.seq_valid = false;
    Ok(())
}

/// Invalidate the entire cache (SYS_FLUSH / `sync`). Write-through means
/// nothing is pending on disk.
pub fn flush() {
    let mut c = CACHE.lock();
    for e in c.entries.iter_mut() {
        e.valid = false;
        e.dwell = false;
    }
    c.seq_valid = false;
}

/// Reset hit/miss counters (used around the boot-time block benchmark).
pub fn reset_stats() {
    let mut c = CACHE.lock();
    c.hits = 0;
    c.misses = 0;
}

/// (hits, misses) since the last reset.
pub fn stats() -> (usize, usize) {
    let c = CACHE.lock();
    (c.hits, c.misses)
}

/// Is sector `lba` currently cached? (Tests.)
pub fn is_cached(lba: u32) -> bool {
    let c = CACHE.lock();
    c.contains(lba)
}

/// End LBA of the most recent sequential read, if any. (Tests.)
pub fn last_seq_end() -> Option<u32> {
    let c = CACHE.lock();
    c.seq_valid.then_some(c.last_seq_end)
}