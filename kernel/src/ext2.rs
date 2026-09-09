//! M7: ext2 filesystem driver (revision 1, 1 KiB blocks, single group).
//!
//! Mounted alongside FAT32 by `vfs::init` and exercised by the kernel-side
//! M7 self-test; implements the same `FileSystem` trait, so ext2 slots in
//! without touching the syscall ABI.
//!
//! Supported: subdirectory-aware listing/lookup/creation (M8), file
//! read/write with direct + single/double indirect block mapping, bitmap-
//! based block and inode allocation (with superblock / group-descriptor
//! free-count sync), read-modify-write on partial blocks. Triple-indirect
//! blocks and multi-group volumes are rejected (`NotSupported`).
//!
//! On-disk assumptions (matching tools/mkext2): 1 KiB blocks
//! (`s_log_block_size` = 0), 128-byte revision-1 inodes, `first_data_block`
//! = 1, one block group, no directory filetype feature (entries carry a
//! zero filetype byte and `name_len` is 8-bit).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ata::{AtaDrive, SECTOR_SIZE};
use crate::vfs::FsError;

const SUPER_MAGIC: u16 = 0xEF53;
/// Fixed block size (`s_log_block_size` == 0).
const BLOCK: usize = 1024;
/// Root directory inode (fixed by the ext2 spec).
const ROOT_INO: u32 = 2;
/// Direct block pointers per inode (`i_block[0..12]`).
const DIRECT: usize = 12;
/// u32 pointers per indirect block.
const PTRS: usize = BLOCK / 4;
const MODE_DIR: u16 = 0x4000;
const MODE_REG: u16 = 0x8000;

pub struct Ext2 {
    part_start: u32,
    blocks_count: u32,
    inodes_count: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    /// Cached free counts; synced to the superblock + GD on every allocation.
    free_blocks: u32,
    free_inodes: u32,
}

/// The inode fields the driver needs, parsed out of the inode table.
#[derive(Clone, Copy)]
struct Inode {
    ino: u32,
    mode: u16,
    size: u32,
    /// `i_blocks`: 512-byte sectors occupied (including indirect blocks).
    sectors: u32,
    /// `i_block[0..15]`.
    blocks: [u32; 15],
}

/// One directory entry as found on disk. Free slots (inode 0) are included:
/// the directory allocator reuses them when inserting new names.
struct DirEnt {
    ino: u32,
    name: String,
    rec_len: u16,
    /// Byte offset of this entry within the directory's data.
    offset: u32,
}

impl Ext2 {
    /// Parse and validate the superblock + group descriptor 0 of the
    /// partition at `part_start` (LBA units, 512-byte sectors).
    pub fn mount(dev: &mut AtaDrive, part_start: u32) -> Result<Self, FsError> {
        // The superblock lives 1024 bytes into the partition: block 1 for
        // 1 KiB blocks (LBA part_start + 2).
        let mut sb = [0u8; BLOCK];
        read_part_block(dev, part_start, 1, &mut sb)?;
        let g16 = |o: usize| u16::from_le_bytes(sb[o..o + 2].try_into().unwrap());
        let g32 = |o: usize| u32::from_le_bytes(sb[o..o + 4].try_into().unwrap());
        if g16(56) != SUPER_MAGIC {
            return Err(FsError::BadFs);
        }
        if g32(24) != 0 {
            return Err(FsError::NotSupported); // block size != 1 KiB
        }
        if g32(20) != 1 {
            return Err(FsError::BadFs); // first_data_block must be 1
        }
        let inode_size = if g32(76) == 0 { 128 } else { g16(88) };
        if inode_size != 128 {
            return Err(FsError::NotSupported);
        }
        let (blocks_count, inodes_count) = (g32(4), g32(0));
        let (blocks_per_group, inodes_per_group) = (g32(32), g32(40));
        if blocks_count == 0
            || inodes_count == 0
            || blocks_per_group == 0
            || inodes_per_group == 0
            || blocks_per_group > 8 * BLOCK as u32
        {
            return Err(FsError::BadFs); // bitmap must fit in one block
        }
        let groups = (blocks_count - 1).div_ceil(blocks_per_group);
        if groups != 1 {
            return Err(FsError::NotSupported); // mkext2 volumes are single-group
        }
        let mut gd = [0u8; BLOCK];
        read_part_block(dev, part_start, 2, &mut gd)?;
        // Spec ext2 GD32 layout: u32 pointers at 0/4/8, u16 free counts at
        // 12/14 (the kernel's sync_free_counts writes the same fields).
        let gd32 = |o: usize| u32::from_le_bytes(gd[o..o + 4].try_into().unwrap());
        let (block_bitmap, inode_bitmap, inode_table) = (gd32(0), gd32(4), gd32(8));
        if block_bitmap == 0 || inode_bitmap == 0 || inode_table == 0 {
            return Err(FsError::BadFs);
        }
        for b in [block_bitmap, inode_bitmap, inode_table] {
            if b >= blocks_count {
                return Err(FsError::BadFs);
            }
        }
        Ok(Self {
            part_start,
            blocks_count,
            inodes_count,
            blocks_per_group,
            inodes_per_group,
            inode_size,
            block_bitmap,
            inode_bitmap,
            inode_table,
            free_blocks: g32(12),
            free_inodes: g32(16),
        })
    }

    /// One-line summary for boot logs.
    pub fn summary(&self) -> String {
        alloc::format!(
            "blocks={} (free {}) inodes={} (free {})",
            self.blocks_count,
            self.free_blocks,
            self.inodes_count,
            self.free_inodes
        )
    }
}

