//! `mkfat` — build a fixed-size FAT32 filesystem image for OnyxOS M6+.
//!
//! Writes a proper FAT32 (>= 65525 clusters, so `fatfs` picks FAT32) and adds
//! seed files: two built-in text files plus any files named on the command
//! line (user ELFs, the shell's AUTOEXEC script, ...), so the kernel's FAT32
//! driver, the disk-ELF loader and the ring-3 shell have real contents.
//!
//! Usage: mkfat <out.img> <mb-size> [seed-file ...]
//!
//! Each seed file is stored at the root under its uppercase 8.3 name
//! (e.g. `shell.elf` -> `SHELL.ELF`).
#![allow(clippy::missing_errors_doc)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(out), Some(size_str)) = (args.first().cloned(), args.get(1).cloned()) else {
        eprintln!("usage: mkfat <out.img> <mb> [seed-file ...]");
        return ExitCode::FAILURE;
    };
    // Extra seed files: everything after <out> and <mb>.
    let seeds: Vec<String> = args.split_off(2);
    let Ok(mb) = size_str.parse::<u64>() else {
        eprintln!("bad size: {size_str}");
        return ExitCode::FAILURE;
    };
    // At least 16 MiB so the volume is unambiguously FAT32 (>= 65525 clusters
    // requires ~8 MiB min; 16 MiB is safe and still small).
    let mb = mb.max(16);
    let bytes = mb * 1024 * 1024;
    match create(out.clone(), bytes, &seeds) {
        Ok(()) => eprintln!("mkfat: wrote {out} ({bytes} bytes)"),
        Err(e) => {
            eprintln!("mkfat: error: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn create(out: String, bytes: u64, seeds: &[String]) -> std::io::Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&out)?;
    f.set_len(bytes)?;

    // Force FAT32 by requesting a type that only has a FAT32 form:
    // fatfs picks FAT32 when sectors-per-cluster * cluster count is large.
    // Force FAT32 explicitly: fatfs's size-based estimate picks FAT16 for a
    // 32-40 MiB volume (2-4 KiB clusters → < 65525 clusters). With Fat32
    // forced, it uses 512-byte clusters, and build.rs sizes the volume at
    // 40 MiB so the cluster count clears FAT32's 65525 minimum.
    let format_options = fatfs::FormatVolumeOptions::new()
        .bytes_per_sector(512)
        .fat_type(fatfs::FatType::Fat32)
        .total_sectors((bytes / 512) as u32);
    fatfs::format_volume(&f, format_options)?;

    let fs = fatfs::FileSystem::new(&f, fatfs::FsOptions::new())?;
    let root = fs.root_dir();
    write_seed(&root, "README.TXT", b"OnyxOS M6 FAT32 test volume\n")?;
    write_seed(&root, "WALLPAPER.TXT", b"future home of wallpaper.png\n")?;
    // M8: extra seeds from the command line (user ELFs, AUTOEXEC script, ...).
    // Each is stored at the root under its uppercase 8.3 name so the kernel's
    // FAT driver (8.3 short names) and shell path lookups find them.
    for seed in seeds {
        let data = std::fs::read(seed).map_err(|e| {
            std::io::Error::new(e.kind(), format!("read seed {seed}: {e}"))
        })?;
        let name = std::path::Path::new(seed)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad seed name"))?;
        write_seed(&root, name, &data)?;
        eprintln!("mkfat: seeded {name} ({} bytes)", data.len());
    }
    Ok(())
}

fn write_seed(root: &fatfs::Dir<&File>, name: &str, data: &[u8]) -> std::io::Result<()> {
    let mut f = root.create_file(name)?;
    f.truncate()?;
    f.write_all(data)?;
    Ok(())
}