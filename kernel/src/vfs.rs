//! M6/M7/M8: minimal VFS behind a `FileSystem` trait so FAT32 and ext2 slot
//! in without touching syscalls. M8: paths — every trait method takes a
//! `/`-separated path (root = "/") and `mkdir` creates directories.
//! C3: a real mount table replaces the M6/M7 hardcoded `FS`/`FS2` globals —
//! `MOUNTS` holds every mounted filesystem with a mount point and a label;
//! path ops resolve the longest-prefix (FIFO) mount, `list` still surfaces
//! every mounted view of the path.
//!
//! Lock ordering (no deadlock): always mounts first, then the block layer's
//! DISK.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

use crate::ata::{AtaDrive, AtaError};
use crate::serial_writeln;

#[derive(Debug)]
pub enum FsError {
    NotFound,
    Exists,
    /// Not supported by this backend (cross-directory move, bad argument...).
    NotSupported,
    NoSpace,
    /// The on-disk structure is invalid.
    BadFs,
    Io(AtaError),
    /// The target is a directory where a plain-file operation was needed.
    IsDir,
}

/// Metadata about one path (SYS_STAT / seek-at-END). Minimal now; grows with
/// timestamps/links when the M9.7 (Linux struct stat) work needs them.
#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    pub size: u64,
    pub is_dir: bool,
}

pub struct DirEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

