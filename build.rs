//! Build script: combines the compiled kernel with the `bootloader` crate to
//! produce bootable BIOS (MBR) and UEFI (GPT) disk images.
//!
//! Mirrors the official rust-osdev `bootloader` "basic" example layout.
use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // Set by cargo's artifact-dependency feature for `kernel = { artifact = "bin" }`.
    let kernel = PathBuf::from(env::var_os("CARGO_BIN_FILE_KERNEL_kernel").unwrap());

    // Request a standard full-HD framebuffer from the bootloader.
    let mut boot_config = bootloader::BootConfig::default();
    boot_config.frame_buffer.minimum_framebuffer_width = Some(1920);
    boot_config.frame_buffer.minimum_framebuffer_height = Some(1080);

    // The kernel ELF is full of DWARF debug info (4 MB+ at -O0 even with
    // `debug=0`). The builder sizes its internal FAT volume from the kernel
    // *file size*; a 4 MB kernel pushes that volume past the FAT16/FAT32
    // cluster-count threshold, so the FAT gets auto-created as FAT16 — which
    // the bootloader stage-2's embedded FAT32-only driver can't read. Strip
    // the kernel before handing it to the builder: shrinks it to ~250 KB and
    // keeps the bootloader's FAT a proper FAT32. (Runtime behavior unchanged.)
    let kernel = strip_kernel(kernel);

    // Create the base BIOS (MBR) disk image from the bootloader.
    let bios_path = out_dir.join("bios.img");
    // Start from a clean slate: if a previous run appended the FAT32
    // partition but died before the copy step, a leftover image here would
    // get a *second* partition stacked at the end. `create_bios_image`
    // overwrites, but don't rely on it truncating.
    let _ = std::fs::remove_file(&bios_path);
    let mut builder = bootloader::DiskImageBuilder::new(kernel);
    builder.set_boot_config(&boot_config);
    builder.create_bios_image(&bios_path).unwrap();

    // M6 filesystem: append a guaranteed-FAT32 partition for the kernel's VFS
    // + fstest. The bootloader's own FAT (used for its internal files) can be
    // FAT12/16 depending on size; our dedicated one is always FAT32.
    append_fat32_partition(&bios_path, &out_dir);

    // M7 filesystem: append an ext2 partition for the VFS secondary mount.
    // MBR entry 4 (index 3) is the FAT32; ext2 takes entry 3 (index 2), the
    // last free slot of the bootloader's 4-entry MBR.
    append_ext2_partition(&bios_path, &out_dir);

    // M9: also build the UEFI (GPT) image from the same kernel, and append the
    // same FAT32 + ext2 partitions. The kernel's block layer (M5) already
    // parses GPT and `vfs`/`ext2` select partitions by `kind`, so the same
    // filesystem stack mounts on the UEFI image too.
    let uefi_path = out_dir.join("uefi.img");
    let _ = std::fs::remove_file(&uefi_path);
    builder.create_uefi_image(&uefi_path).unwrap();
    append_fat32_partition(&uefi_path, &out_dir);
    append_ext2_partition(&uefi_path, &out_dir);

    // Pass the image path to the crate at compile time.
    println!("cargo:rustc-env=BIOS_PATH={}", bios_path.display());

    // Also copy the image to a stable, predictable location for scripts/tools.
    let profile = env::var("PROFILE").expect("PROFILE env var must be set by cargo");
    let images_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(profile)
        .join("images");
    std::fs::create_dir_all(&images_dir).expect("failed to create images dir");
    std::fs::copy(&bios_path, images_dir.join("bios.img")).expect("failed to copy bios.img");
    std::fs::copy(&uefi_path, images_dir.join("uefi.img")).expect("failed to copy uefi.img");
}

/// Strip DWARF/debug sections from the kernel ELF so the bootloader's FAT
/// volume (sized from the kernel file size) stays a FAT32. Uses rustc's own
/// bundled `llvm-strip`.
fn strip_kernel(path: PathBuf) -> PathBuf {
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .expect("RUSTUP_HOME must be set (portable toolchain)");
    let toolchain_dir = PathBuf::from(rustup_home).join("toolchains");
    let strip = find_tool(&toolchain_dir, "llvm-strip.exe").expect("llvm-strip.exe not found");
    let out = path.with_extension("kernel-stripped");
    let ok = std::process::Command::new(&strip)
        .arg("--strip-all")
        .arg(&path)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("failed to run llvm-strip");
    assert!(ok.success(), "llvm-strip failed");
    eprintln!(
        "stripped kernel: {} -> {} bytes",
        path.display(),
        out.metadata().map(|m| m.len()).unwrap_or(0)
    );
    out
}

