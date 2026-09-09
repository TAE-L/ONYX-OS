//! Smallest OnyxOS ring-3 program: one `write(1, msg, len)` syscall through
//! the kernel's `syscall`/`sysret` fast path, then `exit(42)`.
//!
//! Syscall ABI (implemented kernel-side in `kernel/src/syscall.rs`):
//!   rax = syscall number, rdi/rsi/rdx = args, result in rax.
//!   rcx and r11 are clobbered by the CPU itself (user rip / rflags).
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

/// SYS_WRITE(fd, buf, len): write bytes to the serial console.
const SYS_WRITE: u64 = 0;
/// SYS_EXIT(code): never returns; kernel turns the task into a zombie.
const SYS_EXIT: u64 = 1;

const MSG: &[u8] = b"hello from ring 3!\n";

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    // No console here; spin. (hlt would raise #GP at ring 3.)
    loop {
        core::hint::spin_loop();
    }
}

/// rax = nr, args in rdi/rsi/rdx, result in rax.
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

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        let _ = syscall3(SYS_WRITE, 1, MSG.as_ptr() as u64, MSG.len() as u64);
        syscall3(SYS_EXIT, 42, 0, 0);
    }
    // Unreachable in practice: SYS_EXIT turns us into a scheduler zombie.
    loop {
        core::hint::spin_loop();
    }
}