pub trait FileSystem: Send {
    /// Listing of the directory at `path` (`/` = root). Non-directories are
    /// rejected with `NotSupported`.
    fn list(&mut self, dev: &mut AtaDrive, path: &str) -> Result<Vec<DirEntry>, FsError>;
    /// Returns Ok(()) if `path` exists (file or directory).
    fn open(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError>;
    fn read_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError>;
    fn write_at(
        &mut self,
        dev: &mut AtaDrive,
        path: &str,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError>;
    /// Create the directory at `path`; the parent must already exist.
    fn mkdir(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError>;
    /// Metadata about `path` (file or directory).
    fn stat(&mut self, dev: &mut AtaDrive, path: &str) -> Result<FileStat, FsError> {
        let _ = (dev, path);
        Err(FsError::NotSupported)
    }
    /// Remove the file at `path`. Directories are rejected with `IsDir`.
    fn unlink(&mut self, dev: &mut AtaDrive, path: &str) -> Result<(), FsError> {
        let _ = (dev, path);
        Err(FsError::NotSupported)
    }
    /// Rename `from` to `to` within one directory tree.
    fn rename(&mut self, dev: &mut AtaDrive, from: &str, to: &str) -> Result<(), FsError> {
        let _ = (dev, from, to);
        Err(FsError::NotSupported)
    }
}

/// Split an absolute path into components: `/a/b` -> `["a", "b"]`, `/` -> `[]`.
/// `None` for malformed paths: no leading `/`, a `.`/`..` component, or an
/// over-long path. Empty components (`//`, a trailing `/`) are tolerated.
pub(crate) fn split_path(path: &str) -> Option<Vec<&str>> {
    if !path.starts_with('/') || path.len() > 512 {
        return None;
    }
    let mut out = Vec::new();
    for comp in path[1..].split('/') {
        if comp.is_empty() {
            continue;
        }
        if comp == "." || comp == ".." || comp.len() > 255 {
            return None;
        }
        out.push(comp);
    }
    Some(out)
}

/// One mounted filesystem: its mount point (prefix of path ops that target
/// it), a human label for listings, and the driver behind it. Both current
/// mounts sit at `/`, so path ops pick the first (FAT32) and `list` shows
/// every mounted view — preserving the M6/M7 dual-mount behavior without the
/// hardcoded globals.
struct Mount {
    point: String,
    label: &'static str,
    fs: Box<dyn FileSystem>,
}

/// The mount table (C3). Registered at boot: FAT32 (primary, "/") then ext2
/// (secondary, "/"). New mounts at distinct points slot in untouched.
static MOUNTS: Mutex<Vec<Mount>> = Mutex::new(Vec::new());

/// Does `path` resolve onto the mount at `point`? `/` matches everything; a
/// mount point `/x` matches exactly `/x` and anything under `/x/`.
fn mount_matches(point: &str, path: &str) -> bool {
    if point == "/" {
        return true;
    }
    path.strip_prefix(point)
        .map_or(false, |rest| rest.is_empty() || rest.starts_with('/'))
}

/// Index of the primary mount for a path among `mounts` (already locked): the
/// longest-prefix match, ties broken FIFO (so `/` + `/` picks the first
/// mount, i.e. FAT32).
fn pick_mount<'a>(mounts: &'a [Mount], path: &str) -> Option<usize> {
    mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| mount_matches(&m.point, path))
        .max_by_key(|(i, m)| (m.point.len(), usize::MAX - *i))
        .map(|(i, _)| i)
}

/// Run `f` against the primary mount for `path` (and the drive). Mounts
/// first, then DISK — the documented lock order.
fn with_fs<T>(
    path: &str,
    f: impl FnOnce(&mut Box<dyn FileSystem>, &mut AtaDrive) -> T,
) -> Result<T, FsError> {
    let mut guard = MOUNTS.lock();
    let idx = pick_mount(&guard, path).ok_or(FsError::BadFs)?;
    let fs = &mut guard[idx].fs;
    crate::block::with_disk(|disk| f(fs, &mut disk.drive)).ok_or(FsError::BadFs)
}

/// Find the FAT32 partition on the boot disk and mount it at "/".
pub fn init() {
    let mounted = crate::block::with_disk(|disk| {
        // Prefer a real FAT32 partition: the bootloader's internal FAT volume
        // claims type 0x0C too, but it's tiny (FAT12/16 in practice). Our M6
        // partition (appended by build.rs, >= 32 MiB) is the real FAT32.
        let Some(part) = disk.partitions.iter().find(|p| {
            p.kind.contains("FAT32") && (p.sectors * 512) >= (8 * 1024 * 1024)
        }) else {
            serial_writeln!("M6: no large FAT32 partition found on boot disk");
            return None;
        };
        match crate::fat::Fat32::mount(&mut disk.drive, part.start_lba as u32) {
            Ok(fs) => {
                serial_writeln!(
                    "M6: FAT32 mounted (LBA {}): {}",
                    part.start_lba,
                    fs.summary()
                );
                Some(Box::new(fs) as Box<dyn FileSystem>)
            }
            Err(e) => {
                serial_writeln!("M6: FAT32 mount failed: {e:?}");
                None
            }
        }
    })
    .flatten();
    if let Some(fs) = mounted {
        MOUNTS.lock().push(Mount {
            point: "/".to_string(),
            label: "FAT32",
            fs,
        });
    }

    match list("/") {
        Ok(entries) => {
            serial_writeln!("M6: root directory: {} entries", entries.len());
            for e in &entries {
                serial_writeln!(
                    "M6:   {} ({} bytes{})",
                    e.name,
                    e.size,
                    if e.is_dir { ", dir" } else { "" }
                );
            }
        }
        Err(e) => serial_writeln!("M6: root listing failed: {e:?}"),
    }
}

/// Install a secondary filesystem (called by `ext2::init` at boot) at "/".
pub fn mount_secondary(fs: Box<dyn FileSystem>) {
    MOUNTS.lock().push(Mount {
        point: "/".to_string(),
        label: "ext2",
        fs,
    });
}

/// Directory listing at `path` on the primary mount.
pub fn list(path: &str) -> Result<Vec<DirEntry>, FsError> {
    with_fs(path, |fs, dev| fs.list(dev, path))?
}

/// Listing of `path` on the secondary mount (compat shim for the shell's
/// dual-mount SYS_LS output; ext2 historically mounted beside FAT32).
pub fn list2(path: &str) -> Result<Vec<DirEntry>, FsError> {
    let mut guard = MOUNTS.lock();
    let idx = guard.iter().position(|m| m.label == "ext2").ok_or(FsError::BadFs)?;
    let fs = &mut guard[idx].fs;
    crate::block::with_disk(|disk| fs.list(&mut disk.drive, path)).ok_or(FsError::BadFs)?
}

/// Validate that `path` exists (used by SYS_OPEN).
pub fn open(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, dev| fs.open(dev, path))?
}

/// Create the directory at `path` (the parent must exist).
pub fn mkdir(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, dev| fs.mkdir(dev, path))?
}

/// Read up to `buf.len()` bytes of `path` at file offset `off`.
pub fn read_at(path: &str, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    with_fs(path, |fs, dev| fs.read_at(dev, path, off, buf))?
}

/// Write `buf` at file offset `off` (creates the file if missing).
pub fn write_at(path: &str, off: u64, buf: &[u8]) -> Result<usize, FsError> {
    with_fs(path, |fs, dev| fs.write_at(dev, path, off, buf))?
}

/// Metadata about `path`.
pub fn stat(path: &str) -> Result<FileStat, FsError> {
    with_fs(path, |fs, dev| fs.stat(dev, path))?
}

/// Remove the file at `path` (directories rejected with `IsDir`).
pub fn unlink(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, dev| fs.unlink(dev, path))?
}

/// Rename `from` to `to`.
pub fn rename(from: &str, to: &str) -> Result<(), FsError> {
    with_fs(from, |fs, dev| fs.rename(dev, from, to))?
}