/// Read one 1 KiB block (`block` counts from the start of the partition)
/// into `out` (must be ≥ 1024 bytes; extra bytes are untouched).
fn read_part_block(
    dev: &mut AtaDrive,
    part_start: u32,
    block: u32,
    out: &mut [u8],
) -> Result<(), FsError> {
    let lba = part_start + block * 2;
    dev.read_sectors(lba, 2, &mut out[..BLOCK])
        .map_err(FsError::Io)
}

impl Ext2 {
    fn read_block(&self, dev: &mut AtaDrive, block: u32, out: &mut [u8; BLOCK]) -> Result<(), FsError> {
        read_part_block(dev, self.part_start, block, out)
    }

    fn write_block(&self, dev: &mut AtaDrive, block: u32, data: &[u8; BLOCK]) -> Result<(), FsError> {
        let lba = self.part_start + block * 2;
        dev.write_sectors(lba, 2, &data[..])
            .map_err(FsError::Io)
    }

    /// Fetch an inode's fields from the (128-byte, rev-1) inode table.
    fn read_inode(&self, dev: &mut AtaDrive, ino: u32) -> Result<Inode, FsError> {
        if ino == 0 || ino > self.inodes_count {
            return Err(FsError::BadFs);
        }
        let idx = (ino - 1) as usize;
        let table_block = self.inode_table + (idx * self.inode_size as usize / BLOCK) as u32;
        let off = idx * self.inode_size as usize % BLOCK;
        let mut b = [0u8; BLOCK];
        self.read_block(dev, table_block, &mut b)?;
        let p = &b[off..off + self.inode_size as usize];
        let mut blocks = [0u32; 15];
        for (i, slot) in blocks.iter_mut().enumerate() {
            *slot = u32::from_le_bytes(p[40 + i * 4..44 + i * 4].try_into().unwrap());
        }
        Ok(Inode {
            ino,
            mode: u16::from_le_bytes(p[0..2].try_into().unwrap()),
            size: u32::from_le_bytes(p[4..8].try_into().unwrap()),
            sectors: u32::from_le_bytes(p[28..32].try_into().unwrap()),
            blocks,
        })
    }

