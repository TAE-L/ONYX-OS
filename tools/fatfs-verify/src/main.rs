//! Independent FAT32 reader oracle (host side).
//!
//! Opens a raw FAT32 **partition** image with the third-party `fatfs` crate and
//! checks that the long filenames the kernel wrote are visible, correctly spelled
//! and holding the right bytes.
//!
//! Why a third oracle: `tests/fat32-verify.ps1` is already an independent reader,
//! but it was written by the same person who wrote the writer. This one is a
//! published crate, so a shared misunderstanding is less likely. Two bugs in
//! `fat.rs` (a checksum that was not the checksum, and reversed LFN chunk order)
//! were invisible to every in-kernel test and only surfaced against the spec.
//!
//! Usage: fatfs-verify <fat32-partition-image>

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;

fn main() {
    // The path is the first argument that is not a flag, so the tool works with the
    // flag either before or after it.
    let path = match env::args().skip(1).find(|a| !a.starts_with("--")) {
        Some(p) => p.to_string(),
        None => {
            eprintln!("usage: fatfs-verify <fat32-partition-image> [--selftest]");
            std::process::exit(2);
        }
    };
    let file = File::open(&path).unwrap_or_else(|e| {
        eprintln!("cannot open {path}: {e}");
        std::process::exit(2);
    });
    let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).unwrap_or_else(|e| {
        // The overwhelmingly likely cause is being handed a whole-disk image
        // instead of the bare partition, which `FileSystem::new` cannot see past.
        eprintln!("cannot mount {path} as FAT32: {e}");
        eprintln!("hint: this tool wants the FAT32 PARTITION only, not the whole disk.");
        std::process::exit(2);
    });

    // `fatfs-verify --selftest <img>`: prove the ORACLE can read a multi-slot long
    // name before trusting its verdict on ours. Without this, a reader bug looks
    // exactly like a writer bug -- and that confusion cost real time.
    if env::args().any(|a| a == "--selftest") {
        selftest(&path);
        return;
    }

    let root = fs.root_dir();
    let mut found: BTreeMap<String, (String, u64)> = BTreeMap::new();
    println!("root directory as read by the `fatfs` crate:");
    for entry in root.iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                println!("  <unreadable entry: {e}>");
                continue;
            }
        };
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let (short, size) = (entry.short_file_name(), entry.len());
        println!("  {name:<46} short={short:<14} size={size}");
        found.insert(name, (short, size));
    }

    // (name, expected size, expected contents) -- the sizes come from what
    // build.rs writes in AUTOEXEC.TXT.
    let expected: [(&str, u64, &str); 6] = [
        ("ABCDEFGHIJKLM", 11, "boundary 13"),
        ("ABCDEFGHIJKLMN", 11, "boundary 14"),
        ("InteropTwentyCharsLong", 2, "24"),
        ("InteropVeryLongDocumentNameFortyCharsIndeed", 2, "43"),
        ("interop.lower.name.txt", 9, "lowercase"),
        ("LEGAL83.TXT", 15, "eight dot three"),
    ];

    let mut fails = 0usize;
    let mut oks = 0usize;
    for (name, size, text) in expected {
        match found.get(name) {
            None => {
                println!("FAIL: '{name}' is not visible to an independent reader");
                fails += 1;
            }
            Some((_, got)) if *got != size => {
                println!("FAIL: '{name}' size {got}, expected {size}");
                fails += 1;
            }
            Some(_) => match read_file(&root, name) {
                Some(c) if c == text => {
                    println!("ok:   '{name}' visible, {size} bytes, contents match");
                    oks += 1;
                }
                other => {
                    println!("FAIL: '{name}' contents {other:?}, expected {text:?}");
                    fails += 1;
                }
            },
        }
    }

    // Two names sharing a base must not share an 8.3 alias, or one shadows the
    // other in every FAT reader including this one.
    let a = found.get("ABCDEFGHIJKLM").map(|x| x.0.clone());
    let b = found.get("ABCDEFGHIJKLMN").map(|x| x.0.clone());
    match (a, b) {
        (Some(a), Some(b)) if a == b => {
            println!("FAIL: both names resolved to the same 8.3 alias '{a}'");
            fails += 1;
        }
        (Some(a), Some(b)) => {
            println!("ok:   colliding bases have distinct aliases ({a} / {b})");
            oks += 1;
        }
        _ => {}
    }

    println!();
    if fails > 0 {
        println!("FATFS ORACLE: {oks} ok, {fails} FAILED");
        std::process::exit(1);
    }
    println!("FATFS ORACLE: {oks} checks passed");
}

/// Have `fatfs` write a long name that needs MORE THAN ONE LFN slot, then read it
/// back with the same crate. If it cannot, this crate is not a usable oracle for
/// multi-slot names and its verdict on ours is meaningless.
fn selftest(path: &str) {
    // Deliberately distinctive prefixes so the oracle's own writes can be told apart
    // from the kernel's identical-looking ones when the two are diffed byte for
    // byte in the same volume.
    const NAMES: [&str; 3] = [
        "ZzzOracleShortName",                        // 17 chars -> 2 slots
        "ZzzOracleTwoSlotProbeName",                 // 26 chars -> 2 slots
        "ZzzOracleFourSlotProbeNameGoesOnForAWhile", // 40 chars -> 4 slots
    ];
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open image read-write");
    {
        let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).expect("mount");
        let root = fs.root_dir();
        for n in NAMES {
            let mut f = root.create_file(n).unwrap_or_else(|e| panic!("create {n}: {e}"));
            use std::io::Write;
            f.write_all(b"selftest").expect("write");
        }
    }
    // Re-open from scratch so nothing is served from a stale cache.
    let file2 = File::open(path).expect("reopen");
    let fs2 = fatfs::FileSystem::new(&file2, fatfs::FsOptions::new()).expect("remount");
    let mut bad = 0;
    for n in NAMES {
        let visible = fs2.root_dir().iter().flatten().any(|e| e.file_name() == n);
        println!(
            "{}: fatfs wrote and re-read its own {} ({} chars, needs {} slot(s))",
            if visible { "ok  " } else { "FAIL" },
            n,
            n.chars().count(),
            n.chars().count().div_ceil(13)
        );
        if !visible {
            bad += 1;
        }
    }
    println!();
    if bad > 0 {
        println!("ORACLE SELF-TEST: {bad} of {} names unreadable by fatfs itself", NAMES.len());
        println!("=> fatfs CANNOT be used to judge multi-slot long names.");
        std::process::exit(1);
    }
    println!("ORACLE SELF-TEST: fatfs reads back all of its own multi-slot names; it is a valid oracle.");
}

fn read_file<T: fatfs::ReadWriteSeek>(dir: &fatfs::Dir<'_, T>, name: &str) -> Option<String> {
    // `Dir::open_file`, not `FileSystem::open_file` -- the latter does not exist
    // in fatfs 0.3, and `FileSystem<T>` needs T: ReadWriteSeek, not Read+Seek.
    let mut f = dir.open_file(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}