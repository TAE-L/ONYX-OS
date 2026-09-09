//! Host-side launcher for OnyxOS.
//!
//! This binary runs on Windows (not inside the OS) and boots the disk images
//! that `build.rs` produced. Everything lives inside the project folder and the
//! guest only ever runs inside QEMU — never on this machine's hardware.
//!
//! Usage:
//!   cargo run -- bios       headless + serial to stdout (no window)
//!   cargo run -- bios-gui   open a graphical QEMU window
use std::env;
use std::path::Path;
use std::process::Command;

fn qemu_exe() -> String {
    if let Ok(p) = env::var("QEMU_SYSTEM_X86_64") {
        if Path::new(&p).exists() {
            return p;
        }
    }
    let portable = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".toolchain")
        .join("qemu")
        .join("qemu-system-x86_64.exe");
    if portable.exists() {
        return portable.to_string_lossy().into_owned();
    }
    "qemu-system-x86_64".to_string()
}

fn main() {
    let bios_path = env!("BIOS_PATH");
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bios");

    let qemu = qemu_exe();

    let status = match mode {
        "bios" => run_bios(&qemu, bios_path, false),
        "bios-gui" => run_bios(&qemu, bios_path, true),
        "uefi" => {
            eprintln!(
                "UEFI boot needs OVMF firmware (milestone M8). \
                 Only the BIOS image is currently created: {bios_path}"
            );
            run_bios(&qemu, bios_path, false)
        }
        _ => {
            eprintln!("usage: onyxos [bios|bios-gui|uefi]");
            std::process::exit(2);
        }
    };

    std::process::exit(match status {
        Some(code) => code,
        None => 1,
    });
}

fn run_bios(qemu: &str, bios_path: &str, gui: bool) -> Option<i32> {
    let mut cmd = Command::new(qemu);
    cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    cmd.arg("-no-reboot").arg("-no-shutdown").arg("-m").arg("128M");
    // std VGA exposes the full VBE mode set (incl. 1920x1080x32) we request.
    cmd.arg("-vga").arg("std");
    if gui {
        // zoom-to-fit scales the 1920x1080 console into the window so it
        // always fits a laptop screen (never bigger than the display).
        cmd.arg("-display").arg("gtk,zoom-to-fit=on");
        cmd.arg("-serial").arg("mon:stdio");
    } else {
        cmd.arg("-display").arg("none");
        cmd.arg("-serial").arg("stdio");
    }
    match cmd.status() {
        Ok(s) => Some(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("failed to run QEMU ({qemu}): {e}");
            None
        }
    }
}