    /// Persist an inode's mutable fields (mode/size/sectors/i_block).
    fn write_inode(&self, dev: &mut AtaDrive, ino: &Inode) -> Result<(), FsError> {
        let idx = (ino.ino - 1) as usize;
        let table_block = self.inode_table + (idx * self.inode_size as usize / BLOCK) as u32;
        let off = idx * self.inode_size as usize % BLOCK;
        let mut b = [0u8; BLOCK];
        self.read_block(dev, table_block, &mut b)?;
        let p = &mut b[off..off + self.inode_size as usize];
        p[0..2].copy_from_slice(&ino.mode.to_le_bytes());
        p[4..8].copy_from_slice(&ino.size.to_le_bytes());
        p[28..32].copy_from_slice(&ino.sectors.to_le_bytes());
        for (i, v) in ino.blocks.iter().enumerate() {
            p[40 + i * 4..44 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        self.write_block(dev, table_block, &b)
    }

    /// Physical block holding logical block `idx` of `ino`; 0 if the block
    /// is sparse (hole). Direct, single- and double-indirect only.
    fn map_block(&self, dev: &mut AtaDrive, ino: &Inode, idx: u32) -> Result<u32, FsError> {
        let idx = idx as usize;
        if idx < DIRECT {
            return Ok(ino.blocks[idx]);
        }
        let mut remaining = idx - DIRECT;
        if remaining < PTRS {
            return self.indirect_lookup(dev, ino.blocks[12], remaining);
        }
        remaining -= PTRS;
        if remaining < PTRS * PTRS {
            let mid = self.indirect_lookup(dev, ino.blocks[13], remaining / PTRS)?;
            return self.indirect_lookup(dev, mid, remaining % PTRS);
        }
        Err(FsError::NotSupported) // triple indirect
    }

    /// `idx`-th pointer out of the indirect block `tbl` (0 → sparse).
    fn indirect_lookup(&self, dev: &mut AtaDrive, tbl: u32, idx: usize) -> Result<u32, FsError> {
        if tbl == 0 {
            return Ok(0);
        }
        let mut b = [0u8; BLOCK];
        self.read_block(dev, tbl, &mut b)?;
        Ok(u32::from_le_bytes(b[idx * 4..idx * 4 + 4].try_into().unwrap()))
    }
}

impl Ext2 {
    /// Allocate a zeroed data/metadata block from the group's block bitmap
    /// (bit b ↔ block b+1) and sync the free counts. Returns the block number.
    fn alloc_block(&mut self, dev: &mut AtaDrive) -> Result<u32, FsError> {
        if self.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        let mut bits = [0u8; BLOCK];
        self.read_block(dev, self.block_bitmap, &mut bits)?;
        for byte in 0..BLOCK {
            if bits[byte] == 0xFF {
                continue;
            }
            for bit in 0u8..8 {
                if bits[byte] & (1 << bit) == 0 {
                    bits[byte] |= 1 << bit;
                    self.write_block(dev, self.block_bitmap, &bits)?;
                    self.free_blocks -= 1;
                    self.sync_free_counts(dev)?;
                    let block = (byte as u32) * 8 + bit as u32 + 1;
                    // Fresh blocks must read as zeros: indirect tables and
                    // sparse-hole reads both rely on it.
                    let zeroed = [0u8; BLOCK];
                    self.write_block(dev, block, &zeroed)?;
                    return Ok(block);
                }
            }
        }
        Err(FsError::NoSpace)
    }

    /// Allocate a free inode from the inode bitmap (bit b ↔ inode b+1).
    /// Reserved inodes (1..=10) are marked used in the bitmap, so the scan
    /// naturally lands on the first allocatable slot.
    fn alloc_inode(&mut self, dev: &mut AtaDrive) -> Result<u32, FsError> {
        if self.free_inodes == 0 {
            return Err(FsError::NoSpace);
        }
        let mut bits = [0u8; BLOCK];
        self.read_block(dev, self.inode_bitmap, &mut bits)?;
        for byte in 0..BLOCK {
            if bits[byte] == 0xFF {
                continue;
            }
            for bit in 0u8..8 {
                if bits[byte] & (1 << bit) == 0 {
                    bits[byte] |= 1 << bit;
                    self.write_block(dev, self.inode_bitmap, &bits)?;
                    self.free_inodes -= 1;
                    self.sync_free_counts(dev)?;
                    return Ok((byte as u32) * 8 + bit as u32 + 1);
                }
            }
        }
        Err(FsError::NoSpace)
    }

    /// Persist the cached free counts to the superblock and group descriptor.
    fn sync_free_counts(&self, dev: &mut AtaDrive) -> Result<(), FsError> {
        let mut sb = [0u8; BLOCK];
        self.read_block(dev, 1, &mut sb)?;
        sb[12..16].copy_from_slice(&self.free_blocks.to_le_bytes());
        sb[16..20].copy_from_slice(&self.free_inodes.to_le_bytes());
        self.write_block(dev, 1, &sb)?;
        let mut gd = [0u8; BLOCK];
        self.read_block(dev, 2, &mut gd)?;
        // GD32: bg_free_blocks_count (u16) at 8, bg_free_inodes_count at 10.
        gd[8..10].copy_from_slice(&(self.free_blocks as u16).to_le_bytes());
        gd[10..12].copy_from_slice(&(self.free_inodes as u16).to_le_bytes());
        self.write_block(dev, 2, &gd)
    }

    /// Make logical block `idx` of `ino` exist, allocating the data block
    /// plus any missing indirect tables along the way. Returns its number.
    fn ensure_block(&mut self, dev: &mut AtaDrive, ino: &mut Inode, idx: u32) -> Result<u32, FsError> {
        let idx = idx as usize;
        if idx < DIRECT {
            if ino.blocks[idx] != 0 {
                return Ok(ino.blocks[idx]);
            }
            let nb = self.alloc_block(dev)?;
            ino.blocks[idx] = nb;
            ino.sectors += (BLOCK / SECTOR_SIZE) as u32;
            return Ok(nb);
        }
        let mut remaining = idx - DIRECT;
        if remaining < PTRS {
            let tbl = self.ensure_indirect(dev, ino, 12)?;
            return self.ensure_slot(dev, tbl, remaining);
        }
        remaining -= PTRS;
        if remaining < PTRS * PTRS {
            let l1 = self.ensure_indirect(dev, ino, 13)?;
            let mid = self.ensure_slot(dev, l1, remaining / PTRS)?;
            return self.ensure_slot(dev, mid, remaining % PTRS);
        }
        Err(FsError::NotSupported)
    }

    /// Get (or allocate + link) the indirect pointer table in `ino.blocks[slot]`.
    fn ensure_indirect(&mut self, dev: &mut AtaDrive, ino: &mut Inode, slot: usize) -> Result<u32, FsError> {
        if ino.blocks[slot] != 0 {
            return Ok(ino.blocks[slot]);
        }
        let nb = self.alloc_block(dev)?; // zeroed by the allocator
        ino.blocks[slot] = nb;
        ino.sectors += (BLOCK / SECTOR_SIZE) as u32;
        Ok(nb)
    }

    /// Get (or allocate) the data block behind pointer `idx` of table `tbl`.
    fn ensure_slot(&mut self, dev: &mut AtaDrive, tbl: u32, idx: usize) -> Result<u32, FsError> {
        let mut t = [0u8; BLOCK];
        self.read_block(dev, tbl, &mut t)?;
        let existing = u32::from_le_bytes(t[idx * 4..idx * 4 + 4].try_into().unwrap());
        if existing != 0 {
            return Ok(existing);
        }
        let nb = self.alloc_block(dev)?;
        t[idx * 4..idx * 4 + 4].copy_from_slice(&nb.to_le_bytes());
        self.write_block(dev, tbl, &t)?;
        Ok(nb)
    }
}

impl Ext2 {
    /// Walk a directory inode's data blocks and collect every entry slot,
    /// including free slots (inode 0) — the allocator reuses those.
    fn dir_entries(&self, dev: &mut AtaDrive, dir: &Inode) -> Result<Vec<DirEnt>, FsError> {
        let nblocks = dir.size as usize / BLOCK;
        let mut out = Vec::new();
        let mut buf = [0u8; BLOCK];
        for lblock in 0..nblocks as u32 {
            let phys = self.map_block(dev, dir, lblock)?;
            if phys == 0 {
                continue; // sparse hole in the directory: nothing here
            }
            self.read_block(dev, phys, &mut buf)?;
            let mut off = 0usize;
            while off + 8 <= BLOCK {
                let rec = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec < 8 || off + rec > BLOCK {
                    return Err(FsError::BadFs); // corrupt record length
                }
                let ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let nlen = buf[off + 6] as usize;
                let name = if ino != 0 {
                    if 8 + nlen > rec {
                        return Err(FsError::BadFs);
                    }
                    String::from_utf8_lossy(&buf[off + 8..off + 8 + nlen]).into_owned()
                } else {
                    String::new()
                };
                out.push(DirEnt {
                    ino,
                    name,
                    rec_len: rec as u16,
                    offset: lblock * BLOCK as u32 + off as u32,
                });
                off += rec;
            }
        }
        Ok(out)
    }

    /// Look up `name` in the directory `dir`.
    fn lookup_in(
        &self,
        dev: &mut AtaDrive,
        dir: &Inode,
        name: &str,
    ) -> Result<Option<Inode>, FsError> {
        for e in self.dir_entries(dev, dir)? {
            if e.ino != 0 && e.name == name {
                return self.read_inode(dev, e.ino).map(Some);
            }
        }
        Ok(None)
    }

    /// Directory listing with sizes + directory flags. Free slots (inode 0)
    /// and the `.`/`..` bookkeeping entries are hidden.
    fn list_dir(&self, dev: &mut AtaDrive, dir: &Inode) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        let mut out = Vec::new();
        for e in self.dir_entries(dev, dir)? {
            if e.ino == 0 || e.name == "." || e.name == ".." {
                continue;
            }
            let ino = self.read_inode(dev, e.ino)?;
            out.push(crate::vfs::DirEntry {
                name: e.name,
                size: ino.size as u64,
                is_dir: ino.mode & 0xF000 == MODE_DIR,
            });
        }
        Ok(out)
    }

    /// Listing of the directory at `path` for the VFS (`/` = root).
    fn list_path(
        &self,
        dev: &mut AtaDrive,
        path: &str,
    ) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        let dir = match self.resolve(dev, path)? {
            Some(i) if i.mode & 0xF000 == MODE_DIR => i,
            Some(_) => return Err(FsError::NotSupported), // a file
            None => return Err(FsError::NotFound),
        };
        self.list_dir(dev, &dir)
    }

