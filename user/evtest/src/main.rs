//! M9.6-B4 ring-3 E2E test: consume raw input events via SYS_INPUT_READ.
//!
//! Reads the kernel's raw event ring (fixed 24-byte little-endian records:
//! kind/flags/code/dx/dy/ts_ns, see kernel::input) and prints every event it
//! sees on serial. It exercises BOTH the blocking path (first call parks the
//! task until an IRQ pushes an event; the scheduler wake pass resumes it) and
//! the non-blocking drain path (follow-up polls). The headless test drives it by
//! typing run /EVTEST.ELF at the shell, then injecting mouse moves/buttons and
//! key presses via QEMU's monitor; it prints a summary and exits with code 0
//! when it saw at least one key event and one mouse event (else 1). The shell's
//! B3 run then reports the exit code, closing the E2E loop.

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_SLEEP: u64 = 10;
const SYS_INPUT_READ: u64 = 16;

// Event kinds (mirror kernel::input).
const EV_KIND_KEY: u8 = 1;
const EV_KIND_MOUSE: u8 = 2;
// Key event flags.
const KEY_DOWN: u8 = 1;
const KEY_UP: u8 = 2;
// Fixed wire record size.
const EV_SIZE: usize = 24;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

fn say(msg: &[u8]) {
    unsafe {
        let _ = syscall3(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
    }
}

fn sayln(s: &[u8]) {
    say(s);
    say(b"\n");
}

fn say_dec(v: u64) {
    if v == 0 {
        say(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = v;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    say(&buf[i..]);
}

// Consume raw input until both a key event and a mouse event were seen
// (or a 100-iteration timeout, ~30 s at 300 ms/iteration), then summarize.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut saw_key = false;
    let mut saw_mouse = false;
    let mut key_count: u32 = 0;
    let mut mouse_count: u32 = 0;
    let mut buf = [0u8; 96]; // up to 4 records per call
    for _ in 0..100 {
        let blocking = if saw_key || saw_mouse { 0 } else { 1 };
        let n = unsafe {
            syscall3(SYS_INPUT_READ, buf.as_mut_ptr() as u64, buf.len() as u64, blocking as u64)
        };
        if (n as i64) < 0 {
            sayln(b"[ev] syscall failed (bad buffer?)");
            unsafe {
                syscall3(SYS_EXIT, 2, 0, 0);
            }
            loop {
                core::hint::spin_loop();
            }
        }
        let mut off = 0;
        while off + EV_SIZE <= n as usize {
            let kd = buf[off];
            let fl = buf[off + 1];
            let code = (buf[off + 2] as u16) | ((buf[off + 3] as u16) << 8);
            let x = (buf[off + 4] as i32)
                | ((buf[off + 5] as i32) << 8)
                | ((buf[off + 6] as i32) << 16)
                | ((buf[off + 7] as i32) << 24);
            let y = (buf[off + 8] as i32)
                | ((buf[off + 9] as i32) << 8)
                | ((buf[off + 10] as i32) << 16)
                | ((buf[off + 11] as i32) << 24);
            let mut ts: u64 = 0;
            for i in 0..8 {
                ts |= (buf[off + 16 + i] as u64) << (i * 8);
            }
            if kd == EV_KIND_KEY {
                say(b"[ev] K ");
                if fl & KEY_DOWN != 0 {
                    say(b"down ");
                } else if fl & KEY_UP != 0 {
                    say(b"up ");
                } else {
                    say(b"?? ");
                }
                say(b"code=");
                say_dec(code as u64);
                say(b" ts=");
                say_dec(ts);
                sayln(b"");
                saw_key = true;
                key_count += 1;
            } else if kd == EV_KIND_MOUSE {
                say(b"[ev] M x=");
                say_dec(x as u64 & 0xFFFF_FFFF);
                say(b" y=");
                say_dec(y as u64 & 0xFFFF_FFFF);
                say(b" b=");
                say_dec(fl as u64);
                say(b" ts=");
                say_dec(ts);
                sayln(b"");
                saw_mouse = true;
                mouse_count += 1;
            } else {
                sayln(b"[ev] unknown kind (driver desync)");
            }
            off += EV_SIZE;
        }
        if saw_key && saw_mouse {
            break;
        }
        unsafe {
            let _ = syscall3(SYS_SLEEP, 300_000_000, 0, 0);
        }
    }
    say(b"[ev] summary key=");
    say_dec(key_count as u64);
    say(b" mouse=");
    say_dec(mouse_count as u64);
    sayln(if saw_key && saw_mouse { b" PASSED" } else { b" FAILED" });
    let code = if saw_key && saw_mouse { 0 } else { 1 };
    unsafe {
        syscall3(SYS_EXIT, code, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}