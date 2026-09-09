//! `mkext2` — build a fixed-size ext2 filesystem image for OnyxOS M7+.
//!
//! Produces the minimal, spec-valid ext2 volume the kernel's ext2 driver
//! expects: 1 KiB blocks, revision 1, 128-byte inodes, a single block group,
//! no compatibility features (so directory entries carry no filetype byte),
//! and two seed files so the driver has real contents to list/read at boot.
//!
//! Layout (8 MiB default = 8192 blocks):
//!   block 0          boot block (unused, outside the bitmap domain)
//!   block 1          superblock
//!   block 2          group descriptor table (1 × 32-byte descriptor)
//!   block 3          block usage bitmap (bit b ↔ block b+1)
//!   block 4          inode usage bitmap (bit b ↔ inode b+1)
//!   blocks 5..132    inode table (1024 inodes × 128 bytes)
//!   blocks 133..     data (root directory, then seed files)
//!
//! Usage: mkext2 <out.img> <mb-size>

use std::fs::File;
use std::io::Write;
use std::process::ExitCode;

const BLOCK: usize = 1024;
const INODE_SIZE: usize = 128;
const INODES: u32 = 1024;
/// First non-reserved inode number (matches the superblock's s_first_ino).
const FIRST_INO: u32 = 11;
const MODE_DIR: u16 = 0x4000;
const MODE_REG: u16 = 0x8000;
const SUPER_MAGIC: u16 = 0xEF53;