    /// Validate that `path` exists (used by SYS_OPEN).
    fn open_path(&self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        match self.resolve(dev, path)? {
            Some(_) => Ok(()),
            None => Err(FsError::NotFound),
        }
    }

    /// Resolve `path` to its inode (`/` = root). Intermediate components must
    /// be directories; a missing final component yields `Ok(None)`.
    fn resolve(&self, dev: &mut AtaDrive, path: &str) -> Result<Option<Inode>, FsError> {
        let parts = crate::vfs::split_path(path).ok_or(FsError::NotSupported)?;
        let mut cur = self.read_inode(dev, ROOT_INO)?;
        for (i, comp) in parts.iter().enumerate() {
            let Some(next) = self.lookup_in(dev, &cur, comp)? else {
                return Ok(None);
            };
            if i + 1 != parts.len() && next.mode & 0xF000 != MODE_DIR {
                return Err(FsError::BadFs); // a file in the middle of the path
            }
            cur = next;
        }
        Ok(Some(cur))
    }

    /// Walk to the parent directory of `path`; returns (parent inode, final
    /// component). The root has no parent (callers reject `/` up front).
    fn resolve_parent(&self, dev: &mut AtaDrive, path: &str) -> Result<(Inode, String), FsError> {
        let parts = crate::vfs::split_path(path).ok_or(FsError::NotSupported)?;
        let Some((last, parents)) = parts.split_last() else {
            return Err(FsError::NotSupported); // "/" has nothing to create in
        };
        let mut cur = self.read_inode(dev, ROOT_INO)?;
        for comp in parents {
            let next = self.lookup_in(dev, &cur, comp)?.ok_or(FsError::NotFound)?;
            if next.mode & 0xF000 != MODE_DIR {
                return Err(FsError::BadFs);
            }
            cur = next;
        }
        Ok((cur, (*last).to_string()))
    }

    /// Read a byte range out of an already-fetched inode, honoring the file
    /// size. Sparse blocks read as zeros; the range is clamped to the EOF.
    fn read_inode_at(
        &self,
        dev: &mut AtaDrive,
        ino: &Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if off >= ino.size as u64 || buf.is_empty() {
            return Ok(0);
        }
        let start = off as usize;
        let end = (start + buf.len()).min(ino.size as usize);
        let mut bbuf = [0u8; BLOCK];
        let mut done = 0usize;
        while done < end - start {
            let pos = start + done;
            let (lblock, inoff) = ((pos / BLOCK) as u32, pos % BLOCK);
            let take = (BLOCK - inoff).min(end - pos);
            let phys = self.map_block(dev, ino, lblock)?;
            if phys == 0 {
                buf[done..done + take].fill(0);
            } else {
                self.read_block(dev, phys, &mut bbuf)?;
                buf[done..done + take].copy_from_slice(&bbuf[inoff..inoff + take]);
            }
            done += take;
        }
        Ok(done)
    }

    /// Read up to `buf.len()` bytes of `path` at file offset `off`.
    fn read_at_path(
        &self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let ino = match self.resolve(dev, path)? {
            Some(i) if i.mode & 0xF000 != MODE_DIR => i,
            Some(_) => return Err(FsError::NotSupported), // a directory
            None => return Err(FsError::NotFound),
        };
        self.read_inode_at(dev, &ino, off, buf)
    }
}