/// Append a guaranteed-FAT32 filesystem as MBR partition 3 of the boot image.
///
/// The bootloader only creates its own tiny internal FAT (kernel file) for
/// stage-2; M6 needs a real FAT32 to mount. This builds one with the `mkfat`
/// helper and writes it into MBR entry index 3.
fn append_fat32_partition(image_path: &Path, out_dir: &Path) {
    // Build the `mkfat` helper from the workspace `tools/mkfat` crate, then
    // run it to produce a guaranteed-FAT32 filesystem image.
    //
    // DEADLOCK WARNING: this build script runs *inside* `cargo build`, which
    // holds the exclusive lock on the shared `target/` dir. A nested `cargo`
    // build pointed at the same target dir would block on that lock forever
    // (observed: root build stuck at "92/94: onyxos(build)"). So the nested
    // build gets its own CARGO_TARGET_DIR, and is skipped entirely when the
    // binary is already present.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mkfat_crate = root.join("tools").join("mkfat");
    let nested_target = root.join("target").join("mkfat-build");
    let mkfat = nested_target
        .join("debug")
        .join(if cfg!(windows) { "mkfat.exe" } else { "mkfat" });

    // Always (re)build mkfat: cargo no-ops when the tool is already fresh,
    // and the existence guard below would otherwise keep a STALE binary after
    // mkfat's sources change, silently regenerating outdated FS images.
    let build = std::process::Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", "mkfat"])
        .env("CARGO_TARGET_DIR", &nested_target)
        .current_dir(&mkfat_crate)
        .output()
        .expect("failed to build mkfat");
    assert!(
        build.status.success(),
        "cargo build mkfat failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(mkfat.exists(), "mkfat binary not found at {}", mkfat.display());

    let fat_path = out_dir.join("onyx-fat32.img");
    // M8 seeds: the shell ELF (loaded from disk by the kernel), the fstest
    // ELF (spawned by the shell via SYS_SPAWN) and the shell's boot script.
    // Seeded files must have uppercase 8.3 names — fatfs stores them as pure
    // short-name directory entries, which the kernel's FAT driver reads.
    let shell_elf = PathBuf::from(env::var_os("CARGO_BIN_FILE_USER_SHELL_shell").unwrap());
    let fstest_elf = PathBuf::from(env::var_os("CARGO_BIN_FILE_USER_FSTEST_fstest").unwrap());
    let evtest_elf = PathBuf::from(env::var_os("CARGO_BIN_FILE_USER_EVTEST_evtest").unwrap());
    let argtest_elf = PathBuf::from(env::var_os("CARGO_BIN_FILE_USER_ARGTEST_argtest").unwrap());
    let shell_seed = copy_seed(&shell_elf, out_dir, "SHELL.ELF");
    let fstest_seed = copy_seed(&fstest_elf, out_dir, "FSTEST.ELF");
    let evtest_seed = copy_seed(&evtest_elf, out_dir, "EVTEST.ELF");
    let argtest_seed = copy_seed(&argtest_elf, out_dir, "ARGTEST.ELF");
    let autoexec_seed = out_dir.join("AUTOEXEC.TXT");
    std::fs::write(
        &autoexec_seed,
        concat!(
            "echo shell online (M8)\n",
            "ls /\n",
            "mkdir /DOCS\n",
            "write /DOCS/NOTE.TXT written by the ring-3 shell\n",
            "cat /DOCS/NOTE.TXT\n",
            // C3 shell smoke: `stat` on an existing file prints size/type;
            // `rm` of a missing file reports "no such file" (idempotent).
            "stat /DOCS/NOTE.TXT\n",
            "rm /DOCS/NOPE.TXT\n",
            "run /FSTEST.ELF\n",
            // M9.6-C1: proves the kernel tokenizes the command line and
            // builds the System V process-start stack (argc/argv/auxv).
            "run /ARGTEST.ELF alpha beta gamma\n",
            "echo autoexec done - entering interactive mode\n",
        ),
    )
    .expect("failed to write AUTOEXEC.TXT seed");

    // 40 MiB: FAT32 needs >= 65525 clusters; even with 512-byte clusters a
    // 32 MiB volume tops out below that, so 40 gives comfortable margin.
    // `.output()` (not `.status()`) so mkfat's stdout can't leak into this
    // build script's stdout, which cargo parses for `cargo:` directives.
    let run = std::process::Command::new(&mkfat)
        .arg(&fat_path)
        .arg("40")
        .arg(&shell_seed)
        .arg(&fstest_seed)
        .arg(&evtest_seed)
        .arg(&argtest_seed)
        .arg(&autoexec_seed)
        .output()
        .expect("failed to run mkfat");
    assert!(
        run.status.success(),
        "mkfat failed:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    // Open the boot image and append the partition image + MBR entry.
    use std::io::{Seek, SeekFrom, Write};
    let mut img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .expect("failed to open boot image");
    let img_len = img.metadata().unwrap().len();

    let fat_bytes = std::fs::read(&fat_path).expect("failed to read mkfat output");
    let fat_size = fat_bytes.len() as u64;
    let aligned_len = (img_len + 511) & !511;
    img.seek(SeekFrom::Start(aligned_len)).unwrap();
    img.write_all(&fat_bytes).unwrap();
    let new_end = aligned_len + fat_size;

    // MBR entry 3.
    let start_sector = (aligned_len / 512) as u32;
    let size_sectors = ((fat_size + 511) / 512) as u32;
    let mut mbr = [0u8; 16];
    mbr[4] = 0x0C; // FAT32 (LBA)
    mbr[8..12].copy_from_slice(&start_sector.to_le_bytes());
    mbr[12..16].copy_from_slice(&size_sectors.to_le_bytes());
    img.seek(SeekFrom::Start(446 + 3 * 16)).unwrap();
    img.write_all(&mbr).unwrap();

    img.set_len(new_end).unwrap();
    eprintln!(
        "appended FAT32 partition: start_lba={start_sector} sectors={size_sectors} (new size {new_end})"
    );
}

/// Build `mkext2`, generate an 8 MiB ext2 volume and append it to the BIOS
/// image as MBR entry 3 (index 2), typed 0x83 ("Linux").
fn append_ext2_partition(image_path: &Path, out_dir: &Path) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mkext2_crate = root.join("tools").join("mkext2");
    let nested_target = root.join("target").join("mkext2-build");
    let mkext2 = nested_target
        .join("debug")
        .join(if cfg!(windows) { "mkext2.exe" } else { "mkext2" });

    // Always (re)build mkext2: cargo no-ops when the tool is already fresh,
    // and the guard below would otherwise keep a STALE binary after mkext2's
    // sources change, silently regenerating outdated filesystem images.
    let build = std::process::Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", "mkext2"])
        .env("CARGO_TARGET_DIR", &nested_target)
        .current_dir(&mkext2_crate)
        .output()
        .expect("failed to build mkext2");
    assert!(
        build.status.success(),
        "cargo build mkext2 failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        mkext2.exists(),
        "mkext2 binary not found at {}",
        mkext2.display()
    );

    let ext2_path = out_dir.join("onyx-ext2.img");
    // 8 MiB: the single-block-group maximum for 1 KiB blocks (mkext2 clamps
    // to it), leaving plenty of free blocks/inodes for the driver's writes.
    // `.output()` (not `.status()`) so stdout can't leak into this build
    // script's stdout, which cargo parses for `cargo:` directives.
    let run = std::process::Command::new(&mkext2)
        .arg(&ext2_path)
        .arg("8")
        .output()
        .expect("failed to run mkext2");
    assert!(
        run.status.success(),
        "mkext2 failed:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    // Open the boot image and append the partition image + MBR entry.
    use std::io::{Seek, SeekFrom, Write};
    let mut img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .expect("failed to open boot image");
    let img_len = img.metadata().unwrap().len();

    let ext2_bytes = std::fs::read(&ext2_path).expect("failed to read mkext2 output");
    let ext2_size = ext2_bytes.len() as u64;
    let aligned_len = (img_len + 511) & !511;
    img.seek(SeekFrom::Start(aligned_len)).unwrap();
    img.write_all(&ext2_bytes).unwrap();
    let new_end = aligned_len + ext2_size;

    // MBR entry 3 (index 2).
    let start_sector = (aligned_len / 512) as u32;
    let size_sectors = ((ext2_size + 511) / 512) as u32;
    let mut mbr = [0u8; 16];
    mbr[4] = 0x83; // Linux
    mbr[8..12].copy_from_slice(&start_sector.to_le_bytes());
    mbr[12..16].copy_from_slice(&size_sectors.to_le_bytes());
    img.seek(SeekFrom::Start(446 + 2 * 16)).unwrap();
    img.write_all(&mbr).unwrap();

    img.set_len(new_end).unwrap();
    eprintln!(
        "appended ext2 partition: start_lba={start_sector} sectors={size_sectors} (new size {new_end})"
    );
}

/// Copy a seed file into `out_dir` under a fixed uppercase 8.3 name (fatfs
/// then stores it as a pure short-name entry the kernel FAT driver can see).
fn copy_seed(src: &Path, out_dir: &Path, name: &str) -> PathBuf {
    let dst = out_dir.join(name);
    std::fs::copy(src, &dst).unwrap_or_else(|e| panic!("failed to copy seed {name}: {e}"));
    dst
}

/// Recursively find the first `name` under `root` (rustc's bundled LLVM tools).
fn find_tool(root: &Path, name: &str) -> Option<PathBuf> {
    let mut out = None;
    walk_tools(root, name, &mut out);
    out
}

fn walk_tools(dir: &Path, name: &str, found: &mut Option<PathBuf>) {
    if found.is_some() {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_tools(&p, name, found);
            } else if p.file_name().map(|n| n == name).unwrap_or(false) {
                *found = Some(p);
                return;
            }
        }
    }
}