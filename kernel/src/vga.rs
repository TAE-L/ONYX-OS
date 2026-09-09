//! Minimal VGA text-mode (80x25) writer at 0xB8000.
//!
//! Only used in legacy BIOS mode for now; a graphical framebuffer console
//! arrives in a later milestone.

use core::ptr;

const BUFFER: usize = 0xB8000;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const COLOR: u8 = 0x0F; // white-on-black

struct VgaWriter {
    row: usize,
    col: usize,
}

impl VgaWriter {
    fn new() -> Self {
        Self { row: 0, col: 0 }
    }

    fn write_byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.newline();
            return;
        }
        let idx = (self.row * WIDTH + self.col) * 2;
        unsafe {
            ptr::write_volatile((BUFFER + idx) as *mut u8, byte);
            ptr::write_volatile((BUFFER + idx + 1) as *mut u8, COLOR);
        }
        self.col += 1;
        if self.col >= WIDTH {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row = (self.row + 1) % HEIGHT;
    }
}

/// Write a static string to the VGA text buffer (no scrolling implemented yet).
pub fn write_str(s: &str) {
    let mut writer = VgaWriter::new();
    for &byte in s.as_bytes() {
        writer.write_byte(byte);
    }
}