/// Write one directory record into a block buffer.
fn write_entry(buf: &mut [u8; BLOCK], off: usize, ino: u32, name: &str, rec_len: u16) {
    buf[off..off + 4].copy_from_slice(&ino.to_le_bytes());
    buf[off + 4..off + 6].copy_from_slice(&rec_len.to_le_bytes());
    buf[off + 6] = name.len() as u8;
    buf[off + 7] = 0; // no filetype feature
    buf[off + 8..off + 8 + name.len()].copy_from_slice(name.as_bytes());
}

/// Patch only the `rec_len` field of the record at `off`.
fn set_rec_len(buf: &mut [u8; BLOCK], off: usize, rec_len: u16) {
    buf[off + 4..off + 6].copy_from_slice(&rec_len.to_le_bytes());
}

impl Ext2 {
    /// Create a regular file with `name` in `parent` and return its
    /// zero-length inode.
    fn create_in(
        &mut self,
        dev: &mut AtaDrive,
        parent: &Inode,
        name: &str,
    ) -> Result<Inode, FsError> {
        if name.len() > 255 {
            return Err(FsError::NotSupported);
        }
        let num = self.alloc_inode(dev)?;
        let ino = Inode {
            ino: num,
            mode: MODE_REG,
            size: 0,
            sectors: 0,
            blocks: [0u32; 15],
        };
        self.write_inode(dev, &ino)?;
        let mut dir = *parent;
        self.add_dir_entry(dev, &mut dir, name, num)?;
        Ok(ino)
    }

    /// Create a directory with `name` in `parent` and return its inode.
    /// Initializes the mandatory `.` (self) and `..` (parent) records in a
    /// freshly allocated directory block. (Link counts stay untouched — this
    /// driver never consults them.)
    fn mkdir_in(
        &mut self,
        dev: &mut AtaDrive,
        parent: &Inode,
        name: &str,
    ) -> Result<Inode, FsError> {
        if name.len() > 255 {
            return Err(FsError::NotSupported);
        }
        let num = self.alloc_inode(dev)?;
        let mut ino = Inode {
            ino: num,
            mode: MODE_DIR,
            size: 0,
            sectors: 0,
            blocks: [0u32; 15],
        };
        let phys = self.ensure_block(dev, &mut ino, 0)?;
        ino.size = BLOCK as u32;
        let mut blk = [0u8; BLOCK];
        write_entry(&mut blk, 0, num, ".", 12);
        write_entry(&mut blk, 12, parent.ino, "..", (BLOCK - 12) as u16);
        self.write_block(dev, phys, &blk)?;
        self.write_inode(dev, &ino)?;
        let mut dir = *parent;
        self.add_dir_entry(dev, &mut dir, name, num)?;
        Ok(ino)
    }

    /// Create the directory at `path` (parent must exist, name must be free).
    fn mkdir(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        if self.resolve(dev, path)?.is_some() {
            return Err(FsError::Exists);
        }
        let (parent, name) = self.resolve_parent(dev, path)?;
        if parent.mode & 0xF000 != MODE_DIR {
            return Err(FsError::BadFs);
        }
        self.mkdir_in(dev, &parent, &name)?;
        Ok(())
    }

    /// Insert `name -> target` into the directory `dir`. Reuses the slack of
    /// an existing record or a free slot when one fits; otherwise appends a
    /// fresh directory block (growing `dir`, which is written back).
    fn add_dir_entry(
        &mut self,
        dev: &mut AtaDrive,
        dir: &mut Inode,
        name: &str,
        target: u32,
    ) -> Result<(), FsError> {
        let need = ((8 + name.len() + 3) / 4 * 4) as u16;
        let entries = self.dir_entries(dev, dir)?;
        // Bytes an entry's own name needs inside its record.
        let span = |e: &DirEnt| ((8 + e.name.len() + 3) / 4 * 4) as u16;

        let mut buf = [0u8; BLOCK];
        for e in &entries {
            let avail = if e.ino == 0 { e.rec_len } else { e.rec_len - span(e) };
            if avail < need {
                continue;
            }
            let phys = match self.map_block(dev, dir, e.offset / BLOCK as u32)? {
                0 => return Err(FsError::BadFs), // entry inside a hole: corrupt
                p => p,
            };
            self.read_block(dev, phys, &mut buf)?;
            let off = (e.offset % BLOCK as u32) as usize;
            if e.ino == 0 {
                // Split the free slot: new entry in front, the remainder
                // (if record-sized) stays free.
                let rest = e.rec_len as usize - need as usize;
                if rest >= 8 {
                    write_entry(&mut buf, off, target, name, need);
                    write_entry(&mut buf, off + need as usize, 0, "", rest as u16);
                } else {
                    write_entry(&mut buf, off, target, name, e.rec_len);
                }
            } else {
                // Shrink the live record to its own span and put the new
                // entry in the freed slack (it absorbs all of it so the
                // record chain still covers the block exactly).
                let own = span(e);
                set_rec_len(&mut buf, off, own);
                write_entry(&mut buf, off + own as usize, target, name, e.rec_len - own);
            }
            self.write_block(dev, phys, &buf)?;
            return Ok(());
        }

        // No room anywhere: extend the directory by one block.
        let lblock = dir.size / BLOCK as u32;
        let phys = self.ensure_block(dev, dir, lblock)?;
        dir.size += BLOCK as u32;
        self.write_inode(dev, dir)?;
        let mut fresh = [0u8; BLOCK];
        write_entry(&mut fresh, 0, target, name, BLOCK as u16);
        self.write_block(dev, phys, &fresh)
    }

