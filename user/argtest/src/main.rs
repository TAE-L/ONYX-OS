//! M9.6-C1 ring-3 test: the kernel builds a System V AMD64 process-start
//! stack (argc / argv[] / envp[] / auxv / strings / AT_RANDOM) below the
//! stack top and starts the task with rsp pointing at argc. `_start` (global
//! asm, exactly like glibc's) lifts those into rdi/rsi/rdx and calls main,
//! which echoes everything it received. Spawned by the shell's AUTOEXEC as
//! `run /ARGTEST.ELF alpha beta gamma` — argv[0] is the command the user
//! typed, argv[1..] the extra tokens.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYS_WRITE: u64 = 0;

// Process-start trampoline, the glibc `_start` idiom: the kernel leaves
// rsp at `argc`; argv[] follows at rsp+8, envp[] after argv's NULL. Load
// the three into the SysV argument registers, re-align rsp, call main,
// then exit(0) with main's return value.
global_asm!(
    ".globl _start",
    "_start:",
    "xor ebp, ebp",            // outermost frame marker
    "mov rdi, [rsp]",          // argc
    "lea rsi, [rsp + 8]",      // argv
    "lea rdx, [rsp + 8]",      // envp = argv + 8*argc + 8 (past argv's NULL)
    "mov rcx, rdi",
    "shl rcx, 3",
    "add rdx, rcx",
    "add rdx, 8",
    "and rsp, -16",            // ABI: rsp 16-aligned before the call
    "call main",
    "mov rdi, rax",            // exit code = main's return
    "mov rax, 1",              // SYS_EXIT
    "syscall",
    "2: jmp 2b",               // unreachable: exit never returns
);

// auxv keys (Linux ABI).
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

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

/// Console line builder. Each logical line is assembled on the stack and
/// emitted with ONE SYS_WRITE — the kernel holds the serial lock for the
/// entire write, so kernel diagnostics can only ever land as their own whole
/// lines between ours, never split inside a line.
struct LineBuf {
    b: [u8; 160],
    n: usize,
}

impl LineBuf {
    fn new() -> Self {
        LineBuf { b: [0u8; 160], n: 0 }
    }
    fn put(&mut self, s: &[u8]) {
        let take = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + take].copy_from_slice(&s[..take]);
        self.n += take;
    }
    fn dec(&mut self, mut v: u64) {
        let mut d = [0u8; 20];
        let mut i = d.len();
        loop {
            i -= 1;
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.put(&d[i..]);
    }
    fn hex(&mut self, v: u64) {
        const HEX: &[u8] = b"0123456789abcdef";
        let mut d = [0u8; 18];
        d[0] = b'0';
        d[1] = b'x';
        for (k, byte) in d[2..].iter_mut().enumerate() {
            *byte = HEX[((v >> (60 - 4 * k)) & 0xF) as usize];
        }
        self.put(&d);
    }
    /// NUL-terminated string with a hard cap (defensive against garbage;
    /// truncation can only cut the tail of the value).
    fn cstr(&mut self, p: *const u8) {
        let mut n = 0usize;
        while n < self.b.len() - self.n - 1 && unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        self.put(unsafe { core::slice::from_raw_parts(p, n) });
    }
    fn nl(&mut self) {
        self.put(b"\n");
    }
    fn flush(&self) {
        say(&self.b[..self.n]);
    }
}

// `no_mangle` so the global-asm `_start` can reference the symbol (and so
// rustc keeps it: nothing at the Rust level references `main`).
#[no_mangle]
pub extern "C" fn main(argc: u64, argv: *const *const u8, envp: *const *const u8) -> i32 {
    let mut ok = argc >= 1;

    let mut l = LineBuf::new();
    l.put(b"[argtest] argc=");
    l.dec(argc);
    l.nl();
    l.flush();
    for i in 0..argc.min(64) {
        let p = unsafe { *argv.add(i as usize) };
        let mut l = LineBuf::new();
        l.put(b"[argtest] argv[");
        l.dec(i);
        l.put(b"]=");
        l.cstr(p);
        l.nl();
        l.flush();
    }

    // envp[]: count entries (the kernel passes an empty environment today).
    let mut l = LineBuf::new();
    let mut envc = 0u64;
    unsafe {
        let mut e = envp;
        while envc < 64 && !(*e).is_null() {
            envc += 1;
            e = e.add(1);
        }
        l.put(b"[argtest] envc=");
        l.dec(envc);
        l.nl();
        l.flush();

        // auxv: (key, value) pairs after envp's NULL, terminated by AT_NULL.
        let mut aux = e.add(1) as *const u64;
        let mut saw_random = false;
        for _ in 0..64 {
            let (t, v) = (*aux, *aux.add(1));
            match t {
                AT_NULL => break,
                AT_PAGESZ => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_pagesz=");
                    l.dec(v);
                    l.nl();
                    l.flush();
                    if v != 4096 {
                        ok = false;
                    }
                }
                AT_PHENT => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_phent=");
                    l.dec(v);
                    l.nl();
                    l.flush();
                    if v != 56 {
                        ok = false;
                    }
                }
                AT_PHNUM => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_phnum=");
                    l.dec(v);
                    l.nl();
                    l.flush();
                }
                AT_PHDR => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_phdr=");
                    l.hex(v);
                    l.nl();
                    l.flush();
                }
                AT_ENTRY => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_entry=");
                    l.hex(v);
                    l.nl();
                    l.flush();
                }
                AT_RANDOM => {
                    say(b"[argtest] at_random=yes\n");
                    saw_random = true;
                }
                AT_EXECFN => {
                    let mut l = LineBuf::new();
                    l.put(b"[argtest] at_execfn=");
                    l.cstr(v as *const u8);
                    l.nl();
                    l.flush();
                }
                _ => {}
            }
            aux = aux.add(2);
        }
        if !saw_random {
            ok = false;
        }
    }

    if ok {
        say(b"[argtest] PASSED\n");
        0
    } else {
        say(b"[argtest] FAILED\n");
        1
    }
}