const SEEDS: [(&str, &[u8]); 2] = [
    ("HELLO.TXT", b"hello from ext2!\n"),
    ("README.TXT", b"OnyxOS M7 ext2 test volume\n"),
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(out), Some(size_str)) = (args.next(), args.next()) else {
        eprintln!("usage: mkext2 <out.img> <mb>");
        return ExitCode::FAILURE;
    };
    let Ok(mb) = size_str.parse::<u64>() else {
        eprintln!("bad size: {size_str}");
        return ExitCode::FAILURE;
    };
    // Single block group: blocks_per_group = 8192 × 1 KiB = 8 MiB max.
    let mb = mb.clamp(1, 8);
    let img = build(mb);
    match File::create(&out).and_then(|mut f| f.write_all(&img)) {
        Ok(()) => {
            eprintln!(
                "mkext2: wrote {out} ({} bytes, {} blocks)",
                img.len(),
                img.len() / BLOCK
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mkext2: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn build(mb: u64) -> Vec<u8> {
    let blocks = (mb as usize) * 1024; // 1024 × 1 KiB blocks per MiB
    let mut img = vec![0u8; blocks * BLOCK];

    // --- fixed layout (see module docs) ---
    let gdt_block = 2usize;
    let block_bitmap_block = 3usize;
    let inode_bitmap_block = 4usize;
    let inode_table_block = 5usize;
    let inode_table_blocks = (INODES as usize * INODE_SIZE) / BLOCK; // 128
    let first_data = inode_table_block + inode_table_blocks; // 133

    // Data blocks handed out: root directory first, then one per seed file.
    let root_block = first_data;
    let data_used = 1 + SEEDS.len();

    // The block bitmap covers blocks 1..blocks (block 0 is the boot block).
    let bitmap_blocks = blocks - 1;
    let used_in_bitmap = first_data - 1 + data_used;
    let free_blocks = (bitmap_blocks - used_in_bitmap) as u32;
    // Inodes 1..=10 are reserved (root is inode 2, inside that range); the
    // seed files take the FIRST_INO.. slots.
    let used_inodes = 10u32 + SEEDS.len() as u32;
    let free_inodes = INODES - used_inodes;

    // ---- superblock (byte offset 1024 = block 1) ----
    let sb = BLOCK;
    put32(&mut img, sb + 0, INODES); // s_inodes_count
    put32(&mut img, sb + 4, blocks as u32); // s_blocks_count
    put32(&mut img, sb + 8, 0); // s_r_blocks_count
    put32(&mut img, sb + 12, free_blocks); // s_free_blocks_count
    put32(&mut img, sb + 16, free_inodes); // s_free_inodes_count
    put32(&mut img, sb + 20, 1); // s_first_data_block (1 for 1 KiB blocks)
    put32(&mut img, sb + 24, 0); // s_log_block_size: 1024 << 0
    put32(&mut img, sb + 28, 0); // s_log_frag_size: fragments = blocks
    put32(&mut img, sb + 32, 8192); // s_blocks_per_group
    put32(&mut img, sb + 36, 8192); // s_frags_per_group
    put32(&mut img, sb + 40, INODES); // s_inodes_per_group
    put16(&mut img, sb + 56, SUPER_MAGIC); // s_magic
    put16(&mut img, sb + 58, 1); // s_state: clean
    put32(&mut img, sb + 72, 0); // s_creator_os: Linux
    put32(&mut img, sb + 76, 1); // s_rev_level (dynamic: 128-byte inodes)
    put32(&mut img, sb + 84, FIRST_INO); // s_first_ino
    put16(&mut img, sb + 88, 128); // s_inode_size

    // ---- group descriptor 0 (block 2) — real ext2 GD32 layout ----
    let gd = gdt_block * BLOCK;
    put32(&mut img, gd + 0, block_bitmap_block as u32); // bg_block_bitmap
    put32(&mut img, gd + 4, inode_bitmap_block as u32); // bg_inode_bitmap
    put32(&mut img, gd + 8, inode_table_block as u32); // bg_inode_table
    put16(&mut img, gd + 12, free_blocks as u16); // bg_free_blocks_count
    put16(&mut img, gd + 14, free_inodes as u16); // bg_free_inodes_count
    put16(&mut img, gd + 16, 1); // bg_used_dirs_count (the root dir)

    // ---- block usage bitmap (block 3): bit b ↔ block b+1 ----
    let bbm = block_bitmap_block * BLOCK;
    for bit in 0..used_in_bitmap {
        img[bbm + bit / 8] |= 1 << (bit % 8);
    }

    // ---- inode usage bitmap (block 4): bit b ↔ inode b+1 ----
    let ibm = inode_bitmap_block * BLOCK;
    for bit in 0..used_inodes as usize {
        img[ibm + bit / 8] |= 1 << (bit % 8);
    }

    // ---- inode table (blocks 5..132), 128 bytes per inode ----
    let itab = inode_table_block * BLOCK;
    let inode_off = |ino: u32| itab + (ino as usize - 1) * INODE_SIZE;

    // inode 2: the root directory, one 1 KiB block of entries.
    let root = inode_off(2);
    put16(&mut img, root + 0, MODE_DIR); // i_mode
    put32(&mut img, root + 4, BLOCK as u32); // i_size
    put32(&mut img, root + 28, 2); // i_blocks: 1 KiB = 2 × 512-byte sectors
    put32(&mut img, root + 40, root_block as u32); // i_block[0]

    // Seed file inodes: one data block each.
    for (i, (_, content)) in SEEDS.iter().enumerate() {
        let off = inode_off(FIRST_INO + i as u32);
        put16(&mut img, off + 0, MODE_REG); // i_mode
        put32(&mut img, off + 4, content.len() as u32); // i_size
        put32(&mut img, off + 28, 2); // i_blocks
        put32(&mut img, off + 40, (root_block + 1 + i) as u32); // i_block[0]
    }

    // ---- root directory block: ".", "..", the seeds, then one free entry ----
    let dir = root_block * BLOCK;
    let rec_len = |name: &str| ((8 + name.len() + 3) / 4 * 4) as u16;
    let mut pos = dir;
    {
        let mut entry = |img: &mut Vec<u8>, pos: usize, ino: u32, name: &str| {
            put32(img, pos, ino);
            put16(img, pos + 4, rec_len(name));
            img[pos + 6] = name.len() as u8; // name_len
            img[pos + 7] = 0; // file_type: feature disabled
            img[pos + 8..pos + 8 + name.len()].copy_from_slice(name.as_bytes());
        };
        entry(&mut img, pos, 2, ".");
        pos += rec_len(".") as usize;
        entry(&mut img, pos, 2, "..");
        pos += rec_len("..") as usize;
        for (i, (name, _)) in SEEDS.iter().enumerate() {
            entry(&mut img, pos, FIRST_INO + i as u32, name);
            pos += rec_len(name) as usize;
        }
    }
    // Everything left: a single free entry (inode 0, name_len 0) spanning to
    // the end of the block — exactly what the kernel's allocator expects to
    // find when it inserts new entries.
    put32(&mut img, pos, 0);
    put16(&mut img, pos + 4, (dir + BLOCK - pos) as u16);

    // ---- seed file contents (one 1 KiB block each) ----
    for (i, (_, content)) in SEEDS.iter().enumerate() {
        let off = (root_block + 1 + i) * BLOCK;
        img[off..off + content.len()].copy_from_slice(content);
    }

    img
}

fn put16(img: &mut [u8], off: usize, v: u16) {
    img[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(img: &mut [u8], off: usize, v: u32) {
    img[off..off + 4].copy_from_slice(&v.to_le_bytes());
}