    /// Write `buf` at `off` in `path` (create-if-missing), growing the file
    /// and its block tree as needed. Read-modify-write on partial blocks.
    fn write_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        if buf.is_empty() {
            // Create-if-missing contract (mirrors fat.rs): a zero-length
            // write materializes the file when it doesn't exist yet (the
            // parent directory must already exist).
            if self.resolve(dev, path)?.is_none() {
                let (parent, name) = self.resolve_parent(dev, path)?;
                self.create_in(dev, &parent, &name)?;
            }
            return Ok(0);
        }
        let mut ino = match self.resolve(dev, path)? {
            Some(i) if i.mode & 0xF000 != MODE_DIR => i,
            Some(_) => return Err(FsError::NotSupported), // a directory
            None => {
                let (parent, name) = self.resolve_parent(dev, path)?;
                self.create_in(dev, &parent, &name)?
            }
        };
        let mut bbuf = [0u8; BLOCK];
        let mut written = 0usize;
        while written < buf.len() {
            let pos = off as usize + written;
            let (lblock, inoff) = ((pos / BLOCK) as u32, pos % BLOCK);
            let take = (BLOCK - inoff).min(buf.len() - written);
            let phys = self.ensure_block(dev, &mut ino, lblock)?;
            if take < BLOCK {
                self.read_block(dev, phys, &mut bbuf)?; // partial: RMW
            } else {
                bbuf = [0u8; BLOCK];
            }
            bbuf[inoff..inoff + take].copy_from_slice(&buf[written..written + take]);
            self.write_block(dev, phys, &bbuf)?;
            written += take;
        }
        let new_end = off as usize + buf.len();
        if new_end as u64 > ino.size as u64 {
            ino.size = new_end as u32;
        }
        self.write_inode(dev, &ino)?;
        Ok(buf.len())
    }
}

// ---------------------------------------------------------------------------
// C3: metadata, unlink, rename.
// ---------------------------------------------------------------------------

impl Ext2 {
    /// Metadata about `path`.
    pub fn stat(&self, dev: &mut AtaDrive, path: &str) -> Result<crate::vfs::FileStat, FsError> {
        match self.resolve(dev, path)? {
            Some(i) => Ok(crate::vfs::FileStat {
                size: i.size as u64,
                is_dir: i.mode & 0xF000 == MODE_DIR,
            }),
            None => Err(FsError::NotFound),
        }
    }

    /// Collect every physical block owned by `ino` (data + indirect tables)
    /// into `out` so unlink can return them to the block bitmap.
    fn collect_blocks(&self, dev: &mut AtaDrive, ino: &Inode, out: &mut Vec<u32>) -> Result<(), FsError> {
        for &b in &ino.blocks[..DIRECT] {
            if b != 0 {
                out.push(b);
            }
        }
        // Single indirect: the table plus every data block it references.
        if ino.blocks[12] != 0 {
            let mut t = [0u8; BLOCK];
            self.read_block(dev, ino.blocks[12], &mut t)?;
            for k in 0..PTRS {
                let v = u32::from_le_bytes(t[k * 4..k * 4 + 4].try_into().unwrap());
                if v != 0 {
                    out.push(v);
                }
            }
            out.push(ino.blocks[12]);
        }
        // Double indirect: the L1 table, each L2 table, and their data blocks.
        if ino.blocks[13] != 0 {
            let mut l1 = [0u8; BLOCK];
            self.read_block(dev, ino.blocks[13], &mut l1)?;
            for k in 0..PTRS {
                let v = u32::from_le_bytes(l1[k * 4..k * 4 + 4].try_into().unwrap());
                if v == 0 {
                    continue;
                }
                let mut l2 = [0u8; BLOCK];
                self.read_block(dev, v, &mut l2)?;
                for j in 0..PTRS {
                    let w = u32::from_le_bytes(l2[j * 4..j * 4 + 4].try_into().unwrap());
                    if w != 0 {
                        out.push(w);
                    }
                }
                out.push(v);
            }
            out.push(ino.blocks[13]);
        }
        Ok(())
    }

    /// Free `ino`'s data + indirect blocks back to the group bitmap.
    fn free_inode_blocks(&mut self, dev: &mut AtaDrive, ino: &Inode) -> Result<(), FsError> {
        let mut list = Vec::new();
        self.collect_blocks(dev, ino, &mut list)?;
        if list.is_empty() {
            return Ok(());
        }
        let mut bits = [0u8; BLOCK];
        self.read_block(dev, self.block_bitmap, &mut bits)?;
        for &b in &list {
            if b == 0 {
                continue;
            }
            let idx = (b - 1) as usize;
            let (byte, bit) = (idx / 8, idx % 8);
            if bits[byte] & (1 << bit) != 0 {
                bits[byte] &= !(1 << bit);
                self.free_blocks += 1;
            }
        }
        self.write_block(dev, self.block_bitmap, &bits)?;
        Ok(())
    }

    /// Free an inode back to the inode bitmap (+ sync free counts).
    fn free_inode(&mut self, dev: &mut AtaDrive, ino: u32) -> Result<(), FsError> {
        let mut bits = [0u8; BLOCK];
        self.read_block(dev, self.inode_bitmap, &mut bits)?;
        let idx = (ino - 1) as usize;
        let (byte, bit) = (idx / 8, idx % 8);
        if bits[byte] & (1 << bit) != 0 {
            bits[byte] &= !(1 << bit);
            self.free_inodes += 1;
        }
        self.write_block(dev, self.inode_bitmap, &bits)?;
        self.sync_free_counts(dev)
    }

    /// Remove the file at `path`: drop its directory record (inode field =
    /// 0, rec_len preserved so the slot stays a reusable free record) and
    /// return its data + inode to the free bitmaps. Directories → `IsDir`.
    pub fn unlink(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        let ino = match self.resolve(dev, path)? {
            Some(i) => i,
            None => return Err(FsError::NotFound),
        };
        if ino.mode & 0xF000 == MODE_DIR {
            return Err(FsError::IsDir);
        }
        let (parent, _) = self.resolve_parent(dev, path)?;
        let entries = self.dir_entries(dev, &parent)?;
        let Some(e) = entries.iter().find(|e| e.ino == ino.ino) else {
            return Err(FsError::BadFs);
        };
        let mut buf = [0u8; BLOCK];
        let phys = match self.map_block(dev, &parent, e.offset / BLOCK as u32)? {
            0 => return Err(FsError::BadFs),
            p => p,
        };
        self.read_block(dev, phys, &mut buf)?;
        let off = (e.offset % BLOCK as u32) as usize;
        buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        buf[off + 6] = 0; // name_len
        self.write_block(dev, phys, &buf)?;
        self.free_inode_blocks(dev, &ino)?;
        self.free_inode(dev, ino.ino)?;
        Ok(())
    }

    /// Same-directory rename: rewrite the source record's name in place.
    /// The new name must fit inside the record's rec_len (slack is absorbed,
    /// which ext2 permits). Cross-directory moves are rejected.
    pub fn rename(&mut self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        let ino = match self.resolve(dev, from)? {
            Some(i) => i,
            None => return Err(FsError::NotFound),
        };
        if self.resolve(dev, to)?.is_some() {
            return Err(FsError::Exists);
        }
        let (from_parent, _) = self.resolve_parent(dev, from)?;
        let (to_parent, to_name) = self.resolve_parent(dev, to)?;
        if to_parent.ino != from_parent.ino {
            return Err(FsError::NotSupported);
        }
        let need = ((8 + to_name.len() + 3) / 4 * 4) as u16;
        let entries = self.dir_entries(dev, &from_parent)?;
        let Some(e) = entries.iter().find(|e| e.ino == ino.ino) else {
            return Err(FsError::BadFs);
        };
        if need > e.rec_len {
            return Err(FsError::NotSupported); // name longer than the slot absorbs
        }
        let mut buf = [0u8; BLOCK];
        let phys = match self.map_block(dev, &from_parent, e.offset / BLOCK as u32)? {
            0 => return Err(FsError::BadFs),
            p => p,
        };
        self.read_block(dev, phys, &mut buf)?;
        let off = (e.offset % BLOCK as u32) as usize;
        buf[off + 6] = to_name.len() as u8;
        buf[off + 8..off + 8 + to_name.len()].copy_from_slice(to_name.as_bytes());
        self.write_block(dev, phys, &buf)?;
        Ok(())
    }
}

/// Bridge to the VFS trait (inherent methods take resolution precedence, so
/// these thin wrappers call the identically-named inherent ones).
impl crate::vfs::FileSystem for Ext2 {
    fn list(&mut self, dev: &mut AtaDrive, path: &str) -> Result<Vec<crate::vfs::DirEntry>, FsError> {
        Ext2::list_path(self, dev, path)
    }

    fn open(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Ext2::open_path(self, dev, path)
    }

    fn read_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        Ext2::read_at_path(self, dev, path, off, buf)
    }

    fn write_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        Ext2::write_at(self, dev, path, off, buf)
    }

    fn mkdir(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Ext2::mkdir(self, dev, path)
    }

    fn stat(&mut self, dev: &mut AtaDrive, path: &str) -> Result<crate::vfs::FileStat, FsError> {
        Ext2::stat(self, dev, path)
    }

    fn unlink(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        Ext2::unlink(self, dev, path)
    }

    fn rename(&mut self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        Ext2::rename(self, dev, from, to)
    }
}

/// Find the ext2 partition (appended by build.rs as MBR type 0x83), mount it,
/// run the kernel-side self-test and install it as the VFS secondary mount.
pub fn init() {
    let mounted = crate::block::with_disk(|disk| {
        let Some(part) = disk
            .partitions
            .iter()
            .find(|p| p.kind.contains("Linux") && p.sectors * 512 >= 1024 * 1024)
        else {
            crate::serial_writeln!("M7: no Linux (0x83) partition found on boot disk");
            return None;
        };
        match Ext2::mount(&mut disk.drive, part.start_lba as u32) {
            Ok(mut fs) => {
                crate::serial_writeln!(
                    "M7: ext2 mounted (LBA {}): {}",
                    part.start_lba,
                    fs.summary()
                );
                self_test(&mut fs, &mut disk.drive);
                Some(Box::new(fs) as Box<dyn crate::vfs::FileSystem>)
            }
            Err(e) => {
                crate::serial_writeln!("M7: ext2 mount failed: {e:?}");
                None
            }
        }
    })
    .flatten();
    match mounted {
        Some(fs) => {
            crate::vfs::mount_secondary(fs);
            crate::serial_writeln!("M7 ready: ext2 live as the VFS secondary mount");
        }
        None => crate::serial_writeln!("M7: ext2 NOT mounted (secondary VFS slot empty)"),
    }
}

/// Kernel-side exercise of the whole driver: seed-file reads, a 13 KiB
/// write/readback that crosses from direct into single-indirect blocks, an
/// overwrite in the middle of the file, and (M8) directory creation with
/// nested write/read. Every line is prefixed `M7:`/`M8:`.
fn self_test(fs: &mut Ext2, dev: &mut AtaDrive) {
    match fs.list_path(dev, "/") {
        Ok(entries) => {
            crate::serial_writeln!("M7: ext2 root: {} entries", entries.len());
            for e in &entries {
                crate::serial_writeln!(
                    "M7:   {} ({} bytes{})",
                    e.name,
                    e.size,
                    if e.is_dir { ", dir" } else { "" }
                );
            }
        }
        Err(e) => {
            crate::serial_writeln!("M7: ext2 root listing failed: {e:?}");
            return;
        }
    }

    // Seed files written by mkext2 must round-trip byte-exactly.
    let seeds: [(&str, &[u8]); 2] = [
        ("/HELLO.TXT", b"hello from ext2!\n"),
        ("/README.TXT", b"OnyxOS M7 ext2 test volume\n"),
    ];
    let mut buf = [0u8; 64];
    for (name, expect) in seeds {
        match fs.read_at_path(dev, name, 0, &mut buf) {
            Ok(n) if &buf[..n] == expect => {
                crate::serial_writeln!("M7: read {name}: OK ({n} bytes)")
            }
            Ok(n) => crate::serial_writeln!(
                "M7: read {name}: MISMATCH ({n} bytes, expected {})",
                expect.len()
            ),
            Err(e) => crate::serial_writeln!("M7: read {name}: {e:?}"),
        }
    }

    // 13 KiB pattern: logical blocks 0..=11 are direct, block 12 comes out of
    // the single-indirect table — exercises ensure_block + map_block growth.
    const BIG: usize = 13 * 1024;
    let mut data = alloc::vec![0u8; BIG];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i * 7 + i / 251) as u8;
    }
    match fs.write_at(dev, "/BIG.BIN", 0, &data) {
        Ok(n) => crate::serial_writeln!(
            "M7: wrote /BIG.BIN ({n} bytes, spans direct + indirect blocks)"
        ),
        Err(e) => {
            crate::serial_writeln!("M7: /BIG.BIN write failed: {e:?}");
            return;
        }
    }
    let mut back = alloc::vec![0u8; BIG];
    let ok = fs
        .read_at_path(dev, "/BIG.BIN", 0, &mut back)
        .map(|n| n == BIG && back[..] == data[..])
        .unwrap_or(false);
    crate::serial_writeln!(
        "M7: indirect read-back: {}",
        if ok { "PASSED" } else { "FAILED" }
    );

    // Overwrite six bytes mid-file (RMW path through a full block), then
    // verify the patched bytes AND that the surrounding pattern survived.
    const MID: usize = 6 * 1024 + 100;
    let patch = b"MIDDLE";
    let _ = fs.write_at(dev, "/BIG.BIN", MID as u64, patch);
    let mut probe = [0u8; 6 + 10];
    let ok2 = fs
        .read_at_path(dev, "/BIG.BIN", MID as u64, &mut probe)
        .map(|n| n == probe.len() && probe[..6] == *patch && probe[6..] == data[MID + 6..MID + 16])
        .unwrap_or(false);
    crate::serial_writeln!(
        "M7: mid-file overwrite: {}",
        if ok2 { "PASSED" } else { "FAILED" }
    );

    // --- M8: subdirectories — mkdir, nested write/read ---
    match fs.mkdir(dev, "/SUB") {
        Ok(()) => crate::serial_writeln!("M8: mkdir /SUB: OK"),
        Err(FsError::Exists) => crate::serial_writeln!("M8: mkdir /SUB: already exists"),
        Err(e) => {
            crate::serial_writeln!("M8: mkdir /SUB failed: {e:?}");
            return;
        }
    }
    let note: &[u8] = b"hello from an ext2 subdirectory\n";
    match fs.write_at(dev, "/SUB/NOTE.TXT", 0, note) {
        Ok(n) if n == note.len() => {
            crate::serial_writeln!("M8: wrote /SUB/NOTE.TXT ({} bytes)", n)
        }
        Ok(n) => crate::serial_writeln!("M8: /SUB/NOTE.TXT short write ({n} bytes)"),
        Err(e) => {
            crate::serial_writeln!("M8: /SUB/NOTE.TXT write failed: {e:?}");
            return;
        }
    }
    let mut nbuf = [0u8; 64];
    match fs.read_at_path(dev, "/SUB/NOTE.TXT", 0, &mut nbuf) {
        Ok(n) if &nbuf[..n] == note => crate::serial_writeln!("M8: subdir read-back: PASSED"),
        Ok(n) => crate::serial_writeln!("M8: subdir read-back: MISMATCH ({n} bytes)"),
        Err(e) => crate::serial_writeln!("M8: subdir read failed: {e:?}"),
    }

    // Two levels deep: /SUB/DEEP/D.TXT exercises multi-component resolution.
    let _ = fs.mkdir(dev, "/SUB/DEEP");
    let deep: &[u8] = b"two levels down\n";
    let _ = fs.write_at(dev, "/SUB/DEEP/D.TXT", 0, deep);
    let mut dbuf = [0u8; 32];
    let ok3 = fs
        .read_at_path(dev, "/SUB/DEEP/D.TXT", 0, &mut dbuf)
        .map(|n| n == deep.len() && dbuf[..n] == *deep)
        .unwrap_or(false);
    crate::serial_writeln!(
        "M8: ext2 nested path write/read: {}",
        if ok3 { "PASSED" } else { "FAILED" }